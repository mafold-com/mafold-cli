//! wasm32 integration tests — run in a REAL headless browser (IdbStore needs
//! actual IndexedDB; Node has none):
//!
//!   wasm-pack test --headless --chrome -- --test wasm
//!
//! The `--test wasm` filter is essential: it compiles ONLY this integration
//! test (the lib builds as a plain dependency), so the native-gated inline
//! test modules (pollster/tokio) are never compiled for wasm32.
//!
//! Each test opens a UNIQUELY named DB — all tests share one browser instance
//! per run, so name collisions would leak state across tests.
#![cfg(target_arch = "wasm32")]

use mafold_core::internal::{IdbStore, Storage, Store};
use mafold_core::{CoreAccount, CoreMessage};
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

fn acct() -> CoreAccount {
    CoreAccount {
        username: "ops".into(), display_name: "Ops".into(), kind: "human".into(),
        avatar_url: None, parent_username: None, template: None, language: None,
        verified: false,
    }
}

fn msg(id: &str, conv: &str, ts: i64, cid: Option<&str>) -> CoreMessage {
    CoreMessage {
        id: id.into(), conversation_id: conv.into(), sender: acct(),
        content: format!("m-{id}"), created_at_ms: ts, finalized_at_ms: Some(ts),
        client_msg_id: cid.map(String::from), thread_root_id: None, channel_id: None,
        payload: None,
    }
}

/// IdbStore KV semantics over real IndexedDB: roundtrip, overwrite, delete,
/// table scoping (the `\u{1f}` composite-key scheme) and key-SORTED scans —
/// the message timeline's time-order depends on the sort.
#[wasm_bindgen_test]
async fn idb_kv_roundtrip_and_scan() {
    let s = IdbStore::open("wasmtest-kv").await.expect("open idb");

    assert_eq!(s.get("a", "k").await, None);
    s.put("a", "k", b"v1".to_vec()).await;
    assert_eq!(s.get("a", "k").await.as_deref(), Some(b"v1".as_ref()));
    s.put("a", "k", b"v2".to_vec()).await;
    assert_eq!(s.get("a", "k").await.as_deref(), Some(b"v2".as_ref()));
    // Table scoping: same key under another table is invisible.
    assert_eq!(s.get("b", "k").await, None);

    // Insert out of order → scan comes back key-sorted, prefix-bounded,
    // table-scoped.
    for k in ["c1|003", "c1|001", "c2|001", "c1|002"] {
        s.put("msg", k, k.as_bytes().to_vec()).await;
    }
    s.put("other", "c1|zzz", vec![9]).await;
    let keys: Vec<String> = s.scan_prefix("msg", "c1|").await.into_iter().map(|(k, _)| k).collect();
    assert_eq!(keys, vec!["c1|001", "c1|002", "c1|003"]);

    s.delete("a", "k").await;
    assert_eq!(s.get("a", "k").await, None);
}

/// The optimistic-send reconcile invariant over REAL async storage: the server
/// echo (same client_msg_id, new id) replaces the placeholder and inherits its
/// payload — one implementation, one behavior, native and wasm alike.
#[wasm_bindgen_test]
async fn store_reconcile_over_idb() {
    let s = Store::new(IdbStore::open("wasmtest-reconcile").await.expect("open idb"));

    let mut optimistic = msg("local-1", "conv1", 100, Some("cmid-1"));
    optimistic.payload = Some("{\"full\":\"local\"}".into());
    s.upsert_message(&optimistic).await;

    // Echo: real id, later ts, same cmid, NO payload → must replace + inherit.
    s.upsert_message(&msg("srv-1", "conv1", 150, Some("cmid-1"))).await;

    let list = s.messages("conv1").await;
    assert_eq!(list.len(), 1, "echo must replace the optimistic copy");
    assert_eq!(list[0].id, "srv-1");
    assert_eq!(list[0].payload.as_deref(), Some("{\"full\":\"local\"}"));
}

/// REGRESSION PIN for the documented wasm hazard the store's `write_lock`
/// exists for: two OVERLAPPING upserts (optimistic + echo sharing a
/// client_msg_id) interleave their get→scan→delete→put awaits on IndexedDB's
/// genuinely-async futures — without the lock they'd both miss each other's
/// write and DUPLICATE. `futures::future::join` drives both concurrently on
/// the single-threaded wasm executor, so every await is an interleaving point.
#[wasm_bindgen_test]
async fn overlapping_upserts_do_not_duplicate() {
    let s = Store::new(IdbStore::open("wasmtest-race").await.expect("open idb"));

    let optimistic = msg("local-9", "convR", 100, Some("cmid-9"));
    let echo = msg("srv-9", "convR", 150, Some("cmid-9"));
    futures::future::join(s.upsert_message(&optimistic), s.upsert_message(&echo)).await;

    let list = s.messages("convR").await;
    assert_eq!(
        list.len(),
        1,
        "overlapping upserts sharing a cmid must reconcile to ONE message (write_lock)"
    );
    assert_eq!(list[0].id, "srv-9", "the newer echo wins");
}

/// Channel bucketing works identically over IndexedDB: a channel message lives
/// on its channel timeline, invisible to the #all main timeline.
#[wasm_bindgen_test]
async fn channel_buckets_over_idb() {
    let s = Store::new(IdbStore::open("wasmtest-channel").await.expect("open idb"));
    s.upsert_message(&msg("a1", "convC", 100, None)).await;
    let mut cm = msg("c1", "convC", 200, None);
    cm.channel_id = Some("chanX".into());
    s.upsert_message(&cm).await;

    let main: Vec<String> = s.messages("convC").await.into_iter().map(|m| m.id).collect();
    let chan: Vec<String> = s.messages("chanX").await.into_iter().map(|m| m.id).collect();
    assert_eq!(main, vec!["a1"]);
    assert_eq!(chan, vec!["c1"]);
}

/// Per-device UI state (the last-open forum channel) persists through real
/// IndexedDB: set → get roundtrip, clear (None = back to #all).
#[wasm_bindgen_test]
async fn last_channel_over_idb() {
    let s = Store::new(IdbStore::open("wasmtest-lastchan").await.expect("open idb"));
    assert_eq!(s.last_channel("convU").await, None);
    s.set_last_channel("convU", Some("chanZ")).await;
    assert_eq!(s.last_channel("convU").await.as_deref(), Some("chanZ"));
    s.set_last_channel("convU", None).await;
    assert_eq!(s.last_channel("convU").await, None);
}
