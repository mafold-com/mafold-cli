//! The provider registry, as delivered by the cloud and held by this process.
//!
//! WHY IT IS NOT A CONST ANY MORE. Calling a connection needs exactly three
//! things the caller cannot know: where to send (`mcp_url`), how the credential
//! rides (`auth`), and whether a native driver handles it (`native_api`).
//! Compiling those into every client made "add a provider" mean shipping five
//! apps — and until all five shipped, a connection a user had just linked read
//! as `unknown` on their own laptop. The pack is now served, exactly like the
//! language packs, so a new MCP provider reaches every surface on the push that
//! adds it.
//!
//! **There is no bundled baseline.** A fresh process has no registry until one
//! arrives, and [`loaded`] says so rather than letting a host paint an empty
//! Connections pane as though the account had none. That is the same contract
//! `Store::langpack_loaded` already carries, for the same reason.
//!
//! ## The part that is a security boundary, not a cache
//!
//! A pack says where a decrypted credential gets sent and under which header.
//! A server that could rewrite it would not need to open the vault — it could
//! simply tell every device to post the contents somewhere else. So a pack is
//! **verified before it is installed** ([`install`]), and an unverifiable one is
//! discarded whole rather than partially applied. The verifying key is compiled
//! in; the registry is not. A key is not a table, and this is the distinction
//! that lets "no bundled baseline" coexist with the vault's promise.

use std::sync::RwLock;

use mafold_types::connections::{
    providers_checksum, providers_digest, ProviderInfo, PACK_PUBLIC_KEY_B64,
};

/// How long a fetched pack is trusted before a refetch is attempted. Short
/// enough that a published fix reaches a long-running daemon the same hour,
/// long enough that a chatty client is not asking on every call.
const TTL_MS: i64 = 15 * 60 * 1000;

pub struct Pack {
    pub version: u32,
    pub providers: Vec<ProviderInfo>,
    /// When this process fetched it — NOT when the server published it, which
    /// is `version`. Only used for the refetch timer.
    pub fetched_at: i64,
}

fn cache() -> &'static RwLock<Option<Pack>> {
    static CACHE: RwLock<Option<Pack>> = RwLock::new(None);
    &CACHE
}

fn read<T>(f: impl FnOnce(Option<&Pack>) -> T) -> T {
    let guard = cache().read().unwrap_or_else(|e| e.into_inner());
    f(guard.as_ref())
}

/// Whether a registry has landed. A host that paints the Connections pane
/// before this is true is painting "you have no providers", which is a
/// statement about the network dressed up as a statement about the account.
pub fn loaded() -> bool {
    read(|p| p.is_some_and(|p| !p.providers.is_empty()))
}

/// One provider, or `None` when this process has no pack yet OR the pack has no
/// such row. Callers must distinguish those two with [`loaded`]: "we don't know
/// yet" and "no such provider" deserve different sentences.
pub fn get(id: &str) -> Option<ProviderInfo> {
    read(|p| p?.providers.iter().find(|x| x.id == id).cloned())
}

pub fn all() -> Vec<ProviderInfo> {
    read(|p| p.map(|p| p.providers.clone()).unwrap_or_default())
}

pub fn version() -> u32 {
    read(|p| p.map_or(0, |p| p.version))
}

/// The digest of what we hold, for asking the server "still current?".
pub fn checksum() -> String {
    read(|p| p.map(|p| providers_checksum(&p.providers)).unwrap_or_default())
}

fn fresh(now: i64) -> bool {
    read(|p| p.is_some_and(|p| now - p.fetched_at < TTL_MS))
}

/// Adopt a pack. **The only way one enters this process**, so it is the one
/// place a signature check has to live.
///
/// Rejecting is deliberately total: a pack that fails verification leaves the
/// previous one in place rather than being merged row by row. A half-applied
/// registry is one where some providers route correctly and others do not,
/// which is worse than an out-of-date one and much harder to see.
pub fn install(
    version: u32,
    providers: Vec<ProviderInfo>,
    signature_b64: &str,
    now: i64,
) -> Result<(), String> {
    if providers.is_empty() {
        return Err("provider pack is empty — refusing to replace a working registry".into());
    }
    verify(version, &providers, signature_b64)?;
    let mut guard = cache().write().unwrap_or_else(|e| e.into_inner());
    *guard = Some(Pack { version, providers, fetched_at: now });
    Ok(())
}

/// Where a test can stand in its own keypair, so the tests below exercise the
/// REAL verification path with real signatures instead of a bypass. Never
/// settable outside `cfg(test)`.
#[cfg(test)]
static TEST_KEY: RwLock<Option<String>> = RwLock::new(None);

fn verifying_key_b64() -> String {
    #[cfg(test)]
    if let Some(k) = TEST_KEY.read().unwrap_or_else(|e| e.into_inner()).clone() {
        return k;
    }
    PACK_PUBLIC_KEY_B64.to_string()
}

/// Check a pack against the compiled-in key.
///
/// Every failure is one sentence about the SIGNATURE, never about the pack's
/// contents: a pack that does not verify is not a pack we then inspect for
/// plausibility. Half-trusting one is how a single altered row gets through.
fn verify(version: u32, providers: &[ProviderInfo], signature_b64: &str) -> Result<(), String> {
    use base64::Engine as _;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let key_bytes: [u8; 32] = base64::engine::general_purpose::STANDARD
        .decode(verifying_key_b64())
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or("the compiled-in pack key is malformed — this is a build problem, not a server one")?;
    let key = VerifyingKey::from_bytes(&key_bytes).map_err(|e| format!("bad pack key: {e}"))?;

    let sig_bytes: [u8; 64] = base64::engine::general_purpose::STANDARD
        .decode(signature_b64)
        .ok()
        .and_then(|v| v.try_into().ok())
        .ok_or("provider pack arrived without a usable signature — refusing it")?;

    key.verify(&providers_digest(version, providers), &Signature::from_bytes(&sig_bytes))
        .map_err(|_| {
            "provider pack failed signature check — refusing it. A pack decides where your \
             credentials are sent, so an unverified one is never partially applied"
                .to_string()
        })
}

/// Drop the pack. Tests only — a running client has no reason to forget one.
#[cfg(test)]
pub fn forget() {
    *cache().write().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Seat a registry WITHOUT verifying it. Tests only — the name is deliberately
/// unpleasant so it cannot be reached for by accident.
///
/// Public rather than `cfg(test)` because the cli's tests live in another
/// crate, and the alternative was making their fixtures carry a real signing
/// key. The verification path is exercised for real by this module's own tests,
/// which sign with a test key and assert that tampering is refused.
#[doc(hidden)]
pub fn install_unverified_for_tests(version: u32, providers: Vec<ProviderInfo>, now: i64) {
    *cache().write().unwrap_or_else(|e| e.into_inner()) =
        Some(Pack { version, providers, fetched_at: now });
}

/// Server answer to `getConnectionProviders`.
#[derive(serde::Deserialize)]
struct PackResp {
    #[serde(default)]
    unchanged: bool,
    #[serde(default)]
    version: u32,
    #[serde(default)]
    providers: Vec<ProviderInfo>,
    #[serde(default)]
    signature: String,
}

/// Make sure this process has a registry, fetching one if it has none or the
/// one it has is stale.
///
/// Returns `Ok` when a usable pack is in hand, INCLUDING when the network
/// failed but a previous pack is still cached — a daemon that has been running
/// for a week should not lose the ability to make calls because one refresh
/// timed out. It errors only when there is nothing to work with, which is the
/// case a caller must surface rather than paper over.
pub async fn ensure(base: &str, token: &str, now: i64) -> Result<(), String> {
    if fresh(now) {
        return Ok(());
    }
    let body = serde_json::json!({ "known_checksum": checksum() }).to_string();
    match crate::net::rpc(base, token, "getConnectionProviders", &body).await {
        Ok(text) => {
            let resp: PackResp = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            if resp.unchanged {
                // Still current: restamp so we don't re-ask every call.
                let mut guard = cache().write().unwrap_or_else(|e| e.into_inner());
                if let Some(p) = guard.as_mut() {
                    p.fetched_at = now;
                }
                return Ok(());
            }
            install(resp.version, resp.providers, &resp.signature, now)
        }
        Err(e) if loaded() => {
            // Keep serving what we have. The failure is real, but it is a
            // refresh failure, and turning it into a call failure would make
            // every connection on the device depend on the api being reachable
            // at that instant.
            let _ = e;
            Ok(())
        }
        Err(e) => Err(format!("no provider registry yet, and fetching one failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mafold_types::connections::provider_infos;

    use base64::Engine as _;
    use ed25519_dalek::{Signer, SigningKey};

    /// The registry is process-global, so these run under one lock rather than
    /// racing each other through `cache()`.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static M: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let g = M.lock().unwrap_or_else(|e| e.into_inner());
        // Stand in the test keypair for every test that holds the lock, so
        // `install` runs its real signature check rather than a bypass.
        *TEST_KEY.write().unwrap_or_else(|e| e.into_inner()) =
            Some(b64(signing_key().verifying_key().as_bytes()));
        g
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    /// A fixed seed: these tests must not depend on entropy, and the key is
    /// worthless outside them.
    fn signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    /// Sign a pack the way `publish-providers.yml` will.
    fn sign(version: u32, providers: &[ProviderInfo]) -> String {
        b64(&signing_key().sign(&providers_digest(version, providers)).to_bytes())
    }

    fn install_signed(version: u32, providers: Vec<ProviderInfo>, now: i64) -> Result<(), String> {
        let sig = sign(version, &providers);
        install(version, providers, &sig, now)
    }

    #[test]
    fn a_fresh_process_knows_nothing_and_says_so() {
        let _g = guard();
        forget();
        assert!(!loaded(), "no pack ⇒ hosts must not paint");
        assert!(get("notion").is_none());
        assert!(all().is_empty());
        assert_eq!(checksum(), "");
    }

    #[test]
    fn installing_makes_providers_resolvable() {
        let _g = guard();
        forget();
        install_signed(7, provider_infos(), 1_000).unwrap();
        assert!(loaded());
        assert_eq!(version(), 7);
        let n = get("notion").expect("installed pack resolves");
        assert_eq!(n.auth.header, "Authorization");
        let f = get("figma").expect("installed pack resolves");
        assert_eq!(f.auth.header, "X-Figma-Token", "the pack must carry the odd one, not guess it");
        forget();
    }

    /// An empty pack is the shape a broken publish takes. Adopting it would
    /// silently unlink every connection on the device.
    #[test]
    fn an_empty_pack_never_replaces_a_working_one() {
        let _g = guard();
        forget();
        install_signed(1, provider_infos(), 0).unwrap();
        assert!(install_signed(2, vec![], 1).is_err());
        assert!(loaded(), "the old pack survives a bad publish");
        assert_eq!(version(), 1);
        forget();
    }

    /// The attack this whole mechanism exists for: a server that serves a pack
    /// pointing a provider's traffic somewhere else. It must not install, and
    /// the previous registry must survive intact.
    #[test]
    fn a_repointed_provider_is_refused_and_changes_nothing() {
        let _g = guard();
        forget();
        install_signed(1, provider_infos(), 0).unwrap();
        let good = get("notion").unwrap().mcp_url.clone();

        let mut tampered = provider_infos();
        let sig = sign(2, &tampered); // signed BEFORE the edit — as an attacker
        for p in tampered.iter_mut() {
            p.mcp_url = Some("https://evil.example/mcp".into());
        }
        let err = install(2, tampered, &sig, 1).unwrap_err();
        assert!(err.contains("signature"), "{err}");

        assert_eq!(get("notion").unwrap().mcp_url, good, "the working registry survived");
        assert_eq!(version(), 1);
        forget();
    }

    /// A pack with no signature at all is not "unsigned but probably fine".
    #[test]
    fn an_unsigned_pack_is_refused() {
        let _g = guard();
        forget();
        assert!(install(1, provider_infos(), "", 0).is_err());
        assert!(!loaded());
    }

    /// A signature is over (version, pack) — so a real old signature cannot be
    /// replayed to make an older registry look like a newer one.
    #[test]
    fn a_signature_does_not_travel_between_versions() {
        let _g = guard();
        forget();
        let packs = provider_infos();
        let sig_v1 = sign(1, &packs);
        assert!(install(2, packs.clone(), &sig_v1, 0).is_err(), "v1's signature must not pass as v2");
        assert!(install(1, packs, &sig_v1, 0).is_ok());
        forget();
    }

    #[test]
    fn freshness_expires_so_a_long_lived_daemon_picks_up_a_publish() {
        let _g = guard();
        forget();
        install_signed(1, provider_infos(), 0).unwrap();
        assert!(fresh(TTL_MS - 1));
        assert!(!fresh(TTL_MS + 1), "past the TTL a refetch must be attempted");
        forget();
    }
}
