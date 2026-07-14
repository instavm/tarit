use std::{net::IpAddr, sync::Arc, time::Duration};

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
        for attempt in 0..self.propagation_attempts {
            if let Ok(values) = self.resolver.lookup_txt(fqdn).await {
                if values.iter().any(|candidate| candidate == value) {
                    return Ok(());
                }
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
}
