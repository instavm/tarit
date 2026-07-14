use std::{collections::HashMap, net::IpAddr, sync::Arc, time::Duration};

use async_trait::async_trait;
use hickory_resolver::{
    config::{NameServerConfigGroup, ResolverConfig, ResolverOpts},
    TokioAsyncResolver,
};
use reqwest::StatusCode;
use serde::Deserialize;

const CLOUDFLARE_API_BASE: &str = "https://api.cloudflare.com/client/v4";
const PROPAGATION_ATTEMPTS: usize = 40;
const PROPAGATION_INTERVAL: Duration = Duration::from_secs(3);
const MAX_AUTHORITATIVE_NS_LOOKUPS: usize = 16;

fn route53_txt_value(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn merge_route53_txt_values(existing: &[String], value: &str) -> Vec<String> {
    let mut merged = Vec::with_capacity(existing.len() + 1);
    for candidate in existing
        .iter()
        .chain(std::iter::once(&route53_txt_value(value)))
    {
        if !merged.contains(candidate) {
            merged.push(candidate.clone());
        }
    }
    merged
}

fn remove_route53_txt_value(existing: &[String], value: &str) -> Option<Vec<String>> {
    let value = route53_txt_value(value);
    let reduced: Vec<_> = existing
        .iter()
        .filter(|candidate| *candidate != &value)
        .cloned()
        .collect();
    (!reduced.is_empty()).then_some(reduced)
}

fn route53_change_batch(
    fqdn: &str,
    values: &[String],
    action: aws_sdk_route53::types::ChangeAction,
    ttl: i64,
) -> aws_sdk_route53::types::ChangeBatch {
    let record_set = aws_sdk_route53::types::ResourceRecordSet::builder()
        .name(fqdn)
        .r#type(aws_sdk_route53::types::RrType::Txt)
        .ttl(ttl)
        .set_resource_records(Some(
            values
                .iter()
                .map(|value| {
                    aws_sdk_route53::types::ResourceRecord::builder()
                        .value(value)
                        .build()
                        .expect("TXT record values are required")
                })
                .collect(),
        ))
        .build()
        .expect("TXT record set requires name and type");
    let change = aws_sdk_route53::types::Change::builder()
        .action(action)
        .resource_record_set(record_set)
        .build()
        .expect("Route53 change requires a record set");

    aws_sdk_route53::types::ChangeBatch::builder()
        .changes(change)
        .build()
        .expect("Route53 change batch requires a change")
}

fn route53_change_id(change_id: &str) -> &str {
    change_id.strip_prefix("/change/").unwrap_or(change_id)
}

fn route53_error<E>(error: E) -> DnsError
where
    E: std::error::Error + Send + Sync + 'static,
{
    DnsError::Route53(Box::new(error))
}

#[derive(Clone, Debug, PartialEq)]
pub struct TxtHandle {
    pub record_id: String,
}

#[derive(Debug, thiserror::Error)]
pub enum DnsError {
    #[error("Cloudflare request failed")]
    Request(#[source] reqwest::Error),
    #[error("Cloudflare returned HTTP status {0}")]
    HttpStatus(StatusCode),
    #[error("Cloudflare returned an unsuccessful response")]
    Cloudflare,
    #[error("Cloudflare response did not include a DNS record ID")]
    MissingRecordId,
    #[error("Route53 request failed")]
    Route53(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("Route53 response did not include a change ID")]
    MissingChangeId,
    #[error("Route53 change was not found")]
    Route53ChangeNotFound,
    #[error("DNS lookup failed")]
    Lookup(#[source] hickory_resolver::error::ResolveError),
    #[error("DNS propagation timed out for {fqdn}")]
    PropagationTimeout { fqdn: String },
}

#[async_trait]
pub trait DnsProvider: Send + Sync {
    async fn upsert_txt(&self, fqdn: &str, value: &str) -> Result<TxtHandle, DnsError>;
    async fn delete_txt(&self, handle: &TxtHandle) -> Result<(), DnsError>;
    async fn await_propagation(&self, fqdn: &str, value: &str) -> Result<(), DnsError>;
}

#[async_trait]
pub trait TxtResolver: Send + Sync {
    async fn lookup_txt(&self, fqdn: &str) -> Result<Vec<String>, DnsError>;
}

struct HickoryTxtResolver;

impl HickoryTxtResolver {
    fn bootstrap_resolver() -> TokioAsyncResolver {
        TokioAsyncResolver::tokio_from_system_conf().unwrap_or_else(|_| {
            TokioAsyncResolver::tokio(ResolverConfig::default(), ResolverOpts::default())
        })
    }

    async fn authoritative_resolver(
        bootstrap: &TokioAsyncResolver,
        fqdn: &str,
    ) -> Option<TokioAsyncResolver> {
        let mut candidate = fqdn.trim_end_matches('.');

        for _ in 0..MAX_AUTHORITATIVE_NS_LOOKUPS {
            if candidate.is_empty() {
                break;
            }

            if let Ok(nameservers) = bootstrap.ns_lookup(candidate).await {
                let nameservers: Vec<String> =
                    nameservers.iter().map(ToString::to_string).collect();
                if !nameservers.is_empty() {
                    let mut addresses = Vec::new();
                    for nameserver in nameservers {
                        if let Ok(lookup) = bootstrap.lookup_ip(nameserver).await {
                            addresses.extend(lookup.iter());
                        }
                    }
                    addresses.sort_unstable();
                    addresses.dedup();

                    if !addresses.is_empty() {
                        let (config, options) = authoritative_resolver_configuration(&addresses);
                        return Some(TokioAsyncResolver::tokio(config, options));
                    }

                    return None;
                }
            }

            candidate = match candidate.split_once('.') {
                Some((_, parent)) => parent,
                None => break,
            };
        }

        None
    }
}

fn authoritative_resolver_configuration(ips: &[IpAddr]) -> (ResolverConfig, ResolverOpts) {
    let config = ResolverConfig::from_parts(
        None,
        vec![],
        NameServerConfigGroup::from_ips_clear(ips, 53, false),
    );
    let mut options = ResolverOpts::default();
    options.cache_size = 0;
    options.use_hosts_file = false;
    options.recursion_desired = false;

    (config, options)
}

#[async_trait]
impl TxtResolver for HickoryTxtResolver {
    async fn lookup_txt(&self, fqdn: &str) -> Result<Vec<String>, DnsError> {
        let bootstrap = Self::bootstrap_resolver();
        // Authoritative queries avoid recursive negative caches hiding a just-created TXT record.
        let lookup = match Self::authoritative_resolver(&bootstrap, fqdn).await {
            Some(resolver) => match resolver.txt_lookup(fqdn).await {
                Ok(lookup) => Ok(lookup),
                Err(_) => bootstrap.txt_lookup(fqdn).await,
            },
            None => bootstrap.txt_lookup(fqdn).await,
        }
        .map_err(DnsError::Lookup)?;

        Ok(lookup
            .iter()
            .map(|record| {
                record
                    .txt_data()
                    .iter()
                    .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
                    .collect()
            })
            .collect())
    }
}

async fn await_txt_visibility(
    resolver: &dyn TxtResolver,
    propagation_attempts: usize,
    propagation_interval: Duration,
    fqdn: &str,
    value: &str,
) -> Result<(), DnsError> {
    for attempt in 0..propagation_attempts {
        if let Ok(values) = resolver.lookup_txt(fqdn).await {
            if values.iter().any(|candidate| candidate == value) {
                return Ok(());
            }
        }

        if attempt + 1 < propagation_attempts {
            tokio::time::sleep(propagation_interval).await;
        }
    }

    Err(DnsError::PropagationTimeout {
        fqdn: fqdn.to_owned(),
    })
}

pub struct CloudflareDns {
    client: reqwest::Client,
    api_base: String,
    token: String,
    zone_id: String,
    resolver: Arc<dyn TxtResolver>,
    propagation_attempts: usize,
    propagation_interval: Duration,
}

impl CloudflareDns {
    pub fn new(token: String, zone_id: String) -> Self {
        Self::with_base(token, zone_id, CLOUDFLARE_API_BASE.to_owned())
    }

    pub fn with_base(token: String, zone_id: String, api_base: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_base: api_base.trim_end_matches('/').to_owned(),
            token,
            zone_id,
            resolver: Arc::new(HickoryTxtResolver),
            propagation_attempts: PROPAGATION_ATTEMPTS,
            propagation_interval: PROPAGATION_INTERVAL,
        }
    }

    fn records_url(&self) -> String {
        format!("{}/zones/{}/dns_records", self.api_base, self.zone_id)
    }
}

#[derive(Deserialize)]
struct CloudflareResponse<T> {
    success: bool,
    result: Option<T>,
}

#[derive(Deserialize)]
struct CloudflareRecord {
    id: String,
}

#[async_trait]
impl DnsProvider for CloudflareDns {
    async fn upsert_txt(&self, fqdn: &str, value: &str) -> Result<TxtHandle, DnsError> {
        let response = self
            .client
            .post(self.records_url())
            .bearer_auth(&self.token)
            .json(&serde_json::json!({
                "type": "TXT",
                "name": fqdn,
                "content": value,
                "ttl": 60,
            }))
            .send()
            .await
            .map_err(DnsError::Request)?;

        if !response.status().is_success() {
            return Err(DnsError::HttpStatus(response.status()));
        }

        let response = response
            .json::<CloudflareResponse<CloudflareRecord>>()
            .await
            .map_err(DnsError::Request)?;
        if !response.success {
            return Err(DnsError::Cloudflare);
        }

        let record = response.result.ok_or(DnsError::MissingRecordId)?;
        Ok(TxtHandle {
            record_id: record.id,
        })
    }

    async fn delete_txt(&self, handle: &TxtHandle) -> Result<(), DnsError> {
        let response = self
            .client
            .delete(format!("{}/{}", self.records_url(), handle.record_id))
            .bearer_auth(&self.token)
            .send()
            .await
            .map_err(DnsError::Request)?;

        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        if !response.status().is_success() {
            return Err(DnsError::HttpStatus(response.status()));
        }

        let response = response
            .json::<CloudflareResponse<serde_json::Value>>()
            .await
            .map_err(DnsError::Request)?;
        if response.success {
            Ok(())
        } else {
            Err(DnsError::Cloudflare)
        }
    }

    async fn await_propagation(&self, fqdn: &str, value: &str) -> Result<(), DnsError> {
        await_txt_visibility(
            self.resolver.as_ref(),
            self.propagation_attempts,
            self.propagation_interval,
            fqdn,
            value,
        )
        .await
    }
}

pub struct Route53Dns {
    client: aws_sdk_route53::Client,
    zone_id: String,
    active_records: Arc<tokio::sync::Mutex<HashMap<String, (String, String)>>>,
    resolver: Arc<dyn TxtResolver>,
    propagation_attempts: usize,
    propagation_interval: Duration,
}

impl Route53Dns {
    pub async fn from_env(zone_id: String) -> Result<Self, DnsError> {
        let config = aws_config::load_defaults(aws_config::BehaviorVersion::latest()).await;
        Ok(Self::from_client(
            aws_sdk_route53::Client::new(&config),
            zone_id,
        ))
    }

    pub fn from_client(client: aws_sdk_route53::Client, zone_id: String) -> Self {
        Self {
            client,
            zone_id,
            active_records: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            resolver: Arc::new(HickoryTxtResolver),
            propagation_attempts: PROPAGATION_ATTEMPTS,
            propagation_interval: PROPAGATION_INTERVAL,
        }
    }

    async fn current_txt_record(&self, fqdn: &str) -> Result<Option<(Vec<String>, i64)>, DnsError> {
        let response = self
            .client
            .list_resource_record_sets()
            .hosted_zone_id(&self.zone_id)
            .start_record_name(fqdn)
            .start_record_type(aws_sdk_route53::types::RrType::Txt)
            .max_items(1)
            .send()
            .await
            .map_err(route53_error)?;

        Ok(response
            .resource_record_sets()
            .iter()
            .find(|record_set| {
                record_set.r#type() == &aws_sdk_route53::types::RrType::Txt
                    && record_set
                        .name()
                        .trim_end_matches('.')
                        .eq_ignore_ascii_case(fqdn.trim_end_matches('.'))
            })
            .map(|record_set| {
                (
                    record_set
                        .resource_records()
                        .iter()
                        .map(|record| record.value().to_owned())
                        .collect(),
                    record_set.ttl().unwrap_or(60),
                )
            }))
    }

    async fn change_txt_record(
        &self,
        fqdn: &str,
        values: &[String],
        action: aws_sdk_route53::types::ChangeAction,
        ttl: i64,
    ) -> Result<String, DnsError> {
        let response = self
            .client
            .change_resource_record_sets()
            .hosted_zone_id(&self.zone_id)
            .change_batch(route53_change_batch(fqdn, values, action, ttl))
            .send()
            .await
            .map_err(route53_error)?;

        response
            .change_info()
            .map(|change| route53_change_id(change.id()).to_owned())
            .ok_or(DnsError::MissingChangeId)
    }

    async fn change_id_for(&self, fqdn: &str, value: &str) -> Option<String> {
        self.active_records.lock().await.iter().find_map(
            |(change_id, (record_name, record_value))| {
                (record_name == fqdn && record_value == value).then(|| change_id.clone())
            },
        )
    }
}

#[async_trait]
impl DnsProvider for Route53Dns {
    async fn upsert_txt(&self, fqdn: &str, value: &str) -> Result<TxtHandle, DnsError> {
        let mut active_records = self.active_records.lock().await;
        let (current_values, ttl) = self
            .current_txt_record(fqdn)
            .await?
            .unwrap_or((Vec::new(), 60));
        let values = merge_route53_txt_values(&current_values, value);
        let change_id = self
            .change_txt_record(
                fqdn,
                &values,
                aws_sdk_route53::types::ChangeAction::Upsert,
                ttl,
            )
            .await?;

        active_records.insert(change_id.clone(), (fqdn.to_owned(), value.to_owned()));
        Ok(TxtHandle {
            record_id: change_id,
        })
    }

    async fn delete_txt(&self, handle: &TxtHandle) -> Result<(), DnsError> {
        let mut active_records = self.active_records.lock().await;
        let Some((fqdn, value)) = active_records.get(&handle.record_id).cloned() else {
            return Ok(());
        };
        let Some((current_values, ttl)) = self.current_txt_record(&fqdn).await? else {
            active_records.remove(&handle.record_id);
            return Ok(());
        };
        let quoted_value = route53_txt_value(&value);
        if !current_values.contains(&quoted_value) {
            active_records.remove(&handle.record_id);
            return Ok(());
        }

        let (action, values) = match remove_route53_txt_value(&current_values, &value) {
            Some(values) => (aws_sdk_route53::types::ChangeAction::Upsert, values),
            None => (aws_sdk_route53::types::ChangeAction::Delete, current_values),
        };
        self.change_txt_record(&fqdn, &values, action, ttl).await?;

        active_records.remove(&handle.record_id);
        Ok(())
    }

    async fn await_propagation(&self, fqdn: &str, value: &str) -> Result<(), DnsError> {
        let change_id = self
            .change_id_for(fqdn, value)
            .await
            .ok_or(DnsError::Route53ChangeNotFound)?;

        for attempt in 0..self.propagation_attempts {
            let response = self
                .client
                .get_change()
                .id(&change_id)
                .send()
                .await
                .map_err(route53_error)?;
            if response.change_info().is_some_and(|change| {
                change.status() == &aws_sdk_route53::types::ChangeStatus::Insync
            }) {
                return await_txt_visibility(
                    self.resolver.as_ref(),
                    self.propagation_attempts,
                    self.propagation_interval,
                    fqdn,
                    value,
                )
                .await;
            }

            if attempt + 1 < self.propagation_attempts {
                tokio::time::sleep(self.propagation_interval).await;
            }
        }

        Err(DnsError::PropagationTimeout {
            fqdn: fqdn.to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        net::{IpAddr, Ipv4Addr},
        sync::{Arc, Mutex},
        time::Duration,
    };

    use async_trait::async_trait;
    use axum::{
        extract::State,
        http::{HeaderMap, Method, Uri},
        routing::{delete, post},
        Json, Router,
    };
    use serde_json::{json, Value};
    use tokio::net::TcpListener;

    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct CapturedRequest {
        method: Method,
        path: String,
        authorization: Option<String>,
        body: Value,
    }

    #[derive(Clone, Default)]
    struct MockCloudflareState {
        requests: Arc<Mutex<Vec<CapturedRequest>>>,
    }

    struct MockCloudflare {
        base_url: String,
        state: MockCloudflareState,
    }

    impl MockCloudflare {
        fn captured(&self) -> Vec<CapturedRequest> {
            self.state.requests.lock().unwrap().clone()
        }
    }

    async fn create_txt(
        State(state): State<MockCloudflareState>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
        Json(body): Json<Value>,
    ) -> Json<Value> {
        state.requests.lock().unwrap().push(CapturedRequest {
            method,
            path: uri.path().to_owned(),
            authorization: headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            body,
        });

        Json(json!({"success": true, "result": {"id": "rec123"}}))
    }

    async fn delete_txt(
        State(state): State<MockCloudflareState>,
        method: Method,
        uri: Uri,
        headers: HeaderMap,
    ) -> Json<Value> {
        state.requests.lock().unwrap().push(CapturedRequest {
            method,
            path: uri.path().to_owned(),
            authorization: headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned),
            body: Value::Null,
        });

        Json(json!({"success": true}))
    }

    async fn start_mock_cf() -> MockCloudflare {
        let state = MockCloudflareState::default();
        let app = Router::new()
            .route("/zones/{zone_id}/dns_records", post(create_txt))
            .route(
                "/zones/{zone_id}/dns_records/{record_id}",
                delete(delete_txt),
            )
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();

        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        MockCloudflare {
            base_url: format!("http://{address}"),
            state,
        }
    }

    #[tokio::test]
    async fn cloudflare_creates_and_deletes_txt_by_id() {
        let mock = start_mock_cf().await;
        let dns = CloudflareDns::with_base("tok".into(), "zone1".into(), mock.base_url.clone());

        let handle = dns
            .upsert_txt("_acme-challenge.shares.example.com", "abc")
            .await
            .unwrap();
        assert_eq!(handle.record_id, "rec123");

        dns.delete_txt(&handle).await.unwrap();

        let requests = mock.captured();
        assert!(requests.iter().any(|request| {
            request.method == Method::POST
                && request.path == "/zones/zone1/dns_records"
                && request.authorization.as_deref() == Some("Bearer tok")
                && request.body
                    == json!({
                        "type": "TXT",
                        "name": "_acme-challenge.shares.example.com",
                        "content": "abc",
                        "ttl": 60,
                    })
        }));
        assert!(requests.iter().any(|request| {
            request.method == Method::DELETE
                && request.path == "/zones/zone1/dns_records/rec123"
                && request.authorization.as_deref() == Some("Bearer tok")
        }));
    }

    struct FakeTxtResolver {
        values: Vec<String>,
    }

    #[async_trait]
    impl TxtResolver for FakeTxtResolver {
        async fn lookup_txt(&self, _fqdn: &str) -> Result<Vec<String>, DnsError> {
            Ok(self.values.clone())
        }
    }

    fn dns_with_resolver(
        resolver: Arc<dyn TxtResolver>,
        propagation_attempts: usize,
    ) -> CloudflareDns {
        CloudflareDns {
            client: reqwest::Client::new(),
            api_base: "http://127.0.0.1".to_owned(),
            token: "tok".to_owned(),
            zone_id: "zone1".to_owned(),
            resolver,
            propagation_attempts,
            propagation_interval: Duration::ZERO,
        }
    }

    #[tokio::test]
    async fn await_propagation_succeeds_with_injected_resolver() {
        let dns = dns_with_resolver(
            Arc::new(FakeTxtResolver {
                values: vec!["expected".to_owned()],
            }),
            1,
        );

        dns.await_propagation("_acme-challenge.shares.example.com", "expected")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn await_propagation_times_out_when_value_never_appears() {
        let dns = dns_with_resolver(Arc::new(FakeTxtResolver { values: vec![] }), 2);

        let error = dns
            .await_propagation("_acme-challenge.shares.example.com", "expected")
            .await
            .unwrap_err();

        assert!(matches!(error, DnsError::PropagationTimeout { .. }));
    }

    #[test]
    fn authoritative_resolver_configuration_disables_cache_and_hosts_file() {
        let (config, options) =
            authoritative_resolver_configuration(&[IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1))]);

        assert_eq!(options.cache_size, 0);
        assert!(!options.use_hosts_file);
        assert!(!options.recursion_desired);
        assert_eq!(config.name_servers().len(), 2);
        assert!(config
            .name_servers()
            .iter()
            .any(|server| server.protocol == hickory_resolver::config::Protocol::Udp));
        assert!(config
            .name_servers()
            .iter()
            .any(|server| server.protocol == hickory_resolver::config::Protocol::Tcp));
        assert!(config
            .name_servers()
            .iter()
            .all(|server| server.socket_addr.port() == 53));
    }

    #[test]
    fn route53_quotes_txt_values_for_rrsets() {
        assert_eq!(route53_txt_value("abc"), "\"abc\"");
        assert_eq!(route53_txt_value("a\"b\\c"), "\"a\\\"b\\\\c\"");
    }

    #[test]
    fn route53_merges_txt_values_additively() {
        let existing = vec![
            "\"old\"".to_owned(),
            "\"present\"".to_owned(),
            "\"old\"".to_owned(),
        ];

        assert_eq!(
            merge_route53_txt_values(&existing, "new"),
            vec![
                "\"old\"".to_owned(),
                "\"present\"".to_owned(),
                "\"new\"".to_owned(),
            ]
        );
        assert_eq!(
            merge_route53_txt_values(&existing, "present"),
            vec!["\"old\"".to_owned(), "\"present\"".to_owned()]
        );
    }

    #[test]
    fn route53_reduces_txt_values_for_deletion() {
        let existing = vec!["\"old\"".to_owned(), "\"new\"".to_owned()];

        assert_eq!(
            remove_route53_txt_value(&existing, "new"),
            Some(vec!["\"old\"".to_owned()])
        );
        assert_eq!(
            remove_route53_txt_value(&["\"new\"".to_owned()], "new"),
            None
        );
    }

    #[test]
    fn route53_builds_additive_upsert_change_batch() {
        let batch = route53_change_batch(
            "_acme-challenge.shares.example.com",
            &merge_route53_txt_values(&["\"old\"".to_owned()], "new"),
            aws_sdk_route53::types::ChangeAction::Upsert,
            60,
        );

        let change = &batch.changes()[0];
        assert_eq!(
            change.action(),
            &aws_sdk_route53::types::ChangeAction::Upsert
        );
        let record_set = change.resource_record_set().unwrap();
        assert_eq!(record_set.name(), "_acme-challenge.shares.example.com");
        assert_eq!(
            record_set
                .resource_records()
                .iter()
                .map(|record| record.value())
                .collect::<Vec<_>>(),
            vec!["\"old\"", "\"new\""]
        );
    }

    #[test]
    fn route53_strips_change_path_prefix() {
        assert_eq!(route53_change_id("/change/CHG1"), "CHG1");
        assert_eq!(route53_change_id("CHG1"), "CHG1");
    }
}
