use chacha20poly1305::{
    aead::{Aead, KeyInit},
    ChaCha20Poly1305,
};
use rand::{rngs::OsRng, TryRngCore};

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("failed to decrypt sealed secret")]
    Decrypt,
}

pub struct SealedSecret {
    pub nonce: [u8; 12],
    pub ciphertext: Vec<u8>,
}

pub fn seal(kek: &[u8; 32], plaintext: &[u8]) -> SealedSecret {
    let cipher = ChaCha20Poly1305::new(kek.into());
    let mut nonce = [0; 12];
    OsRng
        .try_fill_bytes(&mut nonce)
        .expect("OS RNG must provide a nonce");
    let ciphertext = cipher
        .encrypt(&nonce.into(), plaintext)
        .expect("ChaCha20Poly1305 encryption must succeed");

    SealedSecret { nonce, ciphertext }
}

pub fn open(kek: &[u8; 32], sealed: &SealedSecret) -> Result<Vec<u8>, CryptoError> {
    ChaCha20Poly1305::new(kek.into())
        .decrypt(&sealed.nonce.into(), sealed.ciphertext.as_ref())
        .map_err(|_| CryptoError::Decrypt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip_and_wrong_key_fails() {
        let kek = [3u8; 32];
        let sealed = seal(&kek, b"private-key-pem");
        assert_ne!(sealed.ciphertext, b"private-key-pem");
        assert_eq!(open(&kek, &sealed).unwrap(), b"private-key-pem");
        let mut wrong = kek;
        wrong[0] ^= 0xff;
        assert!(open(&wrong, &sealed).is_err());
    }

    #[test]
    fn each_seal_uses_a_fresh_nonce() {
        let kek = [9u8; 32];
        let a = seal(&kek, b"x");
        let b = seal(&kek, b"x");
        assert_ne!(a.nonce, b.nonce);
    }
}
