//! The connections vault — the whole key hierarchy, in the SHARED core.
//!
//! Every client that can open a connection runs this exact code: the cli
//! natively, the web through wasm. That is not tidiness. The blobs are the
//! interop surface between a user's own machines, so two implementations of
//! "wrap a master key to a device" would diverge in a nonce, a KDF label or a
//! byte order and the symptom would be *"the credential I added on my laptop
//! won't open in the browser"* — a data bug that only shows up on the second
//! device, i.e. after the user already trusted it.
//!
//! ```text
//!   device keypair (X25519)      private half never leaves the machine
//!         │  wraps
//!         ▼
//!   user master key (UMK)        32 random bytes, one per user
//!         │  wraps                       ▲
//!         ▼                              │ also wrapped by an Argon2id key
//!   per-connection DEK                   │ derived from a recovery passphrase
//!         │  encrypts
//!         ▼
//!   secret payload (JSON)
//! ```
//!
//! What lives OUTSIDE this module, deliberately: where a device key is stored.
//! The cli writes a 0600 file, the browser keeps it in IndexedDB — different
//! machines, different guarantees, and pretending one abstraction covers both
//! would mean the weaker one silently sets the terms. See `.docs/connections-v1.md`.

use anyhow_lite::{bail, Context as _, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use chacha20poly1305::{
    aead::{Aead, KeyInit},
    XChaCha20Poly1305, XNonce,
};
use rand::RngCore;
use zeroize::Zeroize;

/// Minimal error plumbing so the core doesn't take an `anyhow` dependency for
/// one module. Same shape at the call sites.
pub mod anyhow_lite {
    #[derive(Debug)]
    pub struct Error(pub String);
    impl std::fmt::Display for Error {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.0)
        }
    }
    impl std::error::Error for Error {}
    pub type Result<T> = std::result::Result<T, Error>;
    pub trait Context<T> {
        fn context(self, msg: &str) -> Result<T>;
    }
    impl<T, E: std::fmt::Display> Context<T> for std::result::Result<T, E> {
        fn context(self, msg: &str) -> Result<T> {
            self.map_err(|e| Error(format!("{msg}: {e}")))
        }
    }
    impl<T> Context<T> for Option<T> {
        fn context(self, msg: &str) -> Result<T> {
            self.ok_or_else(|| Error(msg.to_string()))
        }
    }
    macro_rules! bail {
        ($($t:tt)*) => { return Err($crate::vault::anyhow_lite::Error(format!($($t)*))) };
    }
    pub(crate) use bail;
}

const NONCE: usize = 24;

/// Argon2id cost for the recovery passphrase. Deliberately expensive: the
/// recovery blob is the only part of the vault an attacker can grind offline
/// after a server compromise, and it is guarded by something a human typed.
pub const ARGON_MEM_KIB: u32 = 64 * 1024;
pub const ARGON_TIME: u32 = 3;
pub const ARGON_LANES: u32 = 1;

// ── key material ───────────────────────────────────────────────────────────

/// 32 bytes that must not linger in freed memory.
#[derive(Clone)]
pub struct Key(pub [u8; 32]);

impl Drop for Key {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

impl Key {
    pub fn random() -> Self {
        let mut k = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut k);
        Self(k)
    }
    pub fn from_b64(s: &str) -> Result<Self> {
        Ok(Self(decode32(s, "key")?))
    }
    pub fn to_b64(&self) -> String {
        B64.encode(self.0)
    }
    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&self.0).into())
    }
}

/// `nonce(24) || ciphertext`, base64. Self-contained so a blob can be moved
/// between devices without carrying parameters alongside it.
pub fn seal(key: &Key, plaintext: &[u8]) -> String {
    let mut nonce_bytes = [0u8; NONCE];
    rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
    let ct = key
        .cipher()
        .encrypt(XNonce::from_slice(&nonce_bytes), plaintext)
        .expect("XChaCha20-Poly1305 never fails with a fresh nonce");
    let mut out = Vec::with_capacity(NONCE + ct.len());
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    B64.encode(out)
}

pub fn open(key: &Key, blob: &str) -> Result<Vec<u8>> {
    let raw = B64.decode(blob.trim()).context("not valid base64")?;
    if raw.len() < NONCE {
        bail!("blob too short");
    }
    let (nonce, ct) = raw.split_at(NONCE);
    key.cipher()
        .decrypt(XNonce::from_slice(nonce), ct)
        .map_err(|_| anyhow_lite::Error("wrong key or corrupt data".into()))
}

fn decode32(b64: &str, what: &str) -> Result<[u8; 32]> {
    let raw = B64.decode(b64.trim()).context(what)?;
    let n = raw.len();
    raw.try_into()
        .map_err(|_| anyhow_lite::Error(format!("{what}: expected 32 bytes, got {n}")))
}

/// A short, readable digest of a public key — the string two humans compare
/// before one approves the other. Truncated to 8 groups of 4 hex chars; a full
/// key is unreadable aloud, and an unread fingerprint protects nobody.
pub fn fingerprint(public_b64: &str) -> String {
    use sha2::{Digest, Sha256};
    let hex = format!("{:x}", Sha256::digest(public_b64.as_bytes()));
    hex.as_bytes()
        .chunks(4)
        .take(8)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("-")
}

/// A device keypair. The secret is base64 here because every caller has to
/// persist it somewhere platform-specific; it is never sent anywhere.
pub struct DeviceKeypair {
    pub secret: String,
    pub public: String,
}

pub fn generate_device() -> DeviceKeypair {
    let secret = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
    let public = x25519_dalek::PublicKey::from(&secret);
    DeviceKeypair {
        secret: B64.encode(secret.to_bytes()),
        public: B64.encode(public.as_bytes()),
    }
}

pub fn public_from_secret(secret_b64: &str) -> Result<String> {
    let secret = x25519_dalek::StaticSecret::from(decode32(secret_b64, "device secret")?);
    Ok(B64.encode(x25519_dalek::PublicKey::from(&secret).as_bytes()))
}

// ── wrapping the master key to a device ────────────────────────────────────

/// Seal `umk` **to** a device's public key: ephemeral X25519 → HKDF → AEAD.
///
/// The ephemeral public key travels with the ciphertext, so the recipient needs
/// nothing but its own secret. A fresh ephemeral per wrap means approving the
/// same device twice never reuses a key stream.
pub fn wrap_umk_for(recipient_public_b64: &str, umk: &Key) -> Result<String> {
    let recipient = x25519_dalek::PublicKey::from(decode32(recipient_public_b64, "public key")?);
    let eph = x25519_dalek::EphemeralSecret::random_from_rng(rand::rngs::OsRng);
    let eph_pub = x25519_dalek::PublicKey::from(&eph);
    let shared = eph.diffie_hellman(&recipient);
    let wrapping = derive(shared.as_bytes(), eph_pub.as_bytes(), recipient.as_bytes());
    let sealed = seal(&wrapping, &umk.0);
    Ok(format!("{}.{}", B64.encode(eph_pub.as_bytes()), sealed))
}

/// Open a wrap addressed to this device.
pub fn unwrap_umk(device_secret_b64: &str, wrapped: &str) -> Result<Key> {
    let (eph_b64, sealed) = wrapped.split_once('.').context("malformed wrapped key")?;
    let secret = x25519_dalek::StaticSecret::from(decode32(device_secret_b64, "device secret")?);
    let my_pub = x25519_dalek::PublicKey::from(&secret);
    let eph_pub = x25519_dalek::PublicKey::from(decode32(eph_b64, "ephemeral key")?);
    let shared = secret.diffie_hellman(&eph_pub);
    let wrapping = derive(shared.as_bytes(), eph_pub.as_bytes(), my_pub.as_bytes());
    let raw = open(&wrapping, sealed).context("this wrapped key is not for this device")?;
    Ok(Key(raw
        .try_into()
        .map_err(|_| anyhow_lite::Error("wrapped key has the wrong length".into()))?))
}

/// HKDF over the ECDH output, bound to both public keys.
///
/// Binding matters: a raw ECDH secret is the same for any pair, so without the
/// endpoints in the transcript a wrap could be replayed toward a different
/// recipient and still open.
fn derive(shared: &[u8; 32], eph_pub: &[u8; 32], recipient_pub: &[u8; 32]) -> Key {
    let mut info = Vec::with_capacity(64);
    info.extend_from_slice(eph_pub);
    info.extend_from_slice(recipient_pub);
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(b"mafold-connections-v1"), shared);
    let mut out = [0u8; 32];
    hk.expand(&info, &mut out).expect("32 bytes is a valid HKDF length");
    Key(out)
}

// ── per-connection payloads ────────────────────────────────────────────────

/// A sealed connection: the payload under a fresh DEK, and that DEK under the
/// master key. Returned together because storing one without the other is a row
/// nobody can ever open.
pub struct SealedPayload {
    pub blob: String,
    pub wrapped_dek: String,
}

pub fn seal_payload(umk: &Key, payload_json: &str) -> SealedPayload {
    let dek = Key::random();
    SealedPayload {
        blob: seal(&dek, payload_json.as_bytes()),
        wrapped_dek: seal(umk, &dek.0),
    }
}

pub fn open_payload(umk: &Key, blob: &str, wrapped_dek: &str) -> Result<String> {
    let dek_raw = open(umk, wrapped_dek).context("unwrap DEK")?;
    let dek = Key(dek_raw
        .try_into()
        .map_err(|_| anyhow_lite::Error("stored DEK has the wrong length".into()))?);
    let plain = open(&dek, blob).context("open connection payload")?;
    String::from_utf8(plain).context("connection payload is not UTF-8")
}

// ── recovery passphrase ────────────────────────────────────────────────────

pub struct RecoveryBlob {
    pub salt: String,
    pub mem_kib: u32,
    pub time_cost: u32,
    pub lanes: u32,
    pub sealed_umk: String,
}

pub fn wrap_umk_with_passphrase(umk: &Key, passphrase: &str) -> Result<RecoveryBlob> {
    let mut salt = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut salt);
    let key = argon_key(passphrase, &salt, ARGON_MEM_KIB, ARGON_TIME, ARGON_LANES)?;
    Ok(RecoveryBlob {
        salt: B64.encode(salt),
        mem_kib: ARGON_MEM_KIB,
        time_cost: ARGON_TIME,
        lanes: ARGON_LANES,
        sealed_umk: seal(&key, &umk.0),
    })
}

/// Open a recovery blob. Parameters come FROM the blob, never from the
/// constants above — a blob written before a cost increase must stay openable.
pub fn unwrap_umk_with_passphrase(blob: &RecoveryBlob, passphrase: &str) -> Result<Key> {
    let salt = B64.decode(blob.salt.trim()).context("salt")?;
    let key = argon_key(passphrase, &salt, blob.mem_kib, blob.time_cost, blob.lanes)?;
    let raw = open(&key, &blob.sealed_umk).context("wrong recovery passphrase")?;
    Ok(Key(raw
        .try_into()
        .map_err(|_| anyhow_lite::Error("recovery blob has the wrong length".into()))?))
}

fn argon_key(pass: &str, salt: &[u8], mem_kib: u32, time: u32, lanes: u32) -> Result<Key> {
    let params = argon2::Params::new(mem_kib, time, lanes, Some(32))
        .map_err(|e| anyhow_lite::Error(format!("bad Argon2 parameters: {e}")))?;
    let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut out = [0u8; 32];
    a.hash_password_into(pass.as_bytes(), salt, &mut out)
        .map_err(|e| anyhow_lite::Error(format!("key derivation failed: {e}")))?;
    Ok(Key(out))
}

/// A new master-key generation id. Not a secret — it exists so a device holding
/// an older UMK can say "locked, a rotate reached you" instead of reporting a
/// decrypt failure that reads like data loss.
pub fn new_key_id() -> String {
    let mut b = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seal_open_roundtrip() {
        let k = Key::random();
        let blob = seal(&k, b"sk-ant-secret");
        assert_eq!(open(&k, &blob).unwrap(), b"sk-ant-secret");
    }

    /// The property the whole design rests on: ciphertext without the key is
    /// just bytes.
    #[test]
    fn a_different_key_cannot_open_it() {
        let blob = seal(&Key::random(), b"sk-ant-secret");
        assert!(open(&Key::random(), &blob).is_err());
    }

    #[test]
    fn tampered_ciphertext_is_rejected_not_silently_garbled() {
        let k = Key::random();
        let blob = seal(&k, b"sk-ant-secret");
        let mut raw = B64.decode(&blob).unwrap();
        let last = raw.len() - 1;
        raw[last] ^= 0x01;
        assert!(open(&k, &B64.encode(raw)).is_err());
    }

    #[test]
    fn a_device_can_open_a_wrap_addressed_to_it() {
        let d = generate_device();
        let umk = Key::random();
        let wrapped = wrap_umk_for(&d.public, &umk).unwrap();
        assert_eq!(unwrap_umk(&d.secret, &wrapped).unwrap().0, umk.0);
    }

    /// Enrollment goes through the server, so a wrap meant for one machine must
    /// be useless to another even though both are the same user's.
    #[test]
    fn another_device_cannot_open_someone_elses_wrap() {
        let (a, b) = (generate_device(), generate_device());
        let umk = Key::random();
        let for_a = wrap_umk_for(&a.public, &umk).unwrap();
        assert!(unwrap_umk(&b.secret, &for_a).is_err());
    }

    #[test]
    fn wrapping_twice_produces_different_bytes() {
        let d = generate_device();
        let umk = Key::random();
        assert_ne!(
            wrap_umk_for(&d.public, &umk).unwrap(),
            wrap_umk_for(&d.public, &umk).unwrap()
        );
    }

    /// The cross-client contract: a payload sealed with one device's unlocked
    /// UMK must open on a DIFFERENT device that was wrapped the same key. This
    /// is the web-adds → cli-reads path, in miniature.
    #[test]
    fn a_payload_sealed_by_one_device_opens_on_another() {
        let umk = Key::random();
        let (web, cli) = (generate_device(), generate_device());
        let for_web = wrap_umk_for(&web.public, &umk).unwrap();
        let for_cli = wrap_umk_for(&cli.public, &umk).unwrap();

        let web_umk = unwrap_umk(&web.secret, &for_web).unwrap();
        let sealed = seal_payload(&web_umk, r#"{"token":"ntn_from_the_browser"}"#);

        let cli_umk = unwrap_umk(&cli.secret, &for_cli).unwrap();
        let out = open_payload(&cli_umk, &sealed.blob, &sealed.wrapped_dek).unwrap();
        assert_eq!(out, r#"{"token":"ntn_from_the_browser"}"#);
    }

    #[test]
    fn recovery_roundtrips_and_rejects_a_wrong_passphrase() {
        let umk = Key::random();
        let blob = wrap_umk_with_passphrase(&umk, "correct horse battery staple").unwrap();
        assert_eq!(
            unwrap_umk_with_passphrase(&blob, "correct horse battery staple").unwrap().0,
            umk.0
        );
        assert!(unwrap_umk_with_passphrase(&blob, "wrong").is_err());
    }

    /// Parameters travel with the blob, so raising the cost later must not
    /// strand blobs written at the old cost.
    #[test]
    fn recovery_honours_the_blobs_own_kdf_parameters() {
        let umk = Key::random();
        let mut salt = [0u8; 16];
        rand::rngs::OsRng.fill_bytes(&mut salt);
        let cheap = argon_key("pw", &salt, 8 * 1024, 1, 1).unwrap();
        let blob = RecoveryBlob {
            salt: B64.encode(salt),
            mem_kib: 8 * 1024,
            time_cost: 1,
            lanes: 1,
            sealed_umk: seal(&cheap, &umk.0),
        };
        assert_eq!(unwrap_umk_with_passphrase(&blob, "pw").unwrap().0, umk.0);
    }

    #[test]
    fn fingerprints_are_stable_readable_and_key_specific() {
        let (a, b) = (generate_device(), generate_device());
        assert_eq!(fingerprint(&a.public), fingerprint(&a.public));
        assert_ne!(fingerprint(&a.public), fingerprint(&b.public));
        assert_eq!(fingerprint(&a.public).len(), 8 * 4 + 7);
    }

    #[test]
    fn public_key_is_recoverable_from_the_secret() {
        let d = generate_device();
        assert_eq!(public_from_secret(&d.secret).unwrap(), d.public);
    }

    #[test]
    fn key_ids_are_distinct() {
        assert_ne!(new_key_id(), new_key_id());
    }
}
