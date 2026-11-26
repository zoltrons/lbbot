//! Users' LB tokens are stored encrypted in the database (nonce || ciphertext).

use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce,
    aead::{Aead, Generate, KeyInit},
};

pub struct Crypto(ChaCha20Poly1305);

impl Crypto {
    /// `TOKEN_KEY`: 32 bytes, base64 encoded (`openssl rand -base64 32`).
    pub fn from_base64(key: &str) -> Result<Self> {
        let bytes = STANDARD.decode(key.trim()).context("TOKEN_KEY is not base64")?;
        let key = Key::try_from(bytes.as_slice()).context("TOKEN_KEY must be 32 bytes")?;
        Ok(Self(ChaCha20Poly1305::new(&key)))
    }

    pub fn encrypt(&self, plain: &str) -> Vec<u8> {
        let nonce = Nonce::generate();
        let mut out = nonce.to_vec();
        out.extend(self.0.encrypt(&nonce, plain.as_bytes()).expect("encrypting into Vec cannot fail"));
        out
    }

    pub fn decrypt(&self, data: &[u8]) -> Result<String> {
        ensure!(data.len() > 12, "stored token too short");
        let (nonce, ct) = data.split_at(12);
        let nonce = Nonce::try_from(nonce).context("bad nonce")?;
        let plain = self.0.decrypt(&nonce, ct).map_err(|_| anyhow!("token decryption failed"))?;
        Ok(String::from_utf8(plain)?)
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn roundtrip() {
        let c = super::Crypto::from_base64("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap();
        let enc = c.encrypt("secret-token");
        assert_eq!(c.decrypt(&enc).unwrap(), "secret-token");
        assert_ne!(c.encrypt("secret-token"), enc);
    }
}
