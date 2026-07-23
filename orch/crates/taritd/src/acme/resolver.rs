use std::{collections::HashMap, fmt, sync::Arc};

use arc_swap::ArcSwap;
use rustls::{
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};

pub struct CertStore {
    exact: HashMap<String, Arc<CertifiedKey>>,
    wildcard: Option<(String, Arc<CertifiedKey>)>,
}

impl CertStore {
    pub fn from_wildcard(base_domain: &str, key: Arc<CertifiedKey>) -> Self {
        Self {
            exact: HashMap::new(),
            wildcard: normalize_sni(base_domain).map(|base| (base, key)),
        }
    }

    pub fn empty() -> Self {
        Self {
            exact: HashMap::new(),
            wildcard: None,
        }
    }

    pub fn insert_exact(&mut self, sni: &str, key: Arc<CertifiedKey>) {
        if let Some(sni) = normalize_sni(sni) {
            self.exact.insert(sni, key);
        }
    }

    pub fn select(&self, sni: Option<&str>) -> Option<Arc<CertifiedKey>> {
        let sni = normalize_sni(sni?)?;

        if let Some(key) = self.exact.get(&sni) {
            return Some(Arc::clone(key));
        }

        let (base_domain, key) = self.wildcard.as_ref()?;
        let label = sni
            .strip_suffix(base_domain)
            .and_then(|prefix| prefix.strip_suffix('.'))?;

        (!label.is_empty() && !label.contains('.')).then(|| Arc::clone(key))
    }
}

pub struct CertResolver {
    inner: ArcSwap<CertStore>,
}

impl CertResolver {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: ArcSwap::from_pointee(CertStore::empty()),
        })
    }

    pub fn install(&self, store: CertStore) {
        self.inner.store(Arc::new(store));
    }

    pub fn current(&self) -> Arc<CertStore> {
        self.inner.load_full()
    }
}

impl fmt::Debug for CertResolver {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CertResolver")
            .finish_non_exhaustive()
    }
}

impl ResolvesServerCert for CertResolver {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.inner.load().select(client_hello.server_name())
    }
}

fn normalize_sni(sni: &str) -> Option<String> {
    idna::domain_to_ascii(sni)
        .ok()
        .map(|domain| domain.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rcgen::{CertificateParams, KeyPair};
    use rustls::{
        crypto::ring::sign::any_supported_type,
        pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer},
        sign::CertifiedKey,
    };

    use super::*;

    fn test_key(subject_alt_name: &str) -> Arc<CertifiedKey> {
        let key_pair = KeyPair::generate().unwrap();
        let certificate = CertificateParams::new(vec![subject_alt_name.to_owned()])
            .unwrap()
            .self_signed(&key_pair)
            .unwrap();
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_pair.serialize_der()));
        let signing_key = any_supported_type(&private_key).unwrap();

        Arc::new(CertifiedKey::new(
            vec![certificate.der().clone()],
            signing_key,
        ))
    }

    fn wildcard_key() -> Arc<CertifiedKey> {
        test_key("*.shares.example.com")
    }

    fn exact_key() -> Arc<CertifiedKey> {
        test_key("special.shares.example.com")
    }

    #[test]
    fn wildcard_matches_exactly_one_label() {
        let store = CertStore::from_wildcard("shares.example.com", wildcard_key());

        assert!(store.select(Some("abc.shares.example.com")).is_some());
        assert!(store.select(Some("ABC.shares.example.com")).is_some());
        assert!(store.select(Some("a.b.shares.example.com")).is_none());
        assert!(store.select(Some("shares.example.com")).is_none());
        assert!(store.select(Some("evil.com")).is_none());
        assert!(store.select(None).is_none());
    }

    #[test]
    fn exact_entry_precedes_wildcard() {
        let mut store = CertStore::from_wildcard("shares.example.com", wildcard_key());
        let expected = exact_key();
        store.insert_exact("special.shares.example.com", Arc::clone(&expected));

        let got = store.select(Some("special.shares.example.com")).unwrap();

        assert!(Arc::ptr_eq(&got, &expected));
    }
}
