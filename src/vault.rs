//! Where THIS machine keeps its vault key material.
//!
//! The crypto itself is not here — it moved to `mafold_core::vault` so the cli
//! and the web run the same key hierarchy. Two implementations would eventually
//! disagree on a nonce or a KDF label, and the symptom would be a credential
//! added on one device that refuses to open on another: a data bug that only
//! appears after the user has already trusted the second device.
//!
//! What IS here is the part that genuinely differs per platform: a Unix file at
//! 0600 versus the browser's IndexedDB. Pretending one abstraction covered both
//! would mean the weaker guarantee silently setting the terms for the stronger.

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

pub use mafold_core::vault::{
    fingerprint, new_key_id, open_payload, seal_payload, unwrap_umk, unwrap_umk_with_passphrase,
    wrap_umk_for, wrap_umk_with_passphrase, Key, RecoveryBlob,
};

/// This machine's long-lived X25519 keypair.
///
/// Stored beside the session rather than in an OS keychain because the daemon
/// this unlocks for is itself a background process reading `~/.mafold`. A
/// keychain that prompts would simply be bypassed by whoever runs the daemon
/// headless, which is most of them. The file is 0600 and the tradeoff is stated
/// in the design doc rather than hidden here.
#[derive(Serialize, Deserialize)]
pub struct DeviceKey {
    /// base64 X25519 secret scalar.
    pub secret: String,
    /// base64 X25519 public key.
    pub public: String,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn device_key_path() -> PathBuf {
    home().join(".mafold/device_key.json")
}

fn umk_cache_path() -> PathBuf {
    home().join(".mafold/vault_key")
}

/// Load this machine's keypair, generating it on first use.
pub fn device_key() -> Result<DeviceKey> {
    let path = device_key_path();
    if let Ok(s) = std::fs::read_to_string(&path) {
        if let Ok(k) = serde_json::from_str::<DeviceKey>(&s) {
            return Ok(k);
        }
        // A corrupt key file is not something to silently replace: every
        // connection wrapped for it would become unopenable, and the user would
        // see "locked" with no idea why.
        return Err(anyhow!(
            "{} is unreadable — move it aside and re-enrol this device \
             (`mafold connection devices`) if you have another device or your recovery passphrase",
            path.display()
        ));
    }
    let d = mafold_core::vault::generate_device();
    let k = DeviceKey { secret: d.secret, public: d.public };
    std::fs::create_dir_all(home().join(".mafold")).ok();
    std::fs::write(&path, serde_json::to_string_pretty(&k)?)
        .with_context(|| format!("write {}", path.display()))?;
    restrict(&path);
    Ok(k)
}

/// 0600 on Unix. On Windows the file inherits the user profile's ACL, which is
/// already user-only; there is no chmod to make and pretending otherwise with a
/// no-op wrapper would only obscure that.
#[cfg(unix)]
fn restrict(path: &PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
fn restrict(_path: &PathBuf) {}

/// Cache the opened UMK so a daemon doesn't re-do ECDH on every read.
///
/// This is a cache, not a second source of truth: it is sealed to the device
/// key, so deleting `device_key.json` invalidates it, and it never travels.
///
/// NOTE the web deliberately does NOT do this — a browser keeps the master key
/// in memory only. A long-lived daemon and a browser tab have different threat
/// models, and this is the one place they legitimately diverge.
pub fn cache_umk(umk: &Key, dev: &DeviceKey, key_id: &str) -> Result<()> {
    let wrapped = wrap_umk_for(&dev.public, umk).map_err(|e| anyhow!("{e}"))?;
    let path = umk_cache_path();
    std::fs::write(&path, format!("{key_id}\n{wrapped}"))
        .with_context(|| format!("write {}", path.display()))?;
    restrict(&path);
    Ok(())
}

/// The cached UMK plus the generation it belongs to, if present and openable.
pub fn cached_umk(dev: &DeviceKey) -> Option<(Key, String)> {
    let raw = std::fs::read_to_string(umk_cache_path()).ok()?;
    let (key_id, wrapped) = raw.split_once('\n')?;
    let key = unwrap_umk(&dev.secret, wrapped.trim()).ok()?;
    Some((key, key_id.trim().to_string()))
}

pub fn forget_cached_umk() {
    let _ = std::fs::remove_file(umk_cache_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cli and the web must agree byte-for-byte. This asserts the cli's
    /// re-exports really are the core's, so a future "quick local fix" here
    /// can't quietly fork the format.
    #[test]
    fn the_cli_uses_the_shared_hierarchy() {
        let d = mafold_core::vault::generate_device();
        let umk = Key::random();
        let wrapped = wrap_umk_for(&d.public, &umk).unwrap();
        // Opened through the CORE's function, sealed through the cli's re-export.
        let back = mafold_core::vault::unwrap_umk(&d.secret, &wrapped).unwrap();
        assert_eq!(back.0, umk.0);

        let sealed = seal_payload(&umk, r#"{"token":"t"}"#);
        assert_eq!(
            mafold_core::vault::open_payload(&umk, &sealed.blob, &sealed.wrapped_dek).unwrap(),
            r#"{"token":"t"}"#
        );
    }

    #[test]
    fn fingerprints_come_from_the_shared_impl() {
        let d = mafold_core::vault::generate_device();
        assert_eq!(fingerprint(&d.public), mafold_core::vault::fingerprint(&d.public));
    }
}
