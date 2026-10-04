use anyhow::{bail, Context, Result};
use rustls::{
    pki_types::{CertificateDer, PrivateKeyDer},
    server::WebPkiClientVerifier,
    ClientConfig, RootCertStore, ServerConfig,
};
use rustls_pki_types::pem::PemObject;
use sha2::{Digest, Sha256};
use std::sync::Arc;

use crate::config::PeerTlsConfig;

fn ensure_crypto_provider() {
    // taritd also links russh, which enables aws-lc-rs. Select the reviewed
    // rustls ring provider explicitly rather than allowing feature unification
    // to make provider choice ambiguous or startup-order dependent.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn certificate_chain(path: &std::path::Path) -> Result<Vec<CertificateDer<'static>>> {
    let pem =
        std::fs::read(path).with_context(|| format!("read certificate {}", path.display()))?;
    let certificates = CertificateDer::pem_slice_iter(&pem)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parse certificate {}", path.display()))?;
    if certificates.is_empty() {
        bail!(
            "certificate file {} contains no certificates",
            path.display()
        );
    }
    Ok(certificates)
}

pub fn leaf_certificate_sha256(paths: &PeerTlsConfig) -> Result<String> {
    let chain = certificate_chain(&paths.certificate_chain)?;
    Ok(format!("{:x}", Sha256::digest(chain[0].as_ref())))
}

pub fn certificate_sha256(certificate: &CertificateDer<'_>) -> String {
    format!("{:x}", Sha256::digest(certificate.as_ref()))
}

fn private_key(path: &std::path::Path) -> Result<PrivateKeyDer<'static>> {
    let metadata = std::fs::symlink_metadata(path)
        .with_context(|| format!("inspect private key {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        bail!(
            "private key {} must be a regular non-symlink file",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!(
                "private key {} must not be accessible by group or other users",
                path.display()
            );
        }
    }
    let pem =
        std::fs::read(path).with_context(|| format!("read private key {}", path.display()))?;
    PrivateKeyDer::from_pem_slice(&pem)
        .with_context(|| format!("parse private key {}", path.display()))
}

fn trust_roots(path: &std::path::Path) -> Result<RootCertStore> {
    let mut roots = RootCertStore::empty();
    let certificates = certificate_chain(path)?;
    let (accepted, rejected) = roots.add_parsable_certificates(certificates);
    if accepted == 0 || rejected != 0 {
        bail!(
            "client CA bundle {} contained {accepted} accepted and {rejected} rejected certificates",
            path.display()
        );
    }
    Ok(roots)
}

pub fn server_config(paths: &PeerTlsConfig) -> Result<Arc<ServerConfig>> {
    ensure_crypto_provider();
    let verifier = WebPkiClientVerifier::builder(Arc::new(trust_roots(&paths.client_ca_bundle)?))
        .build()
        .context("build mandatory peer client-certificate verifier")?;
    let mut config = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            certificate_chain(&paths.certificate_chain)?,
            private_key(&paths.private_key)?,
        )
        .context("configure peer TLS server certificate")?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

pub fn client_config(paths: &PeerTlsConfig) -> Result<Arc<ClientConfig>> {
    ensure_crypto_provider();
    let mut config = ClientConfig::builder()
        .with_root_certificates(trust_roots(&paths.client_ca_bundle)?)
        .with_client_auth_cert(
            certificate_chain(&paths.certificate_chain)?,
            private_key(&paths.private_key)?,
        )
        .context("configure peer TLS client identity")?;
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

pub fn reqwest_identity(paths: &PeerTlsConfig) -> Result<reqwest::Identity> {
    let _ = private_key(&paths.private_key)?;
    let mut pem = std::fs::read(&paths.certificate_chain)
        .with_context(|| format!("read certificate {}", paths.certificate_chain.display()))?;
    if !pem.ends_with(b"\n") {
        pem.push(b'\n');
    }
    pem.extend(
        std::fs::read(&paths.private_key)
            .with_context(|| format!("read private key {}", paths.private_key.display()))?,
    );
    reqwest::Identity::from_pem(&pem).context("parse peer TLS client identity")
}

pub fn reqwest_roots(paths: &PeerTlsConfig) -> Result<Vec<reqwest::Certificate>> {
    certificate_chain(&paths.client_ca_bundle)?
        .into_iter()
        .map(|certificate| {
            reqwest::Certificate::from_der(certificate.as_ref())
                .context("parse peer TLS CA certificate")
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, KeyPair};
    use rustls::pki_types::ServerName;
    use std::{path::PathBuf, time::Duration};
    use tokio::io::duplex;

    pub(crate) struct TestPki {
        pub(crate) directory: PathBuf,
        pub(crate) server: PeerTlsConfig,
        server_overlap: PeerTlsConfig,
        server_rotated: PeerTlsConfig,
        pub(crate) client: PeerTlsConfig,
        rotated_client: PeerTlsConfig,
        untrusted_client: PeerTlsConfig,
    }

    impl Drop for TestPki {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.directory);
        }
    }

    fn ca(common_name: &str) -> (rcgen::Certificate, KeyPair) {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, common_name);
        params.distinguished_name = name;
        let key = KeyPair::generate().unwrap();
        (params.self_signed(&key).unwrap(), key)
    }

    fn leaf(
        common_name: &str,
        dns_names: Vec<String>,
        ca: &rcgen::Certificate,
        ca_key: &KeyPair,
    ) -> (String, String) {
        let mut params = CertificateParams::new(dns_names).unwrap();
        let mut name = DistinguishedName::new();
        name.push(DnType::CommonName, common_name);
        params.distinguished_name = name;
        let key = KeyPair::generate().unwrap();
        let certificate = params.signed_by(&key, ca, ca_key).unwrap();
        (certificate.pem(), key.serialize_pem())
    }

    fn write(directory: &std::path::Path, name: &str, contents: &str) -> PathBuf {
        let path = directory.join(name);
        std::fs::write(&path, contents).unwrap();
        if name.contains("key") {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            }
        }
        path
    }

    pub(crate) fn test_pki() -> TestPki {
        let directory = std::env::temp_dir().join(format!(
            "tarit-peer-tls-test-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir(&directory).unwrap();
        let (trusted_ca, trusted_ca_key) = ca("trusted test CA");
        let (rotated_ca, rotated_ca_key) = ca("rotated test CA");
        let (untrusted_ca, untrusted_ca_key) = ca("untrusted test CA");
        let (server_cert, server_key) = leaf(
            "node-a",
            vec!["localhost".into()],
            &trusted_ca,
            &trusted_ca_key,
        );
        let (client_cert, client_key) = leaf("node-b", Vec::new(), &trusted_ca, &trusted_ca_key);
        let (untrusted_cert, untrusted_key) =
            leaf("rogue-node", Vec::new(), &untrusted_ca, &untrusted_ca_key);
        let (rotated_client_cert, rotated_client_key) =
            leaf("node-c", Vec::new(), &rotated_ca, &rotated_ca_key);
        let trusted_ca_path = write(&directory, "trusted-ca.pem", &trusted_ca.pem());
        let rotated_ca_path = write(&directory, "rotated-ca.pem", &rotated_ca.pem());
        let overlap_ca_path = write(
            &directory,
            "overlap-ca.pem",
            &format!("{}{}", trusted_ca.pem(), rotated_ca.pem()),
        );
        let untrusted_ca_path = write(&directory, "untrusted-ca.pem", &untrusted_ca.pem());
        let server_cert_path = write(&directory, "server.pem", &server_cert);
        let server_key_path = write(&directory, "server-key.pem", &server_key);
        TestPki {
            server: PeerTlsConfig {
                certificate_chain: server_cert_path.clone(),
                private_key: server_key_path.clone(),
                client_ca_bundle: trusted_ca_path.clone(),
            },
            server_overlap: PeerTlsConfig {
                certificate_chain: server_cert_path.clone(),
                private_key: server_key_path.clone(),
                client_ca_bundle: overlap_ca_path.clone(),
            },
            server_rotated: PeerTlsConfig {
                certificate_chain: server_cert_path,
                private_key: server_key_path,
                client_ca_bundle: rotated_ca_path,
            },
            client: PeerTlsConfig {
                certificate_chain: write(&directory, "client.pem", &client_cert),
                private_key: write(&directory, "client-key.pem", &client_key),
                client_ca_bundle: overlap_ca_path.clone(),
            },
            rotated_client: PeerTlsConfig {
                certificate_chain: write(&directory, "rotated-client.pem", &rotated_client_cert),
                private_key: write(&directory, "rotated-client-key.pem", &rotated_client_key),
                client_ca_bundle: overlap_ca_path,
            },
            untrusted_client: PeerTlsConfig {
                certificate_chain: write(&directory, "rogue.pem", &untrusted_cert),
                private_key: write(&directory, "rogue-key.pem", &untrusted_key),
                client_ca_bundle: untrusted_ca_path,
            },
            directory,
        }
    }

    async fn handshake(server: Arc<ServerConfig>, client: Arc<ClientConfig>) -> bool {
        let (server_io, client_io) = duplex(64 * 1024);
        let server = tokio_rustls::TlsAcceptor::from(server).accept(server_io);
        let client = tokio_rustls::TlsConnector::from(client)
            .connect(ServerName::try_from("localhost").unwrap(), client_io);
        matches!(
            tokio::time::timeout(Duration::from_secs(2), async {
                tokio::join!(server, client)
            })
            .await,
            Ok((Ok(_), Ok(_)))
        )
    }

    #[tokio::test]
    async fn mutual_tls_accepts_trusted_client_and_rejects_untrusted_client() {
        let pki = test_pki();
        assert!(
            handshake(
                server_config(&pki.server).unwrap(),
                client_config(&pki.client).unwrap()
            )
            .await
        );
        assert!(
            !handshake(
                server_config(&pki.server).unwrap(),
                client_config(&pki.untrusted_client).unwrap(),
            )
            .await
        );
        ensure_crypto_provider();
        let unauthenticated = Arc::new(
            ClientConfig::builder()
                .with_root_certificates(trust_roots(&pki.client.client_ca_bundle).unwrap())
                .with_no_client_auth(),
        );
        assert!(!handshake(server_config(&pki.server).unwrap(), unauthenticated).await);
    }

    #[tokio::test]
    async fn client_ca_rotation_supports_overlap_then_fences_the_old_certificate() {
        let pki = test_pki();
        let overlap = server_config(&pki.server_overlap).unwrap();
        assert!(handshake(Arc::clone(&overlap), client_config(&pki.client).unwrap()).await);
        assert!(handshake(overlap, client_config(&pki.rotated_client).unwrap(),).await);

        let rotated = server_config(&pki.server_rotated).unwrap();
        assert!(!handshake(Arc::clone(&rotated), client_config(&pki.client).unwrap()).await);
        assert!(handshake(rotated, client_config(&pki.rotated_client).unwrap()).await);
    }

    #[cfg(unix)]
    #[test]
    fn private_key_with_group_or_world_access_is_rejected() {
        use std::os::unix::fs::PermissionsExt;
        let pki = test_pki();
        std::fs::set_permissions(
            &pki.server.private_key,
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(server_config(&pki.server).is_err());
    }
}
