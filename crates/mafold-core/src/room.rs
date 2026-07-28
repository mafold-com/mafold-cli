//! Shared-room CRDT engine (native / iOS via UniFFI). A ROOM = (conversation,
//! app) → one Automerge document; a `Counter` key sums concurrent increments.
//!
//! The whole point: iOS runs automerge-rs here, the web runs automerge-wasm, and
//! they converge because they speak the SAME change wire-format through the dumb
//! relay — NOT a shared core. The test below proves that direction concretely by
//! applying REAL change blobs the web client (@automerge/automerge 3.2.6)
//! produced in production and reading the converged value. See
//! .docs/shared-room-crdt-v0.md (this is the P0 engine).

use automerge::transaction::Transactable;
use automerge::{ActorId, AutoCommit, ObjId, ObjType, Prop, ReadDoc, ScalarValue, Value, ROOT};
use base64::{engine::general_purpose::STANDARD, Engine};
use std::sync::{Arc, Mutex, MutexGuard};

/// Read an integer/counter value at a root key (0 if absent or other type).
fn read_counter<D: ReadDoc>(doc: &D, key: &str) -> i64 {
    match doc.get(&ROOT, key) {
        Ok(Some((Value::Scalar(s), _))) => match s.as_ref() {
            ScalarValue::Counter(c) => c.into(),
            ScalarValue::Int(i) => *i,
            // A peer-supplied uint ≥ 2^63 must NOT wrap to a negative i64 (it would
            // converge wrong across devices); saturate instead.
            ScalarValue::Uint(u) => i64::try_from(*u).unwrap_or(i64::MAX),
            _ => 0,
        },
        _ => 0,
    }
}

/// One Automerge scalar → serde_json. A `Counter` surfaces as its integer value.
fn scalar_to_json(s: &ScalarValue) -> serde_json::Value {
    match s {
        ScalarValue::Str(st) => serde_json::Value::String(st.to_string()),
        ScalarValue::Int(i) => serde_json::json!(*i),
        ScalarValue::Uint(u) => serde_json::json!(*u),
        ScalarValue::F64(f) => serde_json::json!(*f),
        ScalarValue::Counter(c) => serde_json::json!(i64::from(c)),
        ScalarValue::Boolean(b) => serde_json::json!(*b),
        _ => serde_json::Value::Null,
    }
}

/// Any Automerge value (scalar OR nested Map/List) at `id` → serde_json,
/// recursively. This is why a room whose root keys hold objects/arrays — e.g.
/// Garden's `folders` LIST and each `item:<id>` MAP — round-trips to real JSON
/// instead of silently vanishing (the old scalar-only reader dropped them). Text
/// objects (unused by Garden) surface as null rather than crash.
/// Hard depth cap for the read-side recursion. Garden's real data is 3–4 levels
/// deep; 128 fully covers every legit shape while stopping a maliciously deep
/// relayed change from blowing the stack. A Rust stack overflow ABORTS the
/// process (SIGABRT) — it does not unwind, so the `lock()` mutex-poison recovery
/// can't catch it. This read path is NOT gated by serde_json's own depth limit
/// (data comes straight from the Automerge doc, not a parsed JSON string), so the
/// cap must live here — mirroring `apply_remote`'s adversarial-relayed posture.
const MAX_JSON_DEPTH: usize = 128;

fn value_to_json<D: ReadDoc>(doc: &D, val: &Value, id: &ObjId, depth: usize) -> serde_json::Value {
    if depth >= MAX_JSON_DEPTH {
        eprintln!("room: value_to_json depth cap ({MAX_JSON_DEPTH}) hit — truncating to null (adversarial nested relayed change?)");
        return serde_json::Value::Null;
    }
    match val {
        Value::Scalar(s) => scalar_to_json(s.as_ref()),
        Value::Object(ObjType::Map) => {
            let mut m = serde_json::Map::new();
            for k in doc.keys(id) {
                if let Ok(Some((v, cid))) = doc.get(id, k.as_str()) {
                    m.insert(k, value_to_json(doc, &v, &cid, depth + 1));
                }
            }
            serde_json::Value::Object(m)
        }
        Value::Object(ObjType::List) => {
            let n = doc.length(id);
            let mut arr = Vec::with_capacity(n);
            for i in 0..n {
                if let Ok(Some((v, cid))) = doc.get(id, i) {
                    arr.push(value_to_json(doc, &v, &cid, depth + 1));
                }
            }
            serde_json::Value::Array(arr)
        }
        Value::Object(_) => serde_json::Value::Null,
    }
}

/// serde_json leaf → the matching Automerge `ScalarValue`. Objects/arrays are
/// NOT scalars (they go through `put_object`/`insert_object`), so they map to None.
fn json_scalar(v: &serde_json::Value) -> Option<ScalarValue> {
    match v {
        serde_json::Value::String(s) => Some(s.as_str().into()),
        serde_json::Value::Bool(b) => Some((*b).into()),
        serde_json::Value::Null => Some(ScalarValue::Null),
        serde_json::Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => Some(i.into()),
            (_, Some(f)) => Some(f.into()),
            _ => None,
        },
        _ => None,
    }
}

/// Write a JSON value under map-key/`prop` of `obj`, recursively materializing
/// nested objects/arrays as Automerge Map/List. `put_object` installs a FRESH
/// container over any prior value → whole-key last-writer-wins, matching the web
/// app's `room.change(d => { d[key] = value })`.
fn write_json<T: Transactable>(tx: &mut T, obj: &ObjId, prop: impl Into<Prop>, v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Object(map) => match tx.put_object(obj, prop, ObjType::Map) {
            Ok(child) => map.iter().all(|(k, cv)| write_json(tx, &child, k.as_str(), cv)),
            Err(_) => false,
        },
        serde_json::Value::Array(items) => match tx.put_object(obj, prop, ObjType::List) {
            Ok(child) => items.iter().enumerate().all(|(i, cv)| insert_json(tx, &child, i, cv)),
            Err(_) => false,
        },
        _ => match json_scalar(v) {
            Some(sv) => tx.put(obj, prop, sv).is_ok(),
            None => false,
        },
    }
}

/// List variant of `write_json`: elements are INSERTED at their index (a list is
/// built, not keyed).
fn insert_json<T: Transactable>(tx: &mut T, list: &ObjId, i: usize, v: &serde_json::Value) -> bool {
    match v {
        serde_json::Value::Object(map) => match tx.insert_object(list, i, ObjType::Map) {
            Ok(child) => map.iter().all(|(k, cv)| write_json(tx, &child, k.as_str(), cv)),
            Err(_) => false,
        },
        serde_json::Value::Array(items) => match tx.insert_object(list, i, ObjType::List) {
            Ok(child) => items.iter().enumerate().all(|(j, cv)| insert_json(tx, &child, j, cv)),
            Err(_) => false,
        },
        _ => match json_scalar(v) {
            Some(sv) => tx.insert(list, i, sv).is_ok(),
            None => false,
        },
    }
}

/// One open shared room: an Automerge doc behind a lock. iOS holds an `Arc<Room>`
/// (UniFFI object); it mutates locally (instant), hands the resulting change bytes
/// to the host to relay, and applies relayed changes back — converging with web.
#[cfg_attr(not(target_arch = "wasm32"), derive(uniffi::Object))]
pub struct Room {
    doc: Mutex<AutoCommit>,
}

impl Room {
    /// Lock the doc, RECOVERING the guard if the mutex was poisoned. Automerge can
    /// panic while parsing an adversarial relayed change (in `apply_remote`) while
    /// holding this lock; a plain `.unwrap()` would then make EVERY subsequent
    /// `Room` call panic across the UniFFI boundary (a permanent crash until app
    /// restart). The recovered doc may be mid-mutation, but Automerge's transaction
    /// boundaries keep it usable, and the alternative — bricking the room — is worse.
    fn lock(&self) -> MutexGuard<'_, AutoCommit> {
        self.doc.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg_attr(not(target_arch = "wasm32"), uniffi::export)]
impl Room {
    /// Open a room: load a prior snapshot (full doc bytes from `snapshot()`) if
    /// present, else start fresh. `actor_hex` = the per-DEVICE id (even-length
    /// hex), so a user's two devices don't clobber each other.
    ///
    /// A CORRUPT snapshot is NOT silently treated as an empty room (which would
    /// look like a clean open while quietly dropping the user's converged state):
    /// the failure is logged loudly so it's diagnosable, and the room falls back
    /// to fresh — the FFI signature stays infallible so the host is unchanged.
    #[cfg_attr(not(target_arch = "wasm32"), uniffi::constructor)]
    pub fn open(snapshot: Option<Vec<u8>>, actor_hex: String) -> Arc<Room> {
        let mut doc = match snapshot {
            Some(bytes) => match AutoCommit::load(&bytes) {
                Ok(doc) => doc,
                Err(e) => {
                    // Distinguish corrupt-vs-empty: a present-but-undecodable
                    // snapshot is data loss, not a fresh room. Surface it.
                    eprintln!("mafold-core: CORRUPT room snapshot ({} bytes), starting fresh: {e}", bytes.len());
                    AutoCommit::new()
                }
            },
            None => AutoCommit::new(),
        };
        if let Ok(actor) = ActorId::try_from(actor_hex.as_str()) {
            doc.set_actor(actor);
        }
        // Drop the post-load delta so the next `save_incremental()` only carries
        // genuinely new local changes.
        let _ = doc.save_incremental();
        Arc::new(Room { doc: Mutex::new(doc) })
    }

    /// Increment a root `Counter` by `by` (concurrent increments sum). Returns the
    /// new change as base64 for the host to relay (empty string on no-op).
    pub fn increment_counter(&self, key: String, by: i64) -> String {
        let mut doc = self.lock();
        let is_counter = matches!(
            doc.get(&ROOT, &key),
            Ok(Some((Value::Scalar(s), _))) if matches!(s.as_ref(), ScalarValue::Counter(_))
        );
        if !is_counter {
            let _ = doc.put(&ROOT, &key, ScalarValue::Counter(0i64.into()));
        }
        if doc.increment(&ROOT, &key, by).is_err() {
            return String::new();
        }
        STANDARD.encode(doc.save_incremental())
    }

    /// Apply a base64 change (or snapshot) relayed from another participant.
    /// Idempotent — a duplicate/echoed change is a no-op. Returns whether the doc
    /// actually changed. A change that is REJECTED (bad base64 / undecodable
    /// automerge — e.g. an adversarial relayed blob) is logged loudly so it's
    /// distinguishable from a legitimate no-op (both still return `false`, keeping
    /// the FFI signature unchanged for the host).
    pub fn apply_remote(&self, change_b64: String) -> bool {
        let bytes = match STANDARD.decode(change_b64.as_bytes()) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("mafold-core: rejected relayed change (bad base64): {e}");
                return false;
            }
        };
        let mut doc = self.lock();
        let before = doc.get_heads();
        if let Err(e) = doc.load_incremental(&bytes) {
            eprintln!("mafold-core: rejected relayed change ({} bytes, undecodable): {e}", bytes.len());
            return false;
        }
        let _ = doc.save_incremental(); // keep the relay cursor at the merged head
        doc.get_heads() != before
    }

    /// Current value of a root counter.
    pub fn counter_value(&self, key: String) -> i64 {
        read_counter(&*self.lock(), &key)
    }

    /// Set a root map key to an arbitrary JSON value — scalar OR nested
    /// object/array — the native equivalent of the web app's
    /// `room.change(d => { d[key] = value })`. Scalars cover chess board/turn;
    /// nested objects/arrays cover Garden (`folders` list, `item:<id>` maps with
    /// `thumb`/`thumbAlt`). Whole-key last-writer-wins (a fresh container replaces
    /// any prior value). Returns the change bytes (base64) to relay, "" on
    /// no-op/unsupported/parse-fail.
    pub fn put(&self, key: String, json_value: String) -> String {
        let v: serde_json::Value = match serde_json::from_str(&json_value) {
            Ok(v) => v,
            Err(_) => return String::new(),
        };
        let mut doc = self.lock();
        // NEVER `put` over a key that already holds a Counter: replacing the counter
        // with a plain scalar destroys its accumulated increments and breaks CRDT
        // convergence (the other devices keep summing into a key this one flattened).
        // Counters are mutated only via `increment_counter`.
        if matches!(
            doc.get(&ROOT, &key),
            Ok(Some((Value::Scalar(s), _))) if matches!(s.as_ref(), ScalarValue::Counter(_))
        ) {
            return String::new();
        }
        if !write_json(&mut *doc, &ROOT, key.as_str(), &v) {
            return String::new();
        }
        STANDARD.encode(doc.save_incremental())
    }

    /// The whole root document as a JSON object string, so the JS app reads
    /// `room.doc.<key>` — scalars (board/turn/seats/…, a `Counter` as a number)
    /// AND nested objects/arrays (Garden's `folders` + `item:<id>`), recursively.
    pub fn snapshot_json(&self) -> String {
        let doc = self.lock();
        let mut map = serde_json::Map::new();
        for k in doc.keys(&ROOT) {
            if let Ok(Some((v, id))) = doc.get(&ROOT, k.as_str()) {
                map.insert(k, value_to_json(&*doc, &v, &id, 0));
            }
        }
        serde_json::to_string(&serde_json::Value::Object(map)).unwrap_or_else(|_| "{}".into())
    }

    /// Full document bytes for local persistence (SQLite); reload via `open`.
    pub fn snapshot(&self) -> Vec<u8> {
        self.lock().save()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};

    // Real change blobs the WEB client (@automerge/automerge 3.2.6) produced and
    // stored in the prod op-log for the counter room: 3×(+1) then 1×(+5) on a
    // root "count" Counter → 8. Applying them with the Rust `automerge` crate and
    // getting 8 proves the wire format interoperates across platforms.
    const WEB_CHANGES: &[&str] = &[
        "hW9KgzPwpIsBQwAQkFtiPId/liqgasm5wjjGqAEBrtzi0QYAAAgVBzQBQgNWA1cCcANxAnMCAgVjb3VudAJ+AQV+GBQAAX4AAX8AfwE=",
        "hW9Kg5ddyFoBXwEz8KSLm/5eC7Ro5ro2A3ZKUNyoJpRsQ4hC1PU0NBr3FxCQW2I8h3+WKqBqybnCOMaoAgO93OLRBgAACBUHNAFCAlYCVwFwAnECcwJ/BWNvdW50AX8FfxQBfwF/AH8B",
        "hW9Kg4r1Gx0BXwGXXchaDKNjuT8mDMGHznR3aRRbJBii+b9cqLotTToSLxCQW2I8h3+WKqBqybnCOMaoAwTO3OLRBgAACBUHNAFCAlYCVwFwAnECcwJ/BWNvdW50AX8FfxQBfwF/AH8B",
        "hW9Kgy2Rj1IBXwGK9RsdjJuik1loYYEqvuRRWreZdGTpgYbXCAX3gwkjvhCQW2I8h3+WKqBqybnCOMaoBAXd3OLRBgAACBUHNAFCAlYCVwFwAnECcwJ/BWNvdW50AX8FfxQFfwF/AH8B",
    ];

    #[test]
    fn web_automerge_changes_apply_in_rust() {
        let mut doc = automerge::Automerge::new();
        for b64 in WEB_CHANGES {
            let bytes = STANDARD.decode(b64).expect("valid base64");
            let change = automerge::Change::from_bytes(bytes).expect("valid automerge change");
            doc.apply_changes(vec![change]).expect("apply change");
        }
        assert_eq!(read_counter(&doc, "count"), 8, "web-produced CRDT changes must converge to 8 in Rust");
    }

    // Two devices (distinct actors), local-first taps relayed both ways, including
    // concurrent increments → both converge to the SUM. This is the Room API the
    // iOS app drives over UniFFI.
    #[test]
    fn rust_room_round_trip_converges() {
        let a = Room::open(None, "a".repeat(32));
        let b = Room::open(None, "b".repeat(32));

        let d1 = a.increment_counter("count".into(), 1);
        let d2 = a.increment_counter("count".into(), 5);
        assert!(b.apply_remote(d1));
        assert!(b.apply_remote(d2));
        assert_eq!(b.counter_value("count".into()), 6);

        // b taps concurrently; relay it to a.
        let d3 = b.increment_counter("count".into(), 2);
        assert!(a.apply_remote(d3));

        assert_eq!(a.counter_value("count".into()), 8);
        assert_eq!(b.counter_value("count".into()), 8);

        // a snapshot reloads to the same value (local persistence).
        let reopened = Room::open(Some(a.snapshot()), "a".repeat(32));
        assert_eq!(reopened.counter_value("count".into()), 8);

        // A garbage relayed change is rejected (no-op `false`), never a panic.
        assert!(!a.apply_remote("!!!not-base64!!!".into()));
        // A corrupt snapshot does NOT panic and does NOT carry stale state — it
        // falls back to fresh (the increments above don't leak in).
        let from_corrupt = Room::open(Some(vec![1, 2, 3, 4]), "a".repeat(32));
        assert_eq!(from_corrupt.counter_value("count".into()), 0);
    }

    // Generic `put` (board string + turn) + JSON snapshot — what 象棋 drives —
    // relays and converges, and reads back as plain JSON for the JS app.
    #[test]
    fn rust_room_put_and_snapshot_json() {
        let a = Room::open(None, "a".repeat(32));
        let b = Room::open(None, "b".repeat(32));

        let d1 = a.put("board".into(), "\"rnbakabnr\"".into());
        let d2 = a.put("turn".into(), "\"b\"".into());
        assert!(b.apply_remote(d1));
        assert!(b.apply_remote(d2));

        let snap: serde_json::Value = serde_json::from_str(&b.snapshot_json()).unwrap();
        assert_eq!(snap["board"], "rnbakabnr");
        assert_eq!(snap["turn"], "b");
    }

    // `put` must NOT clobber a Counter key — that would destroy its accumulated
    // increments and break convergence. The put is rejected; the counter survives.
    #[test]
    fn rust_room_put_does_not_clobber_counter() {
        let a = Room::open(None, "a".repeat(32));
        a.increment_counter("count".into(), 7);
        assert_eq!(a.counter_value("count".into()), 7);
        // Attempt to overwrite the counter with a scalar → no-op, counter intact.
        assert_eq!(a.put("count".into(), "\"oops\"".into()), "");
        assert_eq!(a.counter_value("count".into()), 7);
        a.increment_counter("count".into(), 3);
        assert_eq!(a.counter_value("count".into()), 10);
    }

    // Garden-shaped nested data (a `folders` LIST + `item:<id>` MAPs with a nested
    // `thumb`) must relay and round-trip to real JSON — the old scalar-only reader
    // dropped these entirely, so the Garden shelf rendered empty on a native host.
    #[test]
    fn rust_room_nested_garden_shapes_round_trip() {
        let a = Room::open(None, "a".repeat(32));
        let b = Room::open(None, "b".repeat(32));

        let d1 = a.put(
            "folders".into(),
            r#"[{"id":"f-sites","name":"网站","order":0},{"id":"f-art","name":"艺术品&设计","order":1}]"#.into(),
        );
        let d2 = a.put(
            "item:x1".into(),
            r#"{"title":"失败创业坟场","folderId":"f-sites","thumb":{"url":"/media/a.png","w":800,"h":600},"thumbAlt":{"url":"/media/b.png"}}"#.into(),
        );
        assert!(b.apply_remote(d1));
        assert!(b.apply_remote(d2));

        let snap: serde_json::Value = serde_json::from_str(&b.snapshot_json()).unwrap();
        assert_eq!(snap["folders"][0]["id"], "f-sites");
        assert_eq!(snap["folders"][1]["name"], "艺术品&设计");
        assert_eq!(snap["folders"].as_array().unwrap().len(), 2);
        assert_eq!(snap["item:x1"]["thumb"]["w"], 800);
        assert_eq!(snap["item:x1"]["thumbAlt"]["url"], "/media/b.png");

        // Whole-key LWW: rewriting `folders` REPLACES the list (not merge/append).
        let d3 = a.put("folders".into(), r#"[{"id":"f-only","name":"仅一个"}]"#.into());
        assert!(b.apply_remote(d3));
        let snap2: serde_json::Value = serde_json::from_str(&b.snapshot_json()).unwrap();
        assert_eq!(snap2["folders"].as_array().unwrap().len(), 1);
        assert_eq!(snap2["folders"][0]["id"], "f-only");

        // Delete via `put(key, null)` — the key surfaces as JSON null.
        let d4 = a.put("item:x1".into(), "null".into());
        assert!(b.apply_remote(d4));
        let snap3: serde_json::Value = serde_json::from_str(&b.snapshot_json()).unwrap();
        assert!(snap3["item:x1"].is_null());
    }
}
