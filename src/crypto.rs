//! Everything derived from `TOKEN_KEY`: encrypted LB tokens and keyed hashes of Discord ids.
//!
//! Each use gets its own subkey from BLAKE3's key derivation, so one key never serves two purposes.

use anyhow::{Context, Result, anyhow, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::{
    ChaCha20Poly1305, Key, Nonce, XChaCha20Poly1305, XNonce,
    aead::{Aead, Generate, KeyInit, Payload},
};

/// First byte of a stored token in the current format: version || 24-byte nonce || ciphertext + tag.
const TOKEN_V2: u8 = 2;

pub struct Crypto {
    tokens: XChaCha20Poly1305,
    ids: [u8; 32],
    /// The first format: ChaCha20-Poly1305 with the raw key and no associated data. Read only by the
    /// startup migration that rewrites old rows (`Db::migrate_hashed_ids`).
    legacy: ChaCha20Poly1305,
}

impl Crypto {
    /// `TOKEN_KEY`: 32 bytes, base64 encoded (`openssl rand -base64 32`).
    pub fn from_base64(key: &str) -> Result<Self> {
        let bytes = STANDARD.decode(key.trim()).context("TOKEN_KEY is not base64")?;
        let raw = Key::try_from(bytes.as_slice()).context("TOKEN_KEY must be 32 bytes")?;
        let tokens = blake3::derive_key("lbbot 2026 listenbrainz token encryption", &raw);
        Ok(Self {
            tokens: XChaCha20Poly1305::new(&Key::from(tokens)),
            ids: blake3::derive_key("lbbot 2026 discord id hash", &raw),
            legacy: ChaCha20Poly1305::new(&raw),
        })
    }

    /// Keyed hash of a Discord user or server id. The database stores only this, so a leaked database
    /// doesn't show which Discord account belongs to which ListenBrainz account.
    pub fn id(&self, discord_id: i64) -> i64 {
        let hash = blake3::keyed_hash(&self.ids, &discord_id.to_le_bytes());
        i64::from_le_bytes(hash.as_bytes()[..8].try_into().expect("8 bytes"))
    }

    /// Short form of `id` for logs.
    pub fn log_id(&self, discord_id: i64) -> String {
        format!("{:08x}", self.id(discord_id) as u32)
    }

    /// `owner` (the user's hashed Discord id) is authenticated with the token, so a token copied to
    /// another user's row fails to decrypt.
    pub fn encrypt(&self, plain: &str, owner: i64) -> Vec<u8> {
        let nonce = XNonce::generate();
        let payload = Payload { msg: plain.as_bytes(), aad: &owner.to_le_bytes() };
        let mut out = vec![TOKEN_V2];
        out.extend_from_slice(&nonce);
        out.extend(self.tokens.encrypt(&nonce, payload).expect("encrypting into Vec cannot fail"));
        out
    }

    pub fn decrypt(&self, data: &[u8], owner: i64) -> Result<String> {
        ensure!(data.len() > 1 + 24 + 16 && data[0] == TOKEN_V2, "stored token has an unknown format");
        let (nonce, ct) = data[1..].split_at(24);
        let nonce = XNonce::try_from(nonce).context("bad nonce")?;
        let plain = self
            .tokens
            .decrypt(&nonce, Payload { msg: ct, aad: &owner.to_le_bytes() })
            .map_err(|_| anyhow!("token decryption failed"))?;
        Ok(String::from_utf8(plain)?)
    }

    pub fn decrypt_legacy(&self, data: &[u8]) -> Result<String> {
        ensure!(data.len() > 12, "stored token too short");
        let (nonce, ct) = data.split_at(12);
        let nonce = Nonce::try_from(nonce).context("bad nonce")?;
        let plain = self.legacy.decrypt(&nonce, ct).map_err(|_| anyhow!("token decryption failed"))?;
        Ok(String::from_utf8(plain)?)
    }

    #[cfg(test)]
    pub fn encrypt_legacy(&self, plain: &str) -> Vec<u8> {
        let nonce = Nonce::generate();
        let mut out = nonce.to_vec();
        out.extend(self.legacy.encrypt(&nonce, plain.as_bytes()).expect("encrypting into Vec cannot fail"));
        out
    }
}

#[cfg(test)]
pub fn test_crypto() -> Crypto {
    Crypto::from_base64("AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=").unwrap()
}

#[cfg(test)]
mod tests {
    #[test]
    fn tokens_roundtrip_and_are_bound_to_their_owner() {
        let c = super::test_crypto();
        let enc = c.encrypt("secret-token", 42);
        assert_eq!(c.decrypt(&enc, 42).unwrap(), "secret-token");
        assert!(c.decrypt(&enc, 43).is_err(), "a token copied to another user must not decrypt");
        assert_ne!(c.encrypt("secret-token", 42), enc);
        assert!(c.decrypt(&c.encrypt_legacy("secret-token"), 42).is_err());
        assert_eq!(c.decrypt_legacy(&c.encrypt_legacy("secret-token")).unwrap(), "secret-token");
    }

    #[test]
    fn ids_are_stable_keyed_and_distinct() {
        let c = super::test_crypto();
        let other = super::Crypto::from_base64("HxwdHh8AAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRo=").unwrap();
        let id = 1_575_552_236_925_493_248;
        assert_eq!(c.id(id), c.id(id));
        assert_ne!(c.id(id), id);
        assert_ne!(c.id(id), c.id(id + 1));
        assert_ne!(c.id(id), other.id(id), "the hash depends on TOKEN_KEY");
    }
}
