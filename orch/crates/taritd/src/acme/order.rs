use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, Order, OrderStatus,
};
use rcgen::{CertificateParams, KeyPair};
use tokio::time::sleep;
use x509_parser::{extensions::GeneralName, parse_x509_certificate, pem::parse_x509_pem};

use super::{
    crypto::{self, SealedSecret},
    dns::{DnsError, DnsProvider, TxtHandle},
};

const ORDER_POLL_ATTEMPTS: usize = 60;
const ORDER_POLL_INTERVAL: Duration = Duration::from_secs(2);
const CERTIFICATE_FETCH_ATTEMPTS: usize = 5;

pub struct IssuedCert {
    pub cert_pem: String,
    pub key_pem: String,
    pub not_after: DateTime<Utc>,
    pub sans: Vec<String>,
}

pub struct OrderCtx<'a> {
    pub account: &'a Account,
    pub dns: Arc<dyn DnsProvider>,
    pub identifier: &'a str,
}

/// Removes the ACME challenge TXT record when dropped, so cleanup runs on every
/// exit path from an order, including early returns and future cancellation.
struct TxtGuard {
    dns: Arc<dyn DnsProvider>,
    handle: TxtHandle,
}

impl Drop for TxtGuard {
    fn drop(&mut self) {
        let dns = Arc::clone(&self.dns);
        let handle = self.handle.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                if let Err(error) = dns.delete_txt(&handle).await {
                    tracing::warn!(
                        %error,
                        record_id = %handle.record_id,
                        "failed to delete ACME challenge TXT record"
                    );
                }
            });
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum OrderError {
    #[error("ACME request failed")]
    Acme(#[source] instant_acme::Error),
    #[error("DNS operation failed")]
    Dns(#[source] DnsError),
    #[error("ACME account credential decryption failed")]
    Crypto(#[source] crypto::CryptoError),
    #[error("ACME account credential serialization failed")]
    Credentials(#[source] serde_json::Error),
    #[error("certificate key or signing request generation failed")]
    CertificateGeneration(#[source] rcgen::Error),
    #[error("certificate PEM could not be parsed")]
    CertificateParse,
    #[error("ACME order does not have an authorization")]
    MissingAuthorization,
    #[error("ACME authorization does not offer a DNS-01 challenge")]
    MissingDns01Challenge,
    #[error("ACME order entered unsupported state {status:?}")]
    UnexpectedOrderState { status: OrderStatus },
    #[error("ACME authorization entered unsupported state {status:?}")]
    UnexpectedAuthorizationState { status: AuthorizationStatus },
    #[error("ACME order timed out while waiting for {phase}")]
    Timeout { phase: &'static str },
    #[error("ACME did not provide a certificate chain")]
    MissingCertificate,
}

impl From<instant_acme::Error> for OrderError {
    fn from(error: instant_acme::Error) -> Self {
        Self::Acme(error)
    }
}

impl From<DnsError> for OrderError {
    fn from(error: DnsError) -> Self {
        Self::Dns(error)
    }
}

impl From<crypto::CryptoError> for OrderError {
    fn from(error: crypto::CryptoError) -> Self {
        Self::Crypto(error)
    }
}

pub fn challenge_fqdn(identifier: &str) -> String {
    format!(
        "_acme-challenge.{}",
        identifier.strip_prefix("*.").unwrap_or(identifier)
    )
}

pub fn generate_key_and_csr(identifier: &str) -> Result<(String, Vec<u8>), OrderError> {
    let params = CertificateParams::new(vec![identifier.to_owned()])
        .map_err(OrderError::CertificateGeneration)?;
    let key = KeyPair::generate().map_err(OrderError::CertificateGeneration)?;
    let csr = params
        .serialize_request(&key)
        .map_err(OrderError::CertificateGeneration)?;

    Ok((key.serialize_pem(), csr.der().to_vec()))
}

pub fn parse_not_after_and_sans(
    cert_pem: &str,
) -> Result<(DateTime<Utc>, Vec<String>), OrderError> {
    let (_, pem) = parse_x509_pem(cert_pem.as_bytes()).map_err(|_| OrderError::CertificateParse)?;
    let (_, certificate) =
        parse_x509_certificate(&pem.contents).map_err(|_| OrderError::CertificateParse)?;
    let not_after = DateTime::from_timestamp(certificate.validity().not_after.timestamp(), 0)
        .ok_or(OrderError::CertificateParse)?;
    let sans = certificate
        .subject_alternative_name()
        .map_err(|_| OrderError::CertificateParse)?
        .map(|san| {
            san.value
                .general_names
                .iter()
                .filter_map(|name| match name {
                    GeneralName::DNSName(name) => Some((*name).to_owned()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    Ok((not_after, sans))
}

pub async fn ensure_account(
    directory_url: &str,
    contact: &str,
    stored: Option<SealedSecret>,
    kek: &[u8; 32],
) -> Result<(Account, Option<SealedSecret>), OrderError> {
    if let Some(stored) = stored {
        let credentials: AccountCredentials = serde_json::from_slice(&crypto::open(kek, &stored)?)
            .map_err(OrderError::Credentials)?;
        let account = Account::builder()?.from_credentials(credentials).await?;
        return Ok((account, None));
    }

    let contact = if contact.starts_with("mailto:") {
        contact.to_owned()
    } else {
        format!("mailto:{contact}")
    };
    let contacts = [contact.as_str()];
    let (account, credentials) = Account::builder()?
        .create(
            &NewAccount {
                contact: &contacts,
                terms_of_service_agreed: true,
                only_return_existing: false,
            },
            directory_url.to_owned(),
            None,
        )
        .await?;
    let serialized = serde_json::to_vec(&credentials).map_err(OrderError::Credentials)?;

    Ok((account, Some(crypto::seal(kek, &serialized))))
}

/// Drives a single-shot DNS-01 ACME order for `ctx.identifier` to completion and
/// returns the issued certificate chain together with the locally generated key.
///
/// The order is single-shot: the private key is generated fresh for this run and
/// is returned only on success. If issuance fails after finalization the key is
/// discarded and the caller must start a new order rather than resume, which is
/// why resuming an order that is already past `Ready` (`Processing`/`Valid`) is
/// rejected. The challenge TXT record is removed on every exit path via `TxtGuard`.
pub async fn run_dns01_order(
    ctx: OrderCtx<'_>,
    resume_url: Option<String>,
) -> Result<(IssuedCert, String), OrderError> {
    let identifiers = [Identifier::Dns(ctx.identifier.to_owned())];
    let mut order = match resume_url {
        Some(url) => ctx.account.order(url).await?,
        None => ctx.account.new_order(&NewOrder::new(&identifiers)).await?,
    };
    let order_url = order.url().to_owned();
    let mut _txt_guard: Option<TxtGuard> = None;

    match order.state().status {
        OrderStatus::Pending => {
            let mut authorizations = order.authorizations();
            let mut authorization = match authorizations.next().await {
                Some(authorization) => authorization?,
                None => return Err(OrderError::MissingAuthorization),
            };

            match authorization.status {
                AuthorizationStatus::Pending => {
                    let mut challenge = authorization
                        .challenge(ChallengeType::Dns01)
                        .ok_or(OrderError::MissingDns01Challenge)?;
                    let fqdn = challenge_fqdn(ctx.identifier);
                    let txt_value = challenge.key_authorization().dns_value();
                    let handle = ctx.dns.upsert_txt(&fqdn, &txt_value).await?;
                    _txt_guard = Some(TxtGuard {
                        dns: Arc::clone(&ctx.dns),
                        handle,
                    });
                    ctx.dns.await_propagation(&fqdn, &txt_value).await?;
                    challenge.set_ready().await?;
                }
                AuthorizationStatus::Valid => {}
                status => return Err(OrderError::UnexpectedAuthorizationState { status }),
            }
        }
        OrderStatus::Ready => {}
        status => return Err(OrderError::UnexpectedOrderState { status }),
    }

    wait_for_order_status(&mut order, OrderStatus::Ready, "order readiness").await?;
    let (key_pem, csr_der) = generate_key_and_csr(ctx.identifier)?;
    order.finalize_csr(&csr_der).await?;
    wait_for_order_status(&mut order, OrderStatus::Valid, "certificate issuance").await?;
    let cert_pem = fetch_certificate(&mut order).await?;
    let (not_after, sans) = parse_not_after_and_sans(&cert_pem)?;

    Ok((
        IssuedCert {
            cert_pem,
            key_pem,
            not_after,
            sans,
        },
        order_url,
    ))
}

async fn fetch_certificate(order: &mut Order) -> Result<String, OrderError> {
    for attempt in 0..CERTIFICATE_FETCH_ATTEMPTS {
        match order.certificate().await {
            Ok(Some(cert_pem)) => return Ok(cert_pem),
            Ok(None) => {}
            Err(error) if attempt + 1 == CERTIFICATE_FETCH_ATTEMPTS => return Err(error.into()),
            Err(_) => {}
        }

        if attempt + 1 < CERTIFICATE_FETCH_ATTEMPTS {
            sleep(ORDER_POLL_INTERVAL).await;
        }
    }

    Err(OrderError::MissingCertificate)
}

async fn wait_for_order_status(
    order: &mut Order,
    target: OrderStatus,
    phase: &'static str,
) -> Result<(), OrderError> {
    for attempt in 0..ORDER_POLL_ATTEMPTS {
        let status = order.refresh().await?.status;
        if status == target {
            return Ok(());
        }

        match (target, status) {
            (OrderStatus::Ready, OrderStatus::Pending)
            | (OrderStatus::Valid, OrderStatus::Ready | OrderStatus::Processing) => {}
            (_, status) => return Err(OrderError::UnexpectedOrderState { status }),
        }

        if attempt + 1 < ORDER_POLL_ATTEMPTS {
            sleep(ORDER_POLL_INTERVAL).await;
        }
    }

    Err(OrderError::Timeout { phase })
}

#[cfg(test)]
mod tests {
    use chrono::{Datelike, Duration, Utc};
    use rcgen::{CertificateParams, KeyPair};
    use x509_parser::prelude::{FromDer, GeneralName, ParsedExtension, X509CertificationRequest};

    use super::*;

    #[test]
    fn challenge_fqdn_strips_wildcard() {
        assert_eq!(
            challenge_fqdn("*.shares.example.com"),
            "_acme-challenge.shares.example.com"
        );
    }

    #[test]
    fn csr_covers_the_wildcard_identifier() {
        let (key_pem, csr_der) = generate_key_and_csr("*.shares.example.com").unwrap();

        assert!(key_pem.contains("PRIVATE KEY"));

        let (_, csr) = X509CertificationRequest::from_der(&csr_der).unwrap();
        let sans: Vec<String> = csr
            .requested_extensions()
            .into_iter()
            .flatten()
            .filter_map(|extension| match extension {
                ParsedExtension::SubjectAlternativeName(san) => Some(
                    san.general_names
                        .iter()
                        .filter_map(|name| match name {
                            GeneralName::DNSName(name) => Some((*name).to_owned()),
                            _ => None,
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect();

        assert!(sans.contains(&"*.shares.example.com".to_owned()));
    }

    #[test]
    fn parse_not_after_reads_the_certificate() {
        let now = Utc::now();
        let mut params = CertificateParams::new(vec!["*.shares.example.com".to_owned()]).unwrap();
        params.not_before = rcgen::date_time_ymd(now.year(), now.month() as u8, now.day() as u8);
        let expires = now + Duration::days(10);
        params.not_after =
            rcgen::date_time_ymd(expires.year(), expires.month() as u8, expires.day() as u8);
        let key = KeyPair::generate().unwrap();
        let cert_pem = params.self_signed(&key).unwrap().pem();

        let (not_after, sans) = parse_not_after_and_sans(&cert_pem).unwrap();

        assert!(not_after > Utc::now());
        assert!(sans.contains(&"*.shares.example.com".to_owned()));
    }
}
