use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use chrono::{DateTime, Utc};
use rand::{rng, Rng};
use rustls::{
    crypto::ring::sign::any_supported_type,
    pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer},
    sign::CertifiedKey,
};
use tarit_fleet::{AcmeJob, AcmeJobState, CertRecord, FleetError, PostgresFleet};
use tokio::{
    sync::{oneshot, watch},
    task::JoinHandle,
};
use x509_parser::{parse_x509_certificate, pem::parse_x509_pem};

use crate::config::AcmeConfig;

use super::{
    crypto::{open, seal, CryptoError, SealedSecret},
    dns::DnsProvider,
    order::{self, OrderCtx, OrderError},
    resolver::{CertResolver, CertStore},
};

const BACKOFF_BASE: Duration = Duration::from_secs(30);
const BACKOFF_CAP: Duration = Duration::from_secs(3_600);

pub struct AcmeWorker {
    fleet: Arc<PostgresFleet>,
    resolver: Arc<CertResolver>,
    dns: Arc<dyn DnsProvider>,
    acme: AcmeConfig,
    database_url: String,
    holder: String,
    reconcile_interval: Duration,
    lease: Duration,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagerError {
    #[error("fleet operation failed")]
    Fleet(#[source] FleetError),
    #[error("ACME order failed")]
    Order(#[source] OrderError),
    #[error("certificate secret decryption failed")]
    Crypto(#[source] CryptoError),
    #[error("certificate or private-key PEM could not be parsed")]
    Pem,
    #[error("certificate private key could not be used")]
    PrivateKey(#[source] rustls::Error),
    #[error("sealed certificate secret has an invalid nonce")]
    InvalidNonce,
}

impl From<FleetError> for ManagerError {
    fn from(error: FleetError) -> Self {
        Self::Fleet(error)
    }
}

impl From<OrderError> for ManagerError {
    fn from(error: OrderError) -> Self {
        Self::Order(error)
    }
}

impl From<CryptoError> for ManagerError {
    fn from(error: CryptoError) -> Self {
        Self::Crypto(error)
    }
}

impl AcmeWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        fleet: Arc<PostgresFleet>,
        resolver: Arc<CertResolver>,
        dns: Arc<dyn DnsProvider>,
        acme: AcmeConfig,
        database_url: String,
        holder: String,
        reconcile_interval: Duration,
        lease: Duration,
    ) -> Self {
        Self {
            fleet,
            resolver,
            dns,
            acme,
            database_url,
            holder,
            reconcile_interval,
            lease,
        }
    }

    pub async fn run(self, mut shutdown: watch::Receiver<Option<&'static str>>) {
        if shutdown.borrow().is_some() {
            return;
        }
        if let Err(error) = self.refresh_cache_once().await {
            tracing::warn!(%error, "failed to refresh ACME certificate cache at startup");
        }

        let mut listener = self.open_listener().await;
        let mut interval = tokio::time::interval(self.reconcile_interval);

        loop {
            tokio::select! {
                _ = shutdown.changed() => break,
                _ = interval.tick() => {
                    if listener.is_none() {
                        listener = self.open_listener().await;
                    }
                    if let Err(error) = self.reconcile(shutdown.clone()).await {
                        tracing::warn!(%error, "ACME reconciliation failed");
                    }
                }
                notification = async {
                    match &mut listener {
                        Some(listener) => listener.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    match notification {
                        Some(()) => {
                            if let Err(error) = self.reconcile(shutdown.clone()).await {
                                tracing::warn!(%error, "ACME reconciliation after certificate refresh failed");
                            }
                        }
                        None => listener = None,
                    }
                }
            }
        }
    }

    pub async fn refresh_cache_once(&self) -> Result<(), ManagerError> {
        let Some(certificate) = self.fleet.get_certificate(&self.acme.identifier).await? else {
            return Ok(());
        };
        let sealed = sealed_secret(&certificate.key_nonce, certificate.key_sealed)?;
        let key_pem = open(&self.acme.kek, &sealed)?;
        let certified_key = certified_key_from_pem(&certificate.cert_pem, &key_pem)?;

        self.resolver.install(CertStore::from_wildcard(
            base_domain(&self.acme.identifier),
            certified_key,
        ));
        Ok(())
    }

    async fn open_listener(&self) -> Option<tarit_fleet::CertRefreshListener> {
        match PostgresFleet::cert_refresh_listener(&self.database_url).await {
            Ok(listener) => Some(listener),
            Err(error) => {
                tracing::warn!(
                    error = %ManagerError::from(error),
                    "failed to establish ACME certificate refresh listener"
                );
                None
            }
        }
    }

    async fn reconcile(
        &self,
        shutdown: watch::Receiver<Option<&'static str>>,
    ) -> Result<(), ManagerError> {
        if shutdown.borrow().is_some() {
            return Ok(());
        }
        if let Err(error) = self.refresh_cache_once().await {
            tracing::warn!(%error, "failed to refresh ACME certificate cache");
        }

        let certificate = self.fleet.get_certificate(&self.acme.identifier).await?;
        let needs_renew = match certificate {
            Some(certificate) => {
                let issued = certificate_issued_at(&certificate.cert_pem)?;
                Utc::now() >= renew_at(certificate.not_after, issued, None)
            }
            None => true,
        };
        if !needs_renew || shutdown.borrow().is_some() {
            return Ok(());
        }

        let Some(job) = self
            .fleet
            .claim_acme_job(&self.acme.identifier, &self.holder, self.lease)
            .await?
        else {
            return Ok(());
        };

        self.run_claimed_job(job, shutdown).await
    }

    async fn run_claimed_job(
        &self,
        job: AcmeJob,
        mut shutdown: watch::Receiver<Option<&'static str>>,
    ) -> Result<(), ManagerError> {
        let (stop_renewal, renewal_shutdown) = watch::channel(false);
        let (lease_lost_sender, mut lease_lost) = oneshot::channel();
        let lease_held = Arc::new(AtomicBool::new(true));
        let renewal_task = spawn_lease_renewal(LeaseRenewal {
            fleet: Arc::clone(&self.fleet),
            job_id: job.id,
            fence: job.fence,
            holder: self.holder.clone(),
            lease: self.lease,
            shutdown: renewal_shutdown,
            lease_lost: lease_lost_sender,
            lease_held: Arc::clone(&lease_held),
        });

        let issuance = async {
            let stored = self
                .fleet
                .get_acme_account(&self.acme.directory_url)
                .await?
                .map(|account| sealed_secret(&account.nonce, account.sealed))
                .transpose()?;
            let (account, new_secret) = order::ensure_account(
                &self.acme.directory_url,
                &self.acme.contact,
                stored,
                &self.acme.kek,
            )
            .await?;
            if let Some(new_secret) = new_secret {
                self.fleet
                    .put_acme_account(
                        &self.acme.directory_url,
                        &new_secret.ciphertext,
                        &new_secret.nonce,
                    )
                    .await?;
            }
            order::run_dns01_order(
                OrderCtx {
                    account: &account,
                    dns: Arc::clone(&self.dns),
                    identifier: &self.acme.identifier,
                },
                job.order_url.clone(),
            )
            .await
            .map_err(ManagerError::from)
        };
        tokio::pin!(issuance);

        let issuance = tokio::select! {
            result = &mut issuance => Some(result),
            _ = &mut lease_lost => None,
            _ = shutdown.changed() => None,
        };

        let Some(issuance) = issuance else {
            renewal_task.abort();
            let _ = renewal_task.await;
            return Ok(());
        };
        if !lease_held.load(Ordering::Acquire) {
            stop_lease_renewal(stop_renewal, renewal_task).await;
            return Ok(());
        }

        match issuance {
            Ok((issued, _order_url)) => {
                let sealed = seal(&self.acme.kek, issued.key_pem.as_bytes());
                let certificate = CertRecord {
                    domain: self.acme.identifier.clone(),
                    cert_pem: issued.cert_pem,
                    key_sealed: sealed.ciphertext,
                    key_nonce: sealed.nonce.to_vec(),
                    generation: 0,
                    not_after: issued.not_after,
                    sans: issued.sans,
                };
                match self
                    .fleet
                    .publish_certificate(&certificate, &self.acme.identifier, job.fence)
                    .await
                {
                    Ok(true) => {
                        if let Err(error) = self.fleet.notify_cert_refresh().await {
                            tracing::warn!(
                                error = %ManagerError::from(error),
                                "failed to notify certificate refresh listeners"
                            );
                        }
                        if let Err(error) = self.refresh_cache_once().await {
                            tracing::warn!(%error, "failed to install issued ACME certificate");
                        }
                    }
                    Ok(false) => {}
                    Err(error) => self.record_failure(&job, ManagerError::from(error)).await,
                }
            }
            Err(error) => self.record_failure(&job, error).await,
        }

        stop_lease_renewal(stop_renewal, renewal_task).await;
        Ok(())
    }

    async fn record_failure(&self, job: &AcmeJob, error: ManagerError) {
        let mut updated = job.clone();
        updated.state = AcmeJobState::Failed;
        updated.order_url = None;
        updated.attempt = updated.attempt.saturating_add(1);
        let attempt = u32::try_from(job.attempt).unwrap_or(u32::MAX);
        let retry_seconds = i64::try_from(backoff_after(attempt).as_secs()).unwrap_or(3_600);
        updated.next_attempt_at = Some(Utc::now() + chrono::Duration::seconds(retry_seconds));
        updated.last_error = Some(error.to_string());
        updated.updated_at = Utc::now();

        match self.fleet.save_acme_job(&updated, job.fence).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!("ACME job failure could not be saved because its lease is stale")
            }
            Err(_) => tracing::warn!("failed to save ACME job failure"),
        }
        tracing::warn!(%error, "ACME certificate order failed");
    }
}

pub fn renew_at(
    not_after: DateTime<Utc>,
    issued: DateTime<Utc>,
    ari: Option<(DateTime<Utc>, DateTime<Utc>)>,
) -> DateTime<Utc> {
    let now = Utc::now();
    if let Some((start, end)) = ari {
        if end <= now || start > end {
            return now;
        }
        let start = start.max(now);
        let span_seconds = end.signed_duration_since(start).num_seconds();
        let offset = rng().random_range(0..=span_seconds);
        return start + chrono::Duration::seconds(offset);
    }

    let lifetime_seconds = not_after.signed_duration_since(issued).num_seconds();
    issued + chrono::Duration::seconds(lifetime_seconds.saturating_mul(2) / 3)
}

pub fn backoff_after(attempt: u32) -> Duration {
    let multiplier = 1_u64.checked_shl(attempt).unwrap_or(u64::MAX);
    let seconds = BACKOFF_BASE
        .as_secs()
        .saturating_mul(multiplier)
        .min(BACKOFF_CAP.as_secs());
    let jitter = (seconds / 4).min(BACKOFF_CAP.as_secs().saturating_sub(seconds));

    Duration::from_secs(seconds + rng().random_range(0..=jitter))
}

fn base_domain(identifier: &str) -> &str {
    identifier.strip_prefix("*.").unwrap_or(identifier)
}

fn sealed_secret(nonce: &[u8], ciphertext: Vec<u8>) -> Result<SealedSecret, ManagerError> {
    let nonce = nonce.try_into().map_err(|_| ManagerError::InvalidNonce)?;
    Ok(SealedSecret { nonce, ciphertext })
}

fn certified_key_from_pem(
    cert_pem: &str,
    key_pem: &[u8],
) -> Result<Arc<CertifiedKey>, ManagerError> {
    let certificates = CertificateDer::pem_slice_iter(cert_pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| ManagerError::Pem)?;
    if certificates.is_empty() {
        return Err(ManagerError::Pem);
    }

    let key = PrivateKeyDer::from_pem_slice(key_pem).map_err(|_| ManagerError::Pem)?;
    let signing_key = any_supported_type(&key).map_err(ManagerError::PrivateKey)?;
    Ok(Arc::new(CertifiedKey::new(certificates, signing_key)))
}

fn certificate_issued_at(cert_pem: &str) -> Result<DateTime<Utc>, ManagerError> {
    let (_, pem) = parse_x509_pem(cert_pem.as_bytes()).map_err(|_| ManagerError::Pem)?;
    let (_, certificate) = parse_x509_certificate(&pem.contents).map_err(|_| ManagerError::Pem)?;
    DateTime::from_timestamp(certificate.validity().not_before.timestamp(), 0)
        .ok_or(ManagerError::Pem)
}

struct LeaseRenewal {
    fleet: Arc<PostgresFleet>,
    job_id: uuid::Uuid,
    fence: i64,
    holder: String,
    lease: Duration,
    shutdown: watch::Receiver<bool>,
    lease_lost: oneshot::Sender<()>,
    lease_held: Arc<AtomicBool>,
}

fn spawn_lease_renewal(renewal: LeaseRenewal) -> JoinHandle<()> {
    tokio::spawn(async move {
        let LeaseRenewal {
            fleet,
            job_id,
            fence,
            holder,
            lease,
            mut shutdown,
            lease_lost,
            lease_held,
        } = renewal;
        let renewal_interval = lease
            .checked_div(3)
            .filter(|interval| !interval.is_zero())
            .unwrap_or(Duration::from_millis(1));
        let mut ticker = tokio::time::interval(renewal_interval);
        let mut lease_lost = Some(lease_lost);

        loop {
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = ticker.tick() => match fleet.renew_acme_lease(job_id, fence, &holder, lease).await {
                    Ok(true) => {}
                    Ok(false) => {
                        lease_held.store(false, Ordering::Release);
                        if let Some(sender) = lease_lost.take() {
                            let _ = sender.send(());
                        }
                        return;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "failed to renew ACME job lease; will retry");
                    }
                },
            }
        }
    })
}

async fn stop_lease_renewal(shutdown: watch::Sender<bool>, task: JoinHandle<()>) {
    let _ = shutdown.send(true);
    let _ = task.await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use chrono::{Duration as ChronoDuration, TimeZone, Utc};

    use super::{backoff_after, base_domain, renew_at};

    #[test]
    fn renew_falls_back_to_one_third_lifetime_without_ari() {
        let issued = Utc.timestamp_opt(1_700_000_000, 0).unwrap();
        let not_after = issued + ChronoDuration::days(90);

        let renew = renew_at(not_after, issued, None);

        assert!(
            (renew - (issued + ChronoDuration::days(60)))
                .num_hours()
                .abs()
                <= 24
        );
    }

    #[test]
    fn renews_immediately_when_ari_window_is_in_the_past() {
        let now = Utc::now();

        let renew = renew_at(
            now + ChronoDuration::days(30),
            now - ChronoDuration::days(60),
            Some((now - ChronoDuration::days(2), now - ChronoDuration::days(1))),
        );

        assert!(renew >= now - ChronoDuration::seconds(1));
        assert!(renew <= Utc::now() + ChronoDuration::seconds(1));
    }

    #[test]
    fn renews_within_a_future_ari_window() {
        let now = Utc::now();
        let start = now + ChronoDuration::days(2);
        let end = now + ChronoDuration::days(3);

        let renew = renew_at(now + ChronoDuration::days(90), now, Some((start, end)));

        assert!(renew >= start);
        assert!(renew <= end);
    }

    #[test]
    fn backoff_is_bounded_and_grows() {
        assert!(backoff_after(0) < backoff_after(3));
        assert!(backoff_after(30) <= Duration::from_secs(3_600));
    }

    #[test]
    fn base_domain_strips_the_wildcard_prefix() {
        assert_eq!(base_domain("*.a.b"), "a.b");
    }
}
