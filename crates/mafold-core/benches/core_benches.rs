//! Criterion benches for the core's hot paths, on the REAL native backend
//! (`Store<SqliteStore(":memory:")>`), so numbers include storage + serde cost.
//!
//! Run: `cargo bench` · one group: `cargo bench -- store_write`
//! HTML report: target/criterion/report/index.html
//!
//! Async note: sqlite futures are always-ready (no I/O awaits), so each
//! iteration drives them with `pollster::block_on` — no executor overhead in
//! the measurement beyond a fn call.
#![cfg(not(target_arch = "wasm32"))]

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use mafold_core::internal::{LangPack, SqliteStore, Store};
use mafold_core::{mafold_types as wire, CoreAccount, CoreConversation, CoreMessage};
use pollster::block_on;

// ── fixtures (mirror src/store.rs tests' shapes) ──

fn acct(u: &str) -> CoreAccount {
    CoreAccount {
        username: u.into(), display_name: u.to_uppercase(), kind: "human".into(),
        avatar: None, parent_username: None, template: None, language: None,
        verified: false,
    }
}

fn msg(id: &str, conv: &str, ts: i64, cid: Option<&str>) -> CoreMessage {
    CoreMessage {
        id: id.into(), conversation_id: conv.into(), sender: acct("ops"),
        content: format!("message body {id} — 一条常规长度的聊天消息,用于基准测试。"),
        created_at_ms: ts, finalized_at_ms: Some(ts), client_msg_id: cid.map(String::from),
        thread_root_id: None, channel_id: None,
        payload: Some(format!("{{\"id\":\"{id}\",\"content\":\"payload blob for {id}\"}}")),
    }
}

/// A store pre-seeded with `n` cmid-carrying messages on one timeline — the
/// worst case for the per-upsert reconcile scan.
fn seeded_store(n: usize) -> Store<SqliteStore> {
    let s = Store::new(SqliteStore::open(":memory:").unwrap());
    block_on(async {
        for i in 0..n {
            s.upsert_message(&msg(&format!("m{i:06}"), "conv", i as i64, Some(&format!("c{i:06}")))).await;
        }
    });
    s
}

// ── store: writes ──

fn store_write(c: &mut Criterion) {
    let mut g = c.benchmark_group("store_write");

    // The hot path: every incoming WS message with a client_msg_id triggers an
    // O(N) timeline scan (optimistic-send reconcile). Steady-state: re-upsert
    // the SAME newest message so the timeline doesn't grow between iterations.
    for n in [100usize, 1_000, 5_000] {
        let s = seeded_store(n);
        let echo = msg(&format!("m{:06}", n - 1), "conv", (n - 1) as i64, Some(&format!("c{:06}", n - 1)));
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::new("upsert_reconcile", n), &n, |b, _| {
            b.iter(|| block_on(s.upsert_message(&echo)));
        });
    }

    // Baseline without a cmid: no reconcile scan, just get+put+preview-bump.
    {
        let s = seeded_store(1_000);
        let plain = msg("m000999", "conv", 999, None);
        g.bench_function("upsert_no_cmid_1000", |b| b.iter(|| block_on(s.upsert_message(&plain))));
    }

    // Write-through of a full history page: clear + batch-dedup + preview.
    for n in [100usize, 1_000] {
        let s = Store::new(SqliteStore::open(":memory:").unwrap());
        let batch: Vec<CoreMessage> =
            (0..n).map(|i| msg(&format!("m{i:06}"), "conv", i as i64, Some(&format!("c{i:06}")))).collect();
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::new("replace_messages", n), &n, |b, _| {
            b.iter(|| block_on(s.replace_messages("conv", &batch)));
        });
    }
    g.finish();
}

// ── store: reads ──

fn store_read(c: &mut Criterion) {
    let mut g = c.benchmark_group("store_read");

    for n in [100usize, 1_000, 5_000] {
        let s = seeded_store(n);
        g.throughput(Throughput::Elements(n as u64));
        g.bench_with_input(BenchmarkId::new("messages", n), &n, |b, _| {
            b.iter(|| block_on(s.messages("conv")));
        });
    }

    // Thread view: full timeline scan splitting root vs replies.
    {
        let s = seeded_store(1_000);
        block_on(async {
            for i in 0..50 {
                let mut r = msg(&format!("r{i:03}"), "conv", 10_000 + i, None);
                r.thread_root_id = Some("m000500".into());
                s.upsert_message(&r).await;
            }
        });
        g.bench_function("thread_1000_msgs_50_replies", |b| {
            b.iter(|| block_on(s.thread("conv", "m000500")));
        });
    }

    // Dialog list: denormalized ConvMeta read + sort (no message scans).
    for convs in [50usize, 200] {
        let s = Store::new(SqliteStore::open(":memory:").unwrap());
        block_on(async {
            for i in 0..convs {
                let id = format!("conv{i:04}");
                s.upsert_conversation(&CoreConversation {
                    id: id.clone(), kind: "direct".into(), title: Some(format!("Chat {i}")),
                    participants: vec![acct("ops"), acct("peer")], updated_at_ms: i as i64,
                    unread_count: (i % 5) as u32, last_message: Some(msg("last", &id, i as i64, None)),
                    is_forum: false, forum_member_channels: false,
                    member_add_members: false, member_edit_info: false, member_add_bots: false,
                }).await;
            }
        });
        g.throughput(Throughput::Elements(convs as u64));
        g.bench_with_input(BenchmarkId::new("conversations", convs), &convs, |b, _| {
            b.iter(|| block_on(s.conversations()));
        });
    }
    g.finish();
}

// ── wire conversion (runs once per WS event / REST message) ──

fn wire_conv(c: &mut Criterion) {
    let mut g = c.benchmark_group("wire");
    let plain: wire::Message = serde_json::from_str(
        r#"{"id":"6dd93a1e-46e4-4d31-a461-c8c8fbf9f0a5","conversation_id":"0a02b7d1-6a3c-49f7-97a3-1ec54cf9e2f1",
            "sender":{"username":"ops","display_name":"Ops","kind":"human"},
            "content":"a normal chat message body, medium length, 中英混排都有一点。",
            "created_at":"2026-07-14T00:00:00Z","reactions":[]}"#,
    ).unwrap();
    g.bench_function("message_from_wire", |b| b.iter(|| CoreMessage::from(&plain)));

    let with_attachments: wire::Message = serde_json::from_str(
        r#"{"id":"6dd93a1e-46e4-4d31-a461-c8c8fbf9f0a5","conversation_id":"0a02b7d1-6a3c-49f7-97a3-1ec54cf9e2f1",
            "sender":{"username":"ops","display_name":"Ops","kind":"human"},
            "content":"photo dump",
            "attachments":[
              {"kind":"photo","id":"a1","media_id":"m1","url":"https://api.mafold.com/media/1.jpg","w":1024,"h":768},
              {"kind":"photo","id":"a2","media_id":"m2","url":"https://api.mafold.com/media/2.jpg","w":1024,"h":768},
              {"kind":"file","id":"a3","media_id":"m3","url":"https://api.mafold.com/media/3.pdf","filename":"spec.pdf","size_bytes":123456,"mime":"application/pdf"}
            ],
            "created_at":"2026-07-14T00:00:00Z","reactions":[]}"#,
    ).unwrap();
    g.bench_function("message_from_wire_with_attachments", |b| {
        b.iter(|| CoreMessage::from(&with_attachments))
    });
    g.finish();
}

// ── i18n (per-string render hot path; real production packs) ──

fn i18n(c: &mut Criterion) {
    let mut g = c.benchmark_group("i18n");
    let en: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(include_str!("../../mafold-api/langpacks/en.json")).unwrap();
    let zh: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(include_str!("../../mafold-api/langpacks/zh-Hans.json")).unwrap();
    let mut lp = LangPack::default();
    lp.set_base("en", en);
    lp.set_active("zh-Hans", 15, zh, false);

    let no_args = serde_json::Map::new();
    let mut args = serde_json::Map::new();
    args.insert("name".into(), serde_json::json!("Ops"));

    g.bench_function("t_hit", |b| b.iter(|| lp.t("settings.title", &no_args)));
    g.bench_function("t_base_fallback", |b| b.iter(|| lp.t("site.hero.tagline", &no_args)));
    g.bench_function("t_interpolate", |b| b.iter(|| lp.t("settings.title", &args)));
    g.bench_function("plural", |b| b.iter(|| lp.plural("profile.members.count", 5.0, &no_args)));
    g.finish();
}

// ── flags (resolved snapshot + single check; each hits the KV) ──

fn flags(c: &mut Criterion) {
    let mut g = c.benchmark_group("flags");
    let s = Store::new(SqliteStore::open(":memory:").unwrap());
    let state = serde_json::json!({
        "values": {"gardenApps": true, "moments": false, "showIds": true},
        "version": 3
    })
    .to_string();
    block_on(s.flags_ingest(&state));

    g.bench_function("resolved", |b| b.iter(|| block_on(s.flags_resolved())));
    g.bench_function("enabled", |b| b.iter(|| block_on(s.flags_enabled("gardenApps"))));
    g.finish();
}

criterion_group!(benches, store_write, store_read, wire_conv, i18n, flags);
criterion_main!(benches);
