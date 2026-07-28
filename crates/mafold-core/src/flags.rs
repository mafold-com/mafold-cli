//! Feature-flag engine — the ONE client-side implementation (.docs/feature-flags.md).
//!
//! A flag is born HERE: `KNOWN_FLAGS` is the single home of the key, the
//! compile-time default and the metadata (invariant ① — the server record is a
//! rollout override only, never a second default). Resolution precedence:
//! `local override > server value > compile default (dev or prod)`.
//!
//! Server values arrive as a `mafold_types::FlagState` (bootstrap `getFlags` +
//! WS `flagsChanged`) and are persisted to the core KV, so a cold offline start
//! resolves with the last delivered values (no flicker from run 2 on).
//!
//! Reactivity note (deliberate deviation from the doc's `subscribe(cb)`): every
//! mutator RETURNS the freshly-resolved `{key: bool}` snapshot, and the thin
//! per-platform wrapper (zustand on web, an ObservableObject on iOS) is the
//! change-notifier — the platforms' own reactive systems do this better than a
//! cross-FFI callback, and the engine stays identical everywhere.
//!
//! NOT a security boundary (invariant ②): anything sensitive is enforced
//! server-side regardless of what this engine resolves.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::storage::Storage;
use crate::store::Store;

/// Compile-time flag registry — key + default + metadata. One entry per flag;
/// delete the entry together with its gate code when the feature hits 100%.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct FlagMeta {
    pub key: &'static str,
    /// Production default (when neither an override nor a server value exists).
    pub default: bool,
    /// Default in dev/internal builds (`set_flags_dev(true)`) — preserves the
    /// old devmode.ts behavior of dev-only tabs without magic overrides.
    pub dev_default: bool,
    pub label: &'static str,
    pub description: &'static str,
    /// Who owns the flag's lifecycle (flags are temporary — see the doc §6).
    pub owner: &'static str,
    /// yyyy-mm added — lets a periodic check surface stale flags.
    pub added: &'static str,
}

/// The registry. Keys must match the server's control-plane records and the
/// gate code on every client.
pub static KNOWN_FLAGS: &[FlagMeta] = &[
    FlagMeta {
        key: "showIds",
        default: false,
        dev_default: false,
        label: "Show chat & message IDs",
        description: "Profiles show a copyable chat id; message menus show the message id.",
        owner: "ops",
        added: "2026-07",
    },
    FlagMeta {
        key: "gardenApps",
        default: false,
        dev_default: true,
        label: "Moments & Garden",
        description: "Reveals the bottom tab switcher (Moments / Chats / Garden) under the chat list.",
        owner: "ops",
        added: "2026-07",
    },
    FlagMeta {
        key: "moments",
        default: false,
        dev_default: true,
        label: "Moments",
        description: "The Moments tab (author feed + composer). WIP — gated separately from gardenApps so Garden can ship without exposing unfinished Moments.",
        owner: "ops",
        added: "2026-07",
    },
];

fn meta(key: &str) -> Option<&'static FlagMeta> {
    KNOWN_FLAGS.iter().find(|m| m.key == key)
}

const TBL: &str = "flags";
const K_SERVER: &str = "server";
const K_VERSION: &str = "version";
const K_OVERRIDES: &str = "overrides";

fn de_map(bytes: Option<Vec<u8>>) -> BTreeMap<String, bool> {
    bytes
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

impl<S: Storage> Store<S> {
    /// Mark this process as a dev/internal build — flips unset flags to their
    /// `dev_default` and (by convention, enforced in the client UI) unlocks the
    /// override toggles. Call once right after open; never persisted.
    pub fn set_flags_dev(&self, dev: bool) {
        self.flags_dev.store(dev, std::sync::atomic::Ordering::Relaxed);
    }

    fn flags_is_dev(&self) -> bool {
        self.flags_dev.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Apply a server `FlagState` (bootstrap or WS push). Stale deltas (version
    /// ≤ what we hold) are ignored. Returns the resolved snapshot either way.
    pub async fn flags_ingest(&self, state_json: &str) -> String {
        if let Ok(state) = serde_json::from_str::<mafold_types::FlagState>(state_json) {
            let _guard = self.write_lock.lock().await;
            let held: u64 = self
                .store
                .get(TBL, K_VERSION)
                .await
                .and_then(|b| String::from_utf8(b).ok())
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            // version 0 = "unversioned/first delivery" → always apply.
            if state.version == 0 || state.version > held {
                if let Ok(v) = serde_json::to_vec(&state.values) {
                    self.store.put(TBL, K_SERVER, v).await;
                }
                self.store
                    .put(TBL, K_VERSION, state.version.to_string().into_bytes())
                    .await;
            }
        }
        self.flags_resolved().await
    }

    /// Set (Some) or clear (None) a local dev override. Returns the resolved
    /// snapshot. The client UI must only expose this in dev/internal builds
    /// (invariant ④) — the engine keeps it available so internal accounts on
    /// prod builds can be unlocked deliberately.
    pub async fn flags_set_override(&self, key: &str, value: Option<bool>) -> String {
        {
            let _guard = self.write_lock.lock().await;
            let mut overrides = de_map(self.store.get(TBL, K_OVERRIDES).await);
            match value {
                Some(v) => {
                    overrides.insert(key.to_string(), v);
                }
                None => {
                    overrides.remove(key);
                }
            }
            if let Ok(v) = serde_json::to_vec(&overrides) {
                self.store.put(TBL, K_OVERRIDES, v).await;
            }
        }
        self.flags_resolved().await
    }

    /// The resolved `{key: bool}` snapshot — every known flag, plus any extra
    /// server/override keys (harmless: unknown keys have no gate code).
    pub async fn flags_resolved(&self) -> String {
        let server = de_map(self.store.get(TBL, K_SERVER).await);
        let overrides = de_map(self.store.get(TBL, K_OVERRIDES).await);
        let dev = self.flags_is_dev();

        let mut out: BTreeMap<String, bool> = BTreeMap::new();
        for m in KNOWN_FLAGS {
            out.insert(
                m.key.to_string(),
                if dev { m.dev_default } else { m.default },
            );
        }
        for (k, v) in &server {
            out.insert(k.clone(), *v);
        }
        for (k, v) in &overrides {
            out.insert(k.clone(), *v);
        }
        serde_json::to_string(&out).unwrap_or_else(|_| "{}".into())
    }

    /// One flag, resolved. Unknown key with no server/override value → false.
    pub async fn flags_enabled(&self, key: &str) -> bool {
        let overrides = de_map(self.store.get(TBL, K_OVERRIDES).await);
        if let Some(v) = overrides.get(key) {
            return *v;
        }
        let server = de_map(self.store.get(TBL, K_SERVER).await);
        if let Some(v) = server.get(key) {
            return *v;
        }
        meta(key)
            .map(|m| if self.flags_is_dev() { m.dev_default } else { m.default })
            .unwrap_or(false)
    }

    /// The local overrides map (for the dev-toggle UI's tri-state display).
    pub async fn flags_overrides(&self) -> String {
        let overrides = de_map(self.store.get(TBL, K_OVERRIDES).await);
        serde_json::to_string(&overrides).unwrap_or_else(|_| "{}".into())
    }
}

/// The registry as JSON (for clients to render the toggle list from).
pub fn registry_json() -> String {
    serde_json::to_string(KNOWN_FLAGS).unwrap_or_else(|_| "[]".into())
}

// ─────────────────────────────── tests ───────────────────────────────

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Minimal in-memory Storage for engine tests.
    struct MemStorage {
        map: Mutex<HashMap<(String, String), Vec<u8>>>,
    }
    impl MemStorage {
        fn new() -> Self {
            Self { map: Mutex::new(HashMap::new()) }
        }
    }
    impl Storage for MemStorage {
        async fn get(&self, t: &str, k: &str) -> Option<Vec<u8>> {
            self.map.lock().unwrap().get(&(t.into(), k.into())).cloned()
        }
        async fn put(&self, t: &str, k: &str, v: Vec<u8>) {
            self.map.lock().unwrap().insert((t.into(), k.into()), v);
        }
        async fn delete(&self, t: &str, k: &str) {
            self.map.lock().unwrap().remove(&(t.into(), k.into()));
        }
        async fn scan_prefix(&self, t: &str, prefix: &str) -> Vec<(String, Vec<u8>)> {
            let map = self.map.lock().unwrap();
            let mut out: Vec<(String, Vec<u8>)> = map
                .iter()
                .filter(|((tt, k), _)| tt == t && k.starts_with(prefix))
                .map(|((_, k), v)| (k.clone(), v.clone()))
                .collect();
            out.sort_by(|a, b| a.0.cmp(&b.0));
            out
        }
    }

    fn store() -> Store<MemStorage> {
        Store::new(MemStorage::new())
    }

    fn ingest(s: &Store<MemStorage>, values: &[(&str, bool)], version: u64) -> String {
        let state = mafold_types::FlagState {
            values: values.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
            version,
        };
        pollster::block_on(s.flags_ingest(&serde_json::to_string(&state).unwrap()))
    }

    fn enabled(s: &Store<MemStorage>, key: &str) -> bool {
        pollster::block_on(s.flags_enabled(key))
    }

    // Truth-table row 3: no override, no server value → compile default.
    #[test]
    fn default_applies() {
        let s = store();
        assert!(!enabled(&s, "gardenApps")); // prod default = false
        s.set_flags_dev(true);
        assert!(enabled(&s, "gardenApps")); // dev default = true
        assert!(!enabled(&s, "showIds")); // dev default = false
        assert!(!enabled(&s, "no-such-flag")); // unknown → false
    }

    // Truth-table row 2: server value beats the default.
    #[test]
    fn server_value_beats_default() {
        let s = store();
        ingest(&s, &[("gardenApps", true)], 1);
        assert!(enabled(&s, "gardenApps"));
        s.set_flags_dev(true);
        ingest(&s, &[("gardenApps", false)], 2);
        assert!(!enabled(&s, "gardenApps")); // server false beats dev_default true
    }

    // Truth-table row 1: local override beats everything.
    #[test]
    fn override_beats_server_and_default() {
        let s = store();
        ingest(&s, &[("showIds", false)], 1);
        pollster::block_on(s.flags_set_override("showIds", Some(true)));
        assert!(enabled(&s, "showIds"));
        // clearing the override falls back to the server value
        pollster::block_on(s.flags_set_override("showIds", None));
        assert!(!enabled(&s, "showIds"));
    }

    // Stale deltas (version ≤ held) are dropped.
    #[test]
    fn stale_version_dropped() {
        let s = store();
        ingest(&s, &[("gardenApps", true)], 5);
        ingest(&s, &[("gardenApps", false)], 3); // stale — ignored
        assert!(enabled(&s, "gardenApps"));
        ingest(&s, &[("gardenApps", false)], 6); // newer — applied
        assert!(!enabled(&s, "gardenApps"));
    }

    // The resolved snapshot unions registry + server + overrides.
    #[test]
    fn resolved_snapshot_shape() {
        let s = store();
        ingest(&s, &[("extraKey", true)], 1);
        pollster::block_on(s.flags_set_override("gardenApps", Some(true)));
        let json = pollster::block_on(s.flags_resolved());
        let map: BTreeMap<String, bool> = serde_json::from_str(&json).unwrap();
        assert_eq!(map.get("gardenApps"), Some(&true)); // override
        assert_eq!(map.get("extraKey"), Some(&true)); // server-only key carried
        assert_eq!(map.get("showIds"), Some(&false)); // registry default
    }

    #[test]
    fn registry_is_valid_json() {
        let metas: Vec<serde_json::Value> = serde_json::from_str(&registry_json()).unwrap();
        assert!(metas.iter().any(|m| m["key"] == "gardenApps"));
    }

    /// The dev-UI's tri-state contract: overrides start empty, reflect a set,
    /// and clear back to empty (None removes the key entirely).
    #[test]
    fn overrides_getter_reflects_set_and_clear() {
        let s = store();
        let get = |s: &Store<MemStorage>| -> serde_json::Value {
            serde_json::from_str(&pollster::block_on(s.flags_overrides())).unwrap()
        };
        assert_eq!(get(&s), serde_json::json!({}));
        pollster::block_on(s.flags_set_override("showIds", Some(true)));
        assert_eq!(get(&s), serde_json::json!({"showIds": true}));
        pollster::block_on(s.flags_set_override("showIds", None));
        assert_eq!(get(&s), serde_json::json!({}));
    }
}
