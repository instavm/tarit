use super::*;

// A transaction holds the VM row until both claimants are queued behind it.
// PostgreSQL can queue the second claimant behind the first one.
async fn wait_for_claimants(tx: &tokio_postgres::Transaction<'_>) -> Result<(), FleetError> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let row = tx
            .query_one(
                "WITH RECURSIVE waiters(pid) AS (
                     SELECT pg_backend_pid()
                     UNION
                     SELECT l.pid FROM pg_locks l JOIN waiters w
                         ON w.pid = ANY(pg_blocking_pids(l.pid)) WHERE NOT l.granted
                 ) SELECT COUNT(*) - 1 FROM waiters",
                &[],
            )
            .await?;
        if row.get::<_, i64>(0) == 2 {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(FleetError::Config(
                "two wake claims did not queue on the VM".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn simultaneous_wake_claims_transfer_ownership_once() -> Result<(), FleetError> {
    let Some(url) = postgres_test_database_url() else {
        return Ok(());
    };
    let _database_guard = POSTGRES_TEST_LOCK.lock().await;
    let fleet = PostgresFleet::connect_with_replication_policy(&url, 1, 1).await?;
    let id = Uuid::new_v4();
    let owner = format!("tenant-{id}");
    let hosts = ["source", "left", "right"].map(|name| format!("{name}-{id}"));
    let [source, left, right] = &hosts;
    let sessions = [Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4()];
    for (host, session) in hosts.iter().zip(sessions) {
        fleet
            .upsert_host(&host_record_from_capacity(
                host, session, None, None, 0, 8, 8192,
            ))
            .await?;
    }
    let mut vm = test_vm(id, source, &owner);
    vm.status = VmStatus::Hibernated;
    let generation = fleet.upsert_vm(&vm).await?;
    let mut artifact = test_artifact(&owner, left, id);
    artifact.source_vm_id = Some(id);
    fleet.insert_artifact(&artifact).await?;
    let now = vm.created_at.to_microsecond_precision();
    let binding = FleetHibernationRecord {
        vm_id: id,
        owner_key: owner.clone(),
        artifact_id: artifact.artifact_id,
        egress_policy: EgressPolicyRecord {
            vm_id: id,
            owner_key: owner.clone(),
            revision: 1,
            allowlist: vec![],
            allow_existing: false,
            created_at: now,
            updated_at: now,
        },
        created_at: now,
        updated_at: now,
    };
    fleet.upsert_hibernation(&binding).await?;
    let mut client = fleet.pool.get().await?;
    assert!(matches!(
        fleet
            .claim_hibernated_vm(&owner, id, left, sessions[1])
            .await,
        Err(FleetError::Conflict(_))
    ));
    client.execute(
        "UPDATE fleet_hosts SET last_heartbeat = statement_timestamp() - INTERVAL '1 minute' WHERE host_id = $1",
        &[source],
    ).await?;
    let lock = client.transaction().await?;
    lock.query_one("SELECT id FROM fleet_vms WHERE id = $1 FOR UPDATE", &[&id])
        .await?;
    let release = async {
        let queued = wait_for_claimants(&lock).await;
        lock.commit().await?;
        queued
    };
    let (queued, left_result, right_result) = tokio::join!(
        release,
        fleet.claim_hibernated_vm(&owner, id, left, sessions[1]),
        fleet.claim_hibernated_vm(&owner, id, right, sessions[2])
    );
    queued?;
    let (winner, winning_host, session) = match (left_result, right_result) {
        (Ok(vm), Err(FleetError::Conflict(_))) => (vm, left, sessions[1]),
        (Err(FleetError::Conflict(_)), Ok(vm)) => (vm, right, sessions[2]),
        results => panic!("expected one accepted claim and one conflict, got {results:?}"),
    };
    assert_eq!(&winner.host_id, winning_host);
    assert_eq!(winner.status, VmStatus::Hibernated);
    assert_eq!(winner.revision, vm.revision + 1);
    assert!(matches!(
        fleet.upsert_vm(&vm).await,
        Err(FleetError::Conflict(_))
    ));
    assert!(matches!(
        fleet.delete_vm(&vm).await,
        Err(FleetError::Conflict(_))
    ));
    assert_eq!(fleet.get_vm(id).await?, winner);
    assert_eq!(
        fleet
            .claim_hibernated_vm(&owner, id, winning_host, session)
            .await?,
        winner
    );
    assert_eq!(fleet.upsert_vm(&winner).await?, generation + 1);
    assert_eq!(fleet.get_hibernation(&owner, id).await?, binding);
    assert_eq!(
        fleet
            .get_artifact(&owner, artifact.artifact_id)
            .await?
            .reference_count,
        1
    );
    fleet.delete_vm(&winner).await?;
    assert!(matches!(
        fleet.get_hibernation(&owner, id).await,
        Err(FleetError::NotFound)
    ));
    assert!(matches!(
        fleet.get_artifact(&owner, artifact.artifact_id).await,
        Err(FleetError::NotFound)
    ));
    client
        .execute(
            "DELETE FROM fleet_hosts WHERE host_id = ANY($1)",
            &[&hosts.as_slice()],
        )
        .await?;
    Ok(())
}
