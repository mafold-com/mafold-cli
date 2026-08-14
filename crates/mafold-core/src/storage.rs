//! Data read/write layer — an async KV/document primitive, DECOUPLED from logic.
//! Maps trivially to SQLite (native) and IndexedDB (web). No queries here; all
//! ordering/filtering/dedup lives in `store.rs`, expressed over these four ops.
//! Async so the web backend can be IndexedDB (async-only); native SQLite is
//! synchronous and just returns ready futures.
#![allow(async_fn_in_trait)]

pub trait Storage {
    async fn get(&self, table: &str, key: &str) -> Option<Vec<u8>>;
    async fn put(&self, table: &str, key: &str, value: Vec<u8>);
    async fn delete(&self, table: &str, key: &str);
    /// (key, value) pairs under `table` whose key starts with `prefix`, key-sorted.
    async fn scan_prefix(&self, table: &str, prefix: &str) -> Vec<(String, Vec<u8>)>;
}

/// Shape version of the rebuildable client cache.
///
/// Native SQLite and browser IndexedDB must move together: both persist the
/// same opaque `CoreMessage.payload`, so a wire-shape change (for example the
/// file-id attachment cutover) invalidates both stores equally.
const SCHEMA_VERSION: u32 = 101;

// ───────────────────────── native: SQLite as a KV table ─────────────────────────
#[cfg(not(target_arch = "wasm32"))]
mod sqlite {
    use std::sync::Mutex;
    use rusqlite::{params, Connection};
    use super::Storage;
    use crate::CoreError;

    /// Bump to invalidate the local cache (the value JSON shape changed, etc.).
    /// The DB is a rebuildable cache: a mismatch DROPs `kv` and the network
    /// repopulates — no fragile in-place migrations.
    pub struct SqliteStore {
        conn: Mutex<Connection>,
    }

    impl SqliteStore {
        pub fn open(path: &str) -> Result<Self, CoreError> {
            let conn = Connection::open(path)?;
            conn.pragma_update(None, "journal_mode", "WAL").ok();
            let v: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0);
            if v != i64::from(super::SCHEMA_VERSION) {
                conn.execute("DROP TABLE IF EXISTS kv", []).ok();
                let _ = conn.execute_batch(&format!("PRAGMA user_version = {};", super::SCHEMA_VERSION));
            }
            conn.execute(
                "CREATE TABLE IF NOT EXISTS kv (tbl TEXT NOT NULL, k TEXT NOT NULL, v BLOB NOT NULL, PRIMARY KEY (tbl, k))",
                [],
            )?;
            Ok(Self { conn: Mutex::new(conn) })
        }
    }

    // No await is held across the lock — each op runs to completion synchronously,
    // so the MutexGuard is dropped before the (already-ready) future resolves.
    impl Storage for SqliteStore {
        async fn get(&self, t: &str, k: &str) -> Option<Vec<u8>> {
            self.conn.lock().unwrap()
                .query_row("SELECT v FROM kv WHERE tbl=?1 AND k=?2", params![t, k], |r| r.get(0))
                .ok()
        }
        async fn put(&self, t: &str, k: &str, v: Vec<u8>) {
            let _ = self.conn.lock().unwrap().execute(
                "INSERT INTO kv(tbl,k,v) VALUES(?1,?2,?3) ON CONFLICT(tbl,k) DO UPDATE SET v=?3",
                params![t, k, v],
            );
        }
        async fn delete(&self, t: &str, k: &str) {
            let _ = self.conn.lock().unwrap()
                .execute("DELETE FROM kv WHERE tbl=?1 AND k=?2", params![t, k]);
        }
        async fn scan_prefix(&self, t: &str, p: &str) -> Vec<(String, Vec<u8>)> {
            let conn = self.conn.lock().unwrap();
            let hi = format!("{p}\u{10FFFF}"); // half-open upper bound for the prefix
            let mut stmt = match conn.prepare("SELECT k,v FROM kv WHERE tbl=?1 AND k>=?2 AND k<?3 ORDER BY k") {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let rows = match stmt.query_map(params![t, p, hi], |r| Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))) {
                Ok(r) => r,
                Err(_) => return Vec::new(),
            };
            rows.filter_map(Result::ok).collect()
        }
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub use sqlite::SqliteStore;

// ───────────────────────── web: in-memory (IndexedDB impl lands next) ─────────────
#[cfg(target_arch = "wasm32")]
#[allow(dead_code)] // kept as an ephemeral/test backend; IdbStore is the default
mod mem {
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use super::Storage;
    #[derive(Default)]
    pub struct MemStore {
        data: Mutex<BTreeMap<(String, String), Vec<u8>>>,
    }
    impl Storage for MemStore {
        async fn get(&self, t: &str, k: &str) -> Option<Vec<u8>> {
            self.data.lock().unwrap().get(&(t.into(), k.into())).cloned()
        }
        async fn put(&self, t: &str, k: &str, v: Vec<u8>) {
            self.data.lock().unwrap().insert((t.into(), k.into()), v);
        }
        async fn delete(&self, t: &str, k: &str) {
            self.data.lock().unwrap().remove(&(t.into(), k.into()));
        }
        async fn scan_prefix(&self, t: &str, p: &str) -> Vec<(String, Vec<u8>)> {
            self.data.lock().unwrap().iter()
                .filter(|((tt, k), _)| tt == t && k.starts_with(p))
                .map(|((_, k), v)| (k.clone(), v.clone())).collect()
        }
    }
}
#[cfg(target_arch = "wasm32")]
#[allow(unused_imports)] // kept as an ephemeral/test backend; IdbStore is the default
pub use mem::MemStore;

// ───────────────────────── web: IndexedDB (real persistence) ─────────────────────────
#[cfg(target_arch = "wasm32")]
mod idb_store {
    use idb::{Database, DatabaseEvent, Factory, KeyRange, ObjectStoreParams, Query, TransactionMode};
    use js_sys::Uint8Array;
    use wasm_bindgen::JsValue;
    use super::{Storage, SCHEMA_VERSION};

    const STORE: &str = "kv";
    const SEP: char = '\u{1f}'; // unit separator joining {table}{SEP}{key} into one IDB key

    fn ck(t: &str, k: &str) -> String { format!("{t}{SEP}{k}") }
    fn bytes_to_js(v: &[u8]) -> JsValue {
        let arr = Uint8Array::new_with_length(v.len() as u32);
        arr.copy_from(v);
        arr.into()
    }
    fn js_to_bytes(v: &JsValue) -> Vec<u8> { Uint8Array::new(v).to_vec() }

    /// One IndexedDB object store ("kv") with out-of-line string keys — the same
    /// KV shape SqliteStore exposes, so the portable `Store` logic is identical.
    pub struct IdbStore { db: Database }

    impl IdbStore {
        pub async fn open(name: &str) -> Result<Self, String> {
            let factory = Factory::new().map_err(|e| e.to_string())?;
            let mut req = factory.open(name, Some(SCHEMA_VERSION)).map_err(|e| e.to_string())?;
            req.on_upgrade_needed(|event| {
                if let Ok(db) = event.database() {
                    // This database is only a network-rebuildable cache. Keeping
                    // opaque payloads across a schema bump is actively unsafe:
                    // old attachment JSON reached new React code after the
                    // file-id cutover and crashed the whole chat timeline.
                    if db.store_names().iter().any(|n| n == STORE) {
                        let _ = db.delete_object_store(STORE);
                    }
                    let _ = db.create_object_store(STORE, ObjectStoreParams::new());
                }
            });
            let mut db = req.await.map_err(|e| e.to_string())?;
            // Never make another tab's future schema upgrade wait forever on
            // this connection. The next access reopens the rebuildable cache.
            db.on_version_change(|event| {
                if let Ok(db) = event.database() { db.close(); }
            });
            Ok(Self { db })
        }
    }

    impl Storage for IdbStore {
        async fn get(&self, t: &str, k: &str) -> Option<Vec<u8>> {
            let tx = self.db.transaction(&[STORE], TransactionMode::ReadOnly).ok()?;
            let store = tx.object_store(STORE).ok()?;
            let v: Option<JsValue> = store.get(JsValue::from_str(&ck(t, k))).ok()?.await.ok()?;
            v.as_ref().map(js_to_bytes) // tx stays alive until end of scope
        }
        async fn put(&self, t: &str, k: &str, v: Vec<u8>) {
            if let Ok(tx) = self.db.transaction(&[STORE], TransactionMode::ReadWrite) {
                if let Ok(store) = tx.object_store(STORE) {
                    if let Ok(req) = store.put(&bytes_to_js(&v), Some(&JsValue::from_str(&ck(t, k)))) {
                        let _ = req.await;
                    }
                }
                if let Ok(c) = tx.commit() { let _ = c.await; }
            }
        }
        async fn delete(&self, t: &str, k: &str) {
            if let Ok(tx) = self.db.transaction(&[STORE], TransactionMode::ReadWrite) {
                if let Ok(store) = tx.object_store(STORE) {
                    if let Ok(req) = store.delete(JsValue::from_str(&ck(t, k))) { let _ = req.await; }
                }
                if let Ok(c) = tx.commit() { let _ = c.await; }
            }
        }
        async fn scan_prefix(&self, t: &str, p: &str) -> Vec<(String, Vec<u8>)> {
            let Ok(tx) = self.db.transaction(&[STORE], TransactionMode::ReadOnly) else { return Vec::new() };
            let Ok(store) = tx.object_store(STORE) else { return Vec::new() };
            let lo = JsValue::from_str(&format!("{t}{SEP}{p}"));
            let hi = JsValue::from_str(&format!("{t}{SEP}{p}\u{10FFFF}"));
            let range = |()| KeyRange::bound(&lo, &hi, Some(false), Some(true)).ok();
            let (Some(r1), Some(r2)) = (range(()), range(())) else { return Vec::new() };
            let keys = match store.get_all_keys(Some(Query::KeyRange(r1)), None) {
                Ok(req) => req.await.unwrap_or_default(),
                Err(_) => return Vec::new(),
            };
            let vals = match store.get_all(Some(Query::KeyRange(r2)), None) {
                Ok(req) => req.await.unwrap_or_default(),
                Err(_) => return Vec::new(),
            };
            let strip = format!("{t}{SEP}");
            keys.into_iter().zip(vals.iter()).filter_map(|(kjs, vjs)| {
                let full = kjs.as_string()?;
                let key = full.strip_prefix(&strip)?.to_string();
                Some((key, js_to_bytes(vjs)))
            }).collect()
        }
    }
}
#[cfg(target_arch = "wasm32")]
pub use idb_store::IdbStore;

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::{SqliteStore, Storage};

    #[test]
    fn sqlite_kv_roundtrip_memory() {
        let s = SqliteStore::open(":memory:").unwrap();
        pollster::block_on(async {
            assert_eq!(s.get("a", "k").await, None);
            s.put("a", "k", b"v1".to_vec()).await;
            assert_eq!(s.get("a", "k").await.as_deref(), Some(b"v1".as_ref()));
            s.put("a", "k", b"v2".to_vec()).await; // upsert overwrites
            assert_eq!(s.get("a", "k").await.as_deref(), Some(b"v2".as_ref()));
            // Table scoping: same key under another table is invisible.
            assert_eq!(s.get("b", "k").await, None);
            s.delete("a", "k").await;
            assert_eq!(s.get("a", "k").await, None);
        });
    }

    #[test]
    fn sqlite_scan_prefix_sorted_and_bounded() {
        let s = SqliteStore::open(":memory:").unwrap();
        pollster::block_on(async {
            // Insert OUT of order — scan must come back key-sorted (the message
            // timeline's time-order depends on this).
            for k in ["c1|003", "c1|001", "c2|001", "c1|002"] {
                s.put("msg", k, k.as_bytes().to_vec()).await;
            }
            let keys: Vec<String> = s.scan_prefix("msg", "c1|").await.into_iter().map(|(k, _)| k).collect();
            assert_eq!(keys, vec!["c1|001", "c1|002", "c1|003"], "sorted + c2 excluded");

            // High-unicode keys inside the prefix ARE included…
            s.put("msg", "c1|\u{10FFFE}", vec![1]).await;
            assert!(s.scan_prefix("msg", "c1|").await.iter().any(|(k, _)| k == "c1|\u{10FFFE}"));
            // …but the pathological key EXACTLY prefix+U+10FFFF sits ON the
            // half-open upper bound and is invisible. Documented contract —
            // real keys are uuid/ts based and can never hit it.
            s.put("msg", "c1|\u{10FFFF}", vec![2]).await;
            assert!(!s.scan_prefix("msg", "c1|").await.iter().any(|(k, _)| k == "c1|\u{10FFFF}"));
        });
    }

    #[test]
    fn sqlite_empty_prefix_scans_whole_table_only() {
        let s = SqliteStore::open(":memory:").unwrap();
        pollster::block_on(async {
            s.put("t1", "a", vec![1]).await;
            s.put("t1", "b", vec![2]).await;
            s.put("t2", "c", vec![3]).await;
            let keys: Vec<String> = s.scan_prefix("t1", "").await.into_iter().map(|(k, _)| k).collect();
            assert_eq!(keys, vec!["a", "b"], "all rows of t1, none of t2");
        });
    }

    /// The DB is a rebuildable CACHE: a schema-version mismatch must DROP kv
    /// (no fragile in-place migrations) and restamp the version.
    #[test]
    fn sqlite_schema_mismatch_drops_kv() {
        let path = std::env::temp_dir().join(format!(
            "mafold-core-schema-test-{}-{}.sqlite",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        ));
        let path_s = path.to_str().unwrap().to_string();

        let s = SqliteStore::open(&path_s).unwrap();
        pollster::block_on(s.put("t", "k", b"old".to_vec()));
        drop(s);

        // Simulate an OLD app version's cache: rewind user_version.
        {
            let conn = rusqlite::Connection::open(&path_s).unwrap();
            conn.execute_batch("PRAGMA user_version = 1;").unwrap();
        }

        let s = SqliteStore::open(&path_s).unwrap();
        pollster::block_on(async {
            assert_eq!(s.get("t", "k").await, None, "mismatched schema must drop the kv cache");
            // And the store works normally after the rebuild.
            s.put("t", "k", b"new".to_vec()).await;
            assert_eq!(s.get("t", "k").await.as_deref(), Some(b"new".as_ref()));
        });
        drop(s);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(format!("{path_s}-wal"));
        let _ = std::fs::remove_file(format!("{path_s}-shm"));
    }
}
