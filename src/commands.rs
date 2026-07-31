//! Emulated Claude Code slash commands.
//!
//! Many built-in `/commands` are terminal-UI only and do nothing in headless
//! `claude -p`. Where we can, the daemon EMULATES them locally — running a safe
//! `claude` subcommand or reading the same config the TUI would show — and
//! replies in chat. `/login` and `/logout` actually drive `claude auth` (the
//! sign-in link is posted to the chat for the device flow). Commands we can't
//! reproduce headless get a short "terminal-only" note instead of a useless
//! pass-through. Everything else falls through to `claude -p`.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

pub enum Outcome {
    /// Handled locally — send this markdown reply.
    Reply(String),
    /// Not emulated — let the caller forward it to `claude -p`.
    Forward,
}

/// Route a slash command. `name` is lowercased, without the leading slash.
/// (`/login` is handled in agent.rs — it needs the daemon's per-chat state to
/// relay the pasted auth code into the login process.)
pub async fn handle(name: &str, _arg: &str, workdir: &str, session: Option<&str>) -> Outcome {
    match name {
        // ── usage stats (rich card): local transcript scan + live rate limits ──
        "stats" | "usage" | "cost" => Outcome::Reply(stats(&fetch_limits().await, workdir, session)),
        // ── auth ──
        "logout" => Outcome::Reply(logout().await),
        // ── read local config / state ──
        "config" | "settings" => Outcome::Reply(dump_settings(workdir)),
        "memory" => Outcome::Reply(dump_memory(workdir)),
        "mcp" => Outcome::Reply(fence_block("🔌 MCP servers", "", &run_claude(&["mcp", "list"], 25).await)),
        "agents" => Outcome::Reply(dump_agents(workdir)),
        "skills" => Outcome::Reply(dump_skills(workdir)),
        "hooks" => Outcome::Reply(settings_key(workdir, "hooks", "🪝 Hooks")),
        "permissions" => Outcome::Reply(settings_key(workdir, "permissions", "🔐 Permissions")),
        "plugin" | "plugins" => Outcome::Reply(dump_plugins()),
        "keybindings" => Outcome::Reply(dump_file("⌨️ Keybindings", "json", &home().join(".claude/keybindings.json"))),
        "statusline" => Outcome::Reply(settings_key(workdir, "statusLine", "Status line")),
        "privacy-settings" | "privacy" => Outcome::Reply(settings_key(workdir, "privacy", "Privacy settings")),
        "doctor" => Outcome::Reply(fence_block("🩺 claude doctor", "", &run_claude(&["doctor"], 30).await)),
        // ── terminal-only: a friendly mock note ──
        n if mock_desc(n).is_some() => Outcome::Reply(mock_reply(n)),
        _ => Outcome::Forward,
    }
}

// ───────────────────────── auth ─────────────────────────

/// `/logout` — clear the host's Anthropic credentials.
async fn logout() -> String {
    let out = run_claude(&["auth", "logout"], 20).await;
    format!(
        "👋 Logged out of Anthropic.\n{}\n⚠️ This is the account that powers THIS agent's `claude`. Until you `/login` again (or re-auth on the host), I can't reply to tasks.",
        if out.trim().is_empty() { String::new() } else { format!("{}\n", out.trim()) },
    )
}

pub async fn auth_status_line() -> String {
    let o = run_claude(&["auth", "status", "--text"], 8).await;
    o.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("").to_string()
}

// ───────────────────────── config dumps ─────────────────────────

/// `/config` `/settings` — a structured summary card of the EFFECTIVE config
/// (user ← project ← local, later file wins per top-level key; rows from a
/// project/local file are tagged with their source) with the raw files kept
/// below for the full detail.
fn dump_settings(workdir: &str) -> String {
    use serde_json::Value;

    let files = [
        ("user", home().join(".claude/settings.json")),
        ("project", PathBuf::from(workdir).join(".claude/settings.json")),
        ("local", PathBuf::from(workdir).join(".claude/settings.local.json")),
    ];
    let mut merged = serde_json::Map::new();
    let mut src: std::collections::HashMap<String, &str> = Default::default();
    let mut raws: Vec<(&str, PathBuf, String)> = vec![];
    for (label, p) in &files {
        let Ok(text) = std::fs::read_to_string(p) else { continue };
        if let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(&text) {
            for (k, v) in obj {
                src.insert(k.clone(), label);
                merged.insert(k, v);
            }
        }
        raws.push((label, p.clone(), cap_chars(&text, 2500)));
    }
    if raws.is_empty() {
        return "⚙️ **Settings**\n\n_No settings files found (user or project)._".into();
    }

    // (row label, value, merged key — for the source tag)
    let mut rows: Vec<(String, String, String)> = vec![];

    if let Some(m) = merged.get("model").and_then(|v| v.as_str()) {
        let mut val = m.to_string();
        if let Some(e) = merged.get("effortLevel").and_then(|v| v.as_str()) {
            val.push_str(&format!(" · effort {e}"));
        }
        rows.push(("Model".into(), val, "model".into()));
    }
    if let Some(p) = merged.get("permissions") {
        let mut val = p["defaultMode"].as_str().unwrap_or("default").to_string();
        for k in ["allow", "ask", "deny"] {
            let n = p[k].as_array().map(|a| a.len()).unwrap_or(0);
            if n > 0 { val.push_str(&format!(" · {n} {k}")); }
        }
        rows.push(("Permissions".into(), val, "permissions".into()));
    }
    if let Some(Value::Object(h)) = merged.get("hooks") {
        if !h.is_empty() {
            let val = h.iter().map(|(ev, v)| {
                let n = v.as_array().map(|a| a.len()).unwrap_or(0);
                if n > 1 { format!("{ev} ×{n}") } else { ev.clone() }
            }).collect::<Vec<_>>().join(" · ");
            rows.push(("Hooks".into(), val, "hooks".into()));
        }
    }
    if let Some(Value::Object(e)) = merged.get("env") {
        if !e.is_empty() {
            let names: Vec<&str> = e.keys().map(|s| s.as_str()).take(5).collect();
            let extra = e.len().saturating_sub(names.len());
            let mut val = names.join(" · ");
            if extra > 0 { val.push_str(&format!(" +{extra}")); }
            rows.push(("Env".into(), val, "env".into()));
        }
    }
    if let Some(sl) = merged.get("statusLine") {
        let val = sl["command"].as_str().or(sl["type"].as_str()).unwrap_or("set").to_string();
        rows.push(("Status line".into(), val, "statusLine".into()));
    }
    if let Some(Value::Object(pl)) = merged.get("enabledPlugins") {
        let names: Vec<&str> = pl.iter()
            .filter(|(_, on)| on.as_bool() == Some(true))
            .map(|(k, _)| k.split('@').next().unwrap_or(k))
            .collect();
        if !names.is_empty() {
            rows.push(("Plugins".into(), names.join(" · "), "enabledPlugins".into()));
        }
    }
    if let Some(t) = merged.get("theme").and_then(|v| v.as_str()) {
        rows.push(("Theme".into(), t.to_string(), "theme".into()));
    }
    if let Some(v) = merged.get("voice") {
        let val = if v["enabled"].as_bool() == Some(true) {
            format!("on · {}", v["mode"].as_str().unwrap_or("tap"))
        } else {
            "off".into()
        };
        rows.push(("Voice".into(), val, "voice".into()));
    }
    if let Some(c) = merged.get("commit") {
        if let Some(co) = c["coAuthor"].as_bool() {
            rows.push(("Commit co-author".into(), if co { "on".into() } else { "off".into() }, "commit".into()));
        }
    }

    // Everything else, generically (scalars as-is, objects compacted) — so a
    // key we didn't special-case is still visible.
    const HANDLED: &[&str] = &["model", "effortLevel", "permissions", "hooks", "env", "statusLine", "enabledPlugins", "theme", "voice", "commit"];
    for (k, v) in merged.iter().filter(|(k, _)| !HANDLED.contains(&k.as_str())).take(16) {
        let val = match v {
            Value::String(s) => s.clone(),
            other => serde_json::to_string(other).unwrap_or_default(),
        };
        rows.push((k.clone(), val, k.clone()));
    }

    let mut out = String::from("{% stats title=\"Settings\" icon=\"wrench\" %}\n");
    for (label, val, key) in rows {
        let tag = match src.get(&key) {
            Some(&l) if l != "user" => format!(" · {l}"),
            _ => String::new(),
        };
        out.push_str(&format!("kv|{label}|{}{tag}\n", clip(&val, 90)));
    }
    out.push_str("{% /stats %}\n");
    for (label, p, body) in raws {
        out.push_str(&format!("\n**{label}** · `{}`\n{}\n", p.display(), fence("json", &body)));
    }
    out
}

fn dump_memory(workdir: &str) -> String {
    let files = [
        ("user", home().join(".claude/CLAUDE.md")),
        ("project", PathBuf::from(workdir).join("CLAUDE.md")),
        ("project (.claude)", PathBuf::from(workdir).join(".claude/CLAUDE.md")),
    ];
    let mut out = String::from("🧠 **Memory (CLAUDE.md)**\n");
    let mut any = false;
    for (label, p) in files {
        if let Some(body) = read_capped(&p, 3500) {
            any = true;
            out.push_str(&format!("\n**{label}** · `{}`\n{}\n", p.display(), fence("markdown", &body)));
        }
    }
    if !any { out.push_str("\n_No CLAUDE.md found (user or project)._"); }
    out
}

fn settings_key(workdir: &str, key: &str, title: &str) -> String {
    let files = [
        ("user", home().join(".claude/settings.json")),
        ("project", PathBuf::from(workdir).join(".claude/settings.json")),
        ("project (local)", PathBuf::from(workdir).join(".claude/settings.local.json")),
    ];
    let mut out = format!("{title}\n");
    let mut any = false;
    for (label, p) in files {
        let Ok(text) = std::fs::read_to_string(&p) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { continue };
        if let Some(sub) = v.get(key) {
            any = true;
            let pretty = serde_json::to_string_pretty(sub).unwrap_or_default();
            out.push_str(&format!("\n**{label}**\n{}\n", fence("json", &cap_chars(&pretty, 3000))));
        }
    }
    if !any { out.push_str(&format!("\n_No `{key}` configured._")); }
    out
}

fn dump_agents(workdir: &str) -> String {
    let dirs = [home().join(".claude/agents"), PathBuf::from(workdir).join(".claude/agents")];
    let mut lines: Vec<String> = vec![];
    for d in dirs {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) != Some("md") { continue; }
                let name = p.file_stem().and_then(|s| s.to_str()).unwrap_or("agent").to_string();
                let desc = frontmatter_desc(&p).unwrap_or_default();
                lines.push(if desc.is_empty() { format!("• `{name}`") } else { format!("• `{name}` — {}", clip(&desc, 90)) });
            }
        }
    }
    if lines.is_empty() { return "🤖 **Agents**\n\n_No custom agents found._".into(); }
    lines.sort();
    lines.dedup();
    format!("🤖 **Agents** ({})\n\n{}", lines.len(), lines.join("\n"))
}

fn dump_skills(workdir: &str) -> String {
    let mut names: Vec<String> = vec![];
    let push_dir = |d: PathBuf, prefix: &str, names: &mut Vec<String>| {
        if let Ok(entries) = std::fs::read_dir(&d) {
            for e in entries.flatten() {
                if e.path().join("SKILL.md").is_file() {
                    names.push(format!("{prefix}{}", e.file_name().to_string_lossy()));
                }
            }
        }
    };
    push_dir(home().join(".claude/skills"), "", &mut names);
    push_dir(PathBuf::from(workdir).join(".claude/skills"), "", &mut names);
    // plugin skills
    if let Ok(text) = std::fs::read_to_string(home().join(".claude/plugins/installed_plugins.json")) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(plugins) = v["plugins"].as_object() {
                for (full, installs) in plugins {
                    let short = full.split('@').next().unwrap_or(full);
                    if let Some(p) = installs.as_array().and_then(|a| a.iter().rev().find_map(|i| i["installPath"].as_str())) {
                        push_dir(PathBuf::from(p).join("skills"), &format!("{short}:"), &mut names);
                    }
                }
            }
        }
    }
    names.sort();
    names.dedup();
    if names.is_empty() { return "🧩 **Skills**\n\n_None installed._".into(); }
    format!("🧩 **Skills** ({})\n\n{}", names.len(), names.iter().map(|n| format!("`/{n}`")).collect::<Vec<_>>().join("  "))
}

fn dump_plugins() -> String {
    let Ok(text) = std::fs::read_to_string(home().join(".claude/plugins/installed_plugins.json")) else {
        return "🔌 **Plugins**\n\n_None installed._".into();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else { return "🔌 **Plugins**\n\n_(couldn't read plugin manifest)_".into() };
    let mut lines = vec![];
    if let Some(plugins) = v["plugins"].as_object() {
        for (full, installs) in plugins {
            let ver = installs.as_array().and_then(|a| a.last()).and_then(|i| i["version"].as_str()).unwrap_or("?");
            lines.push(format!("• `{full}` v{ver}"));
        }
    }
    if lines.is_empty() { return "🔌 **Plugins**\n\n_None installed._".into(); }
    format!("🔌 **Plugins** ({})\n\n{}", lines.len(), lines.join("\n"))
}

fn dump_file(title: &str, lang: &str, path: &Path) -> String {
    match read_capped(path, 3500) {
        Some(body) => format!("{title}\n`{}`\n{}", path.display(), fence(lang, &body)),
        None => format!("{title}\n\n_Not set (`{}` not found)._", path.display()),
    }
}

// ───────────────────────── usage stats ─────────────────────────

/// One model's four token buckets.
#[derive(Default, Clone, Copy)]
struct Buckets {
    input: u64,
    output: u64,
    cache_read: u64,
    cache_write: u64,
}

impl Buckets {
    fn add_usage(&mut self, u: &serde_json::Value) {
        self.input += u["input_tokens"].as_u64().unwrap_or(0);
        self.output += u["output_tokens"].as_u64().unwrap_or(0);
        self.cache_read += u["cache_read_input_tokens"].as_u64().unwrap_or(0);
        self.cache_write += u["cache_creation_input_tokens"].as_u64().unwrap_or(0);
    }
    fn merge(&mut self, o: &Buckets) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write += o.cache_write;
    }
    /// input + output — the metric Claude Code's own Stats screen calls "Total
    /// tokens". Cache traffic is ~100× larger and would drown it.
    fn io(&self) -> u64 { self.input + self.output }
    fn any(&self) -> bool { self.io() + self.cache_read + self.cache_write > 0 }
}

/// USD per million tokens as (input, output), by short model name.
///
/// Cache reads bill at input × 0.1 and cache **writes at input × 2.0**: Claude
/// Code writes 1-hour-TTL cache entries, so the 5-minute ×1.25 rate under-counts
/// by ~7%. Checked against the TUI's own session total — $253.70 computed vs
/// $253.65 shown, i.e. display rounding. An unrecognised id prices at the Opus
/// tier so a newly released model never silently reads as free.
fn model_price(short: &str) -> (f64, f64) {
    if short.starts_with("fable") || short.starts_with("mythos") { (10.0, 50.0) }
    else if short.starts_with("sonnet") { (3.0, 15.0) }
    else if short.starts_with("haiku") { (1.0, 5.0) }
    else { (5.0, 25.0) }
}

/// Dollar cost of one model's token buckets.
fn bucket_cost(short: &str, b: &Buckets) -> f64 {
    let (inp, out) = model_price(short);
    (b.input as f64 * inp
        + b.output as f64 * out
        + b.cache_read as f64 * inp * 0.1
        + b.cache_write as f64 * inp * 2.0)
        / 1e6
}

/// "$4.20" / "$59.2k" — compact once the cents stop mattering.
fn fmt_usd(v: f64) -> String {
    if v >= 1000.0 { format!("${:.1}k", v / 1000.0) } else { format!("${v:.2}") }
}

/// Everything the usage card needs, from either data source.
#[derive(Default)]
struct Agg {
    /// epoch day → activity count. Assistant turns when scanned, Claude Code's
    /// own message count when cached — only ever compared against itself
    /// (heatmap shading, streaks, busiest day), never mixed into a total.
    per_day: std::collections::HashMap<i64, u64>,
    per_hour: [u64; 24],
    models: std::collections::HashMap<String, Buckets>,
    messages: u64,
    tools: u64,
    sessions: u64,
    /// Longest single session by wall-clock span, seconds.
    longest_sess: i64,
    /// Earliest activity, ISO — the card's "since".
    first_iso: String,
    /// Today's assistant turns, and how many ran with >150k of input-side
    /// context. Replaces the ">N% of your usage was at >150k context" line the
    /// prose `/usage` used to give us — the structured endpoint carries limits
    /// but no behaviour profile, and we see every turn's usage anyway.
    today_turns: u64,
    today_big_ctx: u64,
}

impl Agg {
    fn merge(&mut self, o: Agg) {
        for (h, n) in o.per_hour.iter().enumerate() { self.per_hour[h] += *n; }
        for (d, n) in o.per_day { *self.per_day.entry(d).or_default() += n; }
        for (m, b) in o.models { self.models.entry(m).or_default().merge(&b); }
        self.messages += o.messages;
        self.tools += o.tools;
        self.sessions += o.sessions;
        // The cache's own figure wins: a partial scan only sees the files it
        // touched, so its widest span is not comparable (a daemon session idle
        // for two months spans 63 days of wall-clock and would swamp it). The
        // cache recomputes daily, so a genuinely longer session lands tomorrow.
        if self.longest_sess == 0 { self.longest_sess = o.longest_sess; }
        if !o.first_iso.is_empty() && (self.first_iso.is_empty() || o.first_iso < self.first_iso) {
            self.first_iso = o.first_iso;
        }
        self.today_turns += o.today_turns;
        self.today_big_ctx += o.today_big_ctx;
    }
    fn tokens_io(&self) -> u64 { self.models.values().map(|b| b.io()).sum() }
    fn cost(&self) -> f64 { self.models.iter().map(|(m, b)| bucket_cost(m, b)).sum() }
    /// Most-used model by input+output tokens — the TUI's "Favorite model".
    fn favorite(&self) -> Option<&str> {
        self.models.iter().filter(|(_, b)| b.io() > 0).max_by_key(|(_, b)| b.io()).map(|(m, _)| m.as_str())
    }
}

/// Scan session transcripts under `~/.claude/projects/` into an [`Agg`].
///
/// `since_day` (epoch day, inclusive) bounds the work: a file last written
/// before it is skipped on its mtime alone — that stat, not the read, is what
/// makes the cache fast path cheap — and older lines inside a surviving file
/// are ignored. Session bookkeeping still spans the whole file, so a session
/// counts toward `sessions` only when its FIRST turn falls inside the window
/// and one resumed across midnight isn't double-counted against the cache's
/// own total. Only `"type":"assistant"` lines are JSON-parsed.
fn scan_transcripts(files: &[PathBuf], since_day: Option<i64>) -> Agg {
    use std::collections::HashMap;
    let since = since_day.unwrap_or(i64::MIN);
    let today = today_epoch_day();
    let mut agg = Agg::default();
    let mut sess_first: HashMap<String, i64> = HashMap::new();
    let mut sess_span: HashMap<String, (i64, i64)> = HashMap::new();

    for f in files {
        if since_day.is_some() {
            let stale = std::fs::metadata(f).ok()
                .and_then(|md| md.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .is_some_and(|d| (d.as_secs() as i64) / 86_400 < since);
            if stale { continue; }
        }
        let Ok(bytes) = std::fs::read(f) else { continue };
        // Lossy so a single bad byte never drops a whole transcript.
        for line in String::from_utf8_lossy(&bytes).lines() {
            // Per-day activity counts what Claude Code counts — every message
            // line except its own bookkeeping — so a cached day and a scanned
            // day are the same unit and the heatmap doesn't dip on the live
            // tail. Verified against the cache: 991 user + 1504 assistant + 129
            // attachment + 22 system = its 2646 for that date, exactly.
            if let Some(day) = line_day(line) {
                if day >= since
                    && !line.contains("\"type\":\"queue-operation\"")
                    && !line.contains("\"type\":\"file-history-delta\"")
                {
                    *agg.per_day.entry(day).or_default() += 1;
                    agg.messages += 1;
                }
            }
            // Cheap pre-filter: skip the (many) non-assistant lines without parsing.
            if !line.contains("\"type\":\"assistant\"") { continue; }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if v["type"].as_str() != Some("assistant") { continue; }
            let Some(ts) = v["timestamp"].as_str() else { continue };
            if ts.len() < 10 { continue; }
            let Some(day) = day_key_epoch(&ts[..10]) else { continue };

            if let Some(sid) = v["sessionId"].as_str() {
                let seen = sess_first.entry(sid.to_string()).or_insert(day);
                if day < *seen { *seen = day; }
                if let Some(secs) = iso_epoch_secs(ts) {
                    let span = sess_span.entry(sid.to_string()).or_insert((secs, secs));
                    if secs < span.0 { span.0 = secs; }
                    if secs > span.1 { span.1 = secs; }
                }
            }
            if day < since { continue; }

            let m = &v["message"];
            let mut b = Buckets::default();
            b.add_usage(&m["usage"]);
            if b.any() {
                if let Some(model) = m["model"].as_str() {
                    agg.models.entry(short_model(model)).or_default().merge(&b);
                }
                if day == today {
                    agg.today_turns += 1;
                    // Input side only — output doesn't sit in the context window.
                    if b.input + b.cache_read + b.cache_write > 150_000 { agg.today_big_ctx += 1; }
                }
            }
            if let Some(content) = m["content"].as_array() {
                agg.tools += content.iter().filter(|x| x["type"].as_str() == Some("tool_use")).count() as u64;
            }
            if ts.len() >= 13 {
                if let Ok(h) = ts[11..13].parse::<usize>() {
                    if h < 24 { agg.per_hour[h] += 1; }
                }
            }
            if agg.first_iso.is_empty() || ts < agg.first_iso.as_str() {
                agg.first_iso = ts.to_string();
            }
        }
    }
    agg.sessions = sess_first.values().filter(|d| **d >= since).count() as u64;
    agg.longest_sess = sess_span.values().map(|(a, b)| b - a).max().unwrap_or(0);
    agg
}

/// Claude Code's own aggregate at `~/.claude/stats-cache.json` as an [`Agg`],
/// plus the epoch day it is complete THROUGH.
///
/// Schema v4 carries the entire Stats screen: `dailyActivity[]`, `modelUsage{}`
/// (four token buckets per model), `hourCounts{}`, `totalSessions`,
/// `totalMessages`, `longestSession.duration` (ms) and `firstSessionDate`. It is
/// rewritten daily and `lastComputedDate` is the last COMPLETE day, so cache +
/// today's transcripts is exactly what the TUI renders — verified field by field
/// against it. Its `costUSD` entries are always 0, so cost is priced here from
/// the buckets instead.
///
/// None on a missing file, a schema older than v4, or any shape drift; the
/// caller then falls back to a full transcript scan.
fn stats_cache() -> Option<(Agg, i64)> {
    let raw = std::fs::read_to_string(home().join(".claude/stats-cache.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if v["version"].as_u64()? < 4 { return None; }
    let through = day_key_epoch(v["lastComputedDate"].as_str()?)?;

    let mut agg = Agg::default();
    for row in v["dailyActivity"].as_array()? {
        let Some(day) = row["date"].as_str().and_then(day_key_epoch) else { continue };
        *agg.per_day.entry(day).or_default() += row["messageCount"].as_u64().unwrap_or(0);
        agg.tools += row["toolCallCount"].as_u64().unwrap_or(0);
    }
    if agg.per_day.is_empty() { return None; }
    for (model, u) in v["modelUsage"].as_object()? {
        let b = agg.models.entry(short_model(model)).or_default();
        b.input += u["inputTokens"].as_u64().unwrap_or(0);
        b.output += u["outputTokens"].as_u64().unwrap_or(0);
        b.cache_read += u["cacheReadInputTokens"].as_u64().unwrap_or(0);
        b.cache_write += u["cacheCreationInputTokens"].as_u64().unwrap_or(0);
    }
    if let Some(hours) = v["hourCounts"].as_object() {
        for (h, n) in hours {
            if let (Ok(h), Some(n)) = (h.parse::<usize>(), n.as_u64()) {
                if h < 24 { agg.per_hour[h] += n; }
            }
        }
    }
    agg.messages = v["totalMessages"].as_u64().unwrap_or(0);
    agg.sessions = v["totalSessions"].as_u64().unwrap_or(0);
    agg.longest_sess = (v["longestSession"]["duration"].as_f64().unwrap_or(0.0) / 1000.0) as i64;
    agg.first_iso = v["firstSessionDate"].as_str().unwrap_or("").to_string();
    Some((agg, through))
}

/// This chat's own session — cost, wall-clock span and net code change.
///
/// `session` is the daemon's live session id for this chat; only when it has
/// none do we fall back to the newest transcript in the workdir, which is a
/// guess (sibling chats share a workdir and race for newest-mtime).
///
/// The TUI prints an API duration next to the wall duration; transcripts carry
/// no per-request timing, so wall-clock (first turn → last turn) is the only
/// honest figure and the only one we show.
struct SessionCost {
    usd: f64,
    wall: i64,
    added: u64,
    removed: u64,
}

fn session_cost(workdir: &str, session: Option<&str>) -> Option<SessionCost> {
    use std::collections::HashMap;
    let id = match session {
        // Session ids are UUIDs; refuse anything path-ish before joining it.
        Some(s) if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') => s.to_string(),
        _ => list_project_sessions(workdir).into_iter().next()?.id,
    };
    let bytes = std::fs::read(project_dir(workdir).join(format!("{id}.jsonl"))).ok()?;

    let mut models: HashMap<String, Buckets> = HashMap::new();
    let (mut first, mut last) = (i64::MAX, i64::MIN);
    let (mut added, mut removed) = (0u64, 0u64);
    for line in String::from_utf8_lossy(&bytes).lines() {
        // A single session can run to hundreds of megabytes — only parse the
        // two line shapes that carry anything we need.
        let assistant = line.contains("\"type\":\"assistant\"");
        if !assistant && !line.contains("structuredPatch") { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if assistant && v["type"].as_str() == Some("assistant") {
            if let Some(secs) = v["timestamp"].as_str().and_then(iso_epoch_secs) {
                first = first.min(secs);
                last = last.max(secs);
            }
            let m = &v["message"];
            if let Some(model) = m["model"].as_str() {
                models.entry(short_model(model)).or_default().add_usage(&m["usage"]);
            }
        }
        // Edit/Write tool results carry the hunks they applied; their +/- lines
        // are the same "code changes" figure the TUI reports.
        for h in v["toolUseResult"]["structuredPatch"].as_array().into_iter().flatten() {
            for l in h["lines"].as_array().into_iter().flatten() {
                match l.as_str().and_then(|s| s.chars().next()) {
                    Some('+') => added += 1,
                    Some('-') => removed += 1,
                    _ => {}
                }
            }
        }
    }
    if models.is_empty() { return None; }
    Some(SessionCost {
        usd: models.iter().map(|(m, b)| bucket_cost(m, b)).sum(),
        wall: if first <= last { last - first } else { 0 },
        added,
        removed,
    })
}

/// `/stats` (also `/usage`, `/cost`) — the whole Claude Code usage picture as a
/// `{% stats %}` card: rate-limit bars, this session's cost, the all-time totals
/// grid, activity heatmap, per-model split and behavior key-values.
///
/// History comes from Claude Code's own [`stats_cache`] when it is readable, plus
/// a live scan of the days it doesn't cover yet — the same two-part assembly the
/// TUI's Stats screen does, which is why the numbers land on it exactly. That
/// also turns a multi-gigabyte pass over the full history into a 40 KB read. If
/// the cache is missing or drifts we fall back to scanning everything, which
/// computes the same fields the slow way.
///
/// `limits_body` is the pre-fetched `limit|`/`kv|` lines from [`fetch_limits`]
/// ("" = section omitted).
fn stats(limits_body: &str, workdir: &str, session: Option<&str>) -> String {
    let files = jsonl_transcripts(&home().join(".claude/projects"));
    let cached = stats_cache();
    if files.is_empty() && cached.is_none() {
        return "📊 No usage data yet (no transcripts under `~/.claude/projects/`).".into();
    }
    let agg = match cached {
        Some((mut history, through)) => {
            history.merge(scan_transcripts(&files, Some(through + 1)));
            history
        }
        None => scan_transcripts(&files, None),
    };

    // Per-model bars, on the same input+output metric as the Tokens tile.
    let mut models: Vec<(String, u64)> = agg.models.iter().map(|(m, b)| (m.clone(), b.io())).filter(|(_, t)| *t > 0).collect();
    models.sort_by(|a, b| b.1.cmp(&a.1));
    models.truncate(5);

    let today = today_epoch_day();
    let mut day_epochs: Vec<i64> = agg.per_day.keys().copied().collect();
    day_epochs.sort_unstable();
    let active_days = day_epochs.len() as u64;
    let (cur_streak, best_streak) = streaks(&day_epochs, today);
    // Active days out of the calendar span since the first one — "141/191".
    let span_days = day_epochs.first().map(|f| today - f + 1).unwrap_or(0).max(active_days as i64);
    let busiest_day = agg.per_day.iter().max_by_key(|(_, n)| **n).map(|(d, _)| fmt_day_short(*d));

    // Heatmap: continuous per-day series ending today, last 20 weeks (the card
    // trims further to its width). offset = Monday-based weekday of the start.
    let start = day_epochs.first().copied().unwrap_or(today).max(today - 139);
    let heat: Vec<u64> = (start..=today).map(|d| agg.per_day.get(&d).copied().unwrap_or(0)).collect();
    // Sparkline only as the short-history fallback — otherwise the two would
    // show the SAME daily series twice.
    let spark: Vec<u64> = day_epochs.iter().rev().take(45).rev().map(|d| agg.per_day[d]).collect();

    let hour = (0..24usize).filter(|&h| agg.per_hour[h] > 0).max_by_key(|&h| agg.per_hour[h])
        .map(|h| format!("{h:02}:00"))
        .unwrap_or_default();

    let mut body = String::new();
    // Rate-limit bars first (the thing people actually check).
    for l in limits_body.lines().filter(|l| l.starts_with("limit|")) {
        body.push_str(l);
        body.push('\n');
    }
    // This chat's session, then the all-time tiles.
    if let Some(s) = session_cost(workdir, session) {
        body.push_str(&format!("tile|This session|{} · {}\n", fmt_usd(s.usd), fmt_dur(s.wall)));
        if s.added + s.removed > 0 {
            body.push_str(&format!("tile|Code changes|+{} / −{}\n", s.added, s.removed));
        }
    }
    let all_cost = agg.cost();
    if all_cost > 0.0 { body.push_str(&format!("tile|All-time cost|{}\n", fmt_usd(all_cost))); }
    if let Some(m) = agg.favorite() { body.push_str(&format!("tile|Favorite|{m}\n")); }
    if let Some(d) = busiest_day { body.push_str(&format!("tile|Most active|{d}\n")); }
    if cur_streak > 0 { body.push_str(&format!("tile|Streak|{cur_streak}d\n")); }
    if best_streak > cur_streak { body.push_str(&format!("tile|Best streak|{best_streak}d\n")); }
    if agg.longest_sess > 0 { body.push_str(&format!("tile|Longest session|{}\n", fmt_dur(agg.longest_sess))); }
    if heat.len() > 6 {
        body.push_str(&format!(
            "heat|{}|{}\n",
            weekday_mon0(start),
            heat.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(","),
        ));
    } else if spark.len() > 1 {
        body.push_str(&format!("spark|{}\n", spark.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",")));
    }
    for (m, tok) in &models {
        body.push_str(&format!("model|{}|{}|{}\n", m, humanize(*tok), tok));
    }
    // Behavior key-values (plan, updated-at) last, plus today's context profile —
    // the structured limits endpoint carries no behaviour data, so this is
    // derived from the transcripts we already walked.
    for l in limits_body.lines().filter(|l| l.starts_with("kv|")) {
        body.push_str(l);
        body.push('\n');
    }
    if agg.today_turns > 0 {
        body.push_str(&format!(
            "kv|Today|{} turns · {}% at >150k ctx\n",
            agg.today_turns,
            agg.today_big_ctx * 100 / agg.today_turns,
        ));
    }

    format!(
        "{{% stats sessions=\"{}\" messages=\"{}\" tools=\"{}\" tokens=\"{}\" days=\"{}\" since=\"{}\" hour=\"{}\" %}}\n{}{{% /stats %}}",
        humanize(agg.sessions), humanize(agg.messages), humanize(agg.tools), humanize(agg.tokens_io()),
        format_args!("{active_days}/{span_days}"), fmt_date(&agg.first_iso), hour, body,
    )
}

// ───────────────────────── rate limits (live) ─────────────────────────

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Claude Code's OAuth access token, from wherever the platform keeps it:
/// `~/.claude/.credentials.json` on Linux/Windows, the login Keychain on macOS.
///
/// None when it is missing, unreadable, or already expired. We deliberately do
/// NOT use the refresh token — minting credentials is Claude Code's job, and an
/// expired one simply drops us to the cached copy on the next line.
fn oauth_token() -> Option<String> {
    let raw = match std::fs::read_to_string(home().join(".claude/.credentials.json")) {
        Ok(s) => s,
        Err(_) => {
            if !cfg!(target_os = "macos") { return None; }
            let out = std::process::Command::new("security")
                .args(["find-generic-password", "-s", "Claude Code-credentials", "-w"])
                .output().ok()?;
            if !out.status.success() { return None; }
            String::from_utf8(out.stdout).ok()?
        }
    };
    let v: serde_json::Value = serde_json::from_str(raw.trim()).ok()?;
    let o = &v["claudeAiOauth"];
    match o["expiresAt"].as_i64() {
        Some(exp) if exp <= now_ms() => return None,
        _ => {}
    }
    o["accessToken"].as_str().map(str::to_string)
}

/// `GET /api/oauth/usage` — the exact request Claude Code makes to refresh its
/// own `cachedUsageUtilization` (its bundle: `fetchUtilization: GET
/// /api/oauth/usage`, 5s timeout).
///
/// Measured at 0.70–0.93s, versus 5.32s to shell out to `claude -p /usage` —
/// and unlike the spawn it starts no session and burns no quota, which matters
/// because that spawn was measuring the thing by consuming it. None on any
/// failure, so the caller falls through to the cache.
async fn fetch_utilization_live() -> Option<serde_json::Value> {
    let token = oauth_token()?;
    let res = reqwest::Client::new()
        .get("https://api.anthropic.com/api/oauth/usage")
        .bearer_auth(token)
        .header("Content-Type", "application/json")
        .timeout(Duration::from_secs(5))
        .send().await.ok()?;
    if !res.status().is_success() { return None; }
    res.json::<serde_json::Value>().await.ok()
}

/// Claude Code's cached copy of the same payload, plus its age in seconds.
/// Instant, but it only refreshes when a Claude Code process starts — measured
/// 14 minutes stale while sitting inside one long turn.
fn cached_utilization() -> Option<(serde_json::Value, i64)> {
    let raw = std::fs::read_to_string(home().join(".claude.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let c = &v["cachedUsageUtilization"];
    let age = (now_ms() - c["fetchedAtMs"].as_i64()?) / 1000;
    Some((c["utilization"].clone(), age.max(0)))
}

/// The plan's rate-limit tier as a label: `default_claude_max_20x` → "Max (20x)".
/// (`seatTier` and `userRateLimitTier` sit next to it and are both null — this is
/// the field that actually carries the tier.)
fn plan_tier() -> Option<String> {
    let raw = std::fs::read_to_string(home().join(".claude.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&raw).ok()?;
    let t = v["oauthAccount"]["organizationRateLimitTier"].as_str()?;
    let t = t.strip_prefix("default_").unwrap_or(t);
    let t = t.strip_prefix("claude_").unwrap_or(t);
    Some(match t.split_once('_') {
        Some((base, mult)) if mult.ends_with('x') => format!("{} ({mult})", cap_first(base)),
        _ => cap_first(t),
    })
}

/// Render the structured utilization payload into `limit|`/`kv|` card lines.
///
/// `limits[]` is already exactly the three rows the UI wants — no prose to scrape:
/// `{kind, percent, severity, resets_at, scope.model.display_name, is_active}`.
/// Reset times are rendered RELATIVE ("resets in 4h 44m"): the payload is UTC and
/// we have no timezone database, and it reads better anyway — it's how Claude's
/// own panel puts it.
fn parse_utilization(util: &serde_json::Value, age_secs: i64) -> String {
    let now = now_ms() / 1000;
    let mut out = String::new();
    for l in util["limits"].as_array().into_iter().flatten() {
        let Some(pct) = l["percent"].as_f64() else { continue };
        let label = match l["kind"].as_str().unwrap_or("") {
            "session" => "Session".to_string(),
            "weekly_all" => "Week (all models)".to_string(),
            "weekly_scoped" => format!(
                "Week ({})",
                l["scope"]["model"]["display_name"].as_str().unwrap_or("scoped"),
            ),
            other => cap_first(&other.replace('_', " ")),
        };
        let note = match l["resets_at"].as_str().and_then(iso_epoch_secs) {
            Some(at) if at > now => format!("resets in {}", fmt_dur(at - now)),
            _ if pct == 0.0 => "not used yet".to_string(),
            _ => String::new(),
        };
        out.push_str(&format!("limit|{label}|{}|{note}\n", pct.round() as i64));
    }
    if out.is_empty() { return String::new(); }
    if let Some(p) = plan_tier() { out.push_str(&format!("kv|Plan|{p}\n")); }
    out.push_str(&format!(
        "kv|Updated|{}\n",
        if age_secs < 45 { "just now".to_string() } else { format!("{} ago", fmt_dur(age_secs)) },
    ));
    out
}

/// The subscription rate-limit rows, cheapest live source first.
///
/// 1. `/api/oauth/usage` (~0.8s, live, no quota) — the same call Claude Code makes.
/// 2. Its on-disk cache (instant, up to ~15min stale) — labelled with its age.
/// 3. Scraping `claude -p /usage` (5.3s, burns quota) — only if the first two are
///    unavailable, e.g. no readable credential.
///
/// Best-effort throughout: "" means the card simply omits the limits section.
async fn fetch_limits() -> String {
    if let Some(u) = fetch_utilization_live().await {
        let s = parse_utilization(&u, 0);
        if !s.is_empty() { return s; }
    }
    if let Some((u, age)) = cached_utilization() {
        let s = parse_utilization(&u, age);
        if !s.is_empty() { return s; }
    }
    parse_usage_text(&run_claude_stdin("/usage", 30).await)
}

/// Parse the plain-text `/usage` report into `limit|label|pct|note` +
/// `kv|label|value` card lines. Line shapes (v2.1.x):
///
/// ```text
/// You are currently using your subscription to power your Claude Code usage
/// Current session: 5% used · resets Jul 3 at 9:19am (Asia/Shanghai)
/// Current week (all models): 23% used · resets Jul 3 at 8:59pm (Asia/Shanghai)
/// Last 24h · 1634 requests · 11 sessions
///   94% of your usage was at >150k context
///   Top skills: /claude-api 1%
/// ```
///
/// Every branch is prefix-matched and skips silently on drift; the behavior
/// profile + Top rows keep the LAST occurrence (the 7d block supersedes 24h).
fn parse_usage_text(text: &str) -> String {
    let mut plan: Option<String> = None;
    let mut limits: Vec<String> = vec![];
    let mut windows: Vec<(String, String)> = vec![]; // "Last 24h" → "1634 requests · 11 sessions"
    let mut profile: Vec<String> = vec![]; // behavior lines of the CURRENT window block
    let mut tops: Vec<(String, String)> = vec![]; // "Top skills" → "…" (last wins)

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() { continue; }

        if let Some(rest) = line.strip_prefix("You are currently using your ") {
            if let Some(p) = rest.split(" to power").next() {
                plan = Some(p.trim().to_string());
            }
        } else if line.starts_with("Current ") && line.contains("% used") {
            let Some((head, tail)) = line.split_once(':') else { continue };
            let label = cap_first(head.trim_start_matches("Current ").trim());
            let tail = tail.trim();
            let pct: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
            if pct.is_empty() { continue; }
            let note = tail.split('·').nth(1).map(|s| strip_trailing_paren(s.trim())).unwrap_or_default();
            limits.push(format!("limit|{label}|{pct}|{note}"));
        } else if line.starts_with("Last ") && line.contains('·') {
            let mut it = line.splitn(2, '·');
            let win = it.next().unwrap_or("").trim().to_string();
            let val = it.next().unwrap_or("").trim().to_string();
            if !val.is_empty() {
                windows.push((win, val));
                profile.clear(); // behavior lines that follow belong to this window
            }
        } else if line.contains("% of your usage") {
            let pct: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
            let tag = if line.contains(">150k") { ">150k ctx" }
                else if line.contains("8+ hours") { "8h+ sessions" }
                else if line.contains("subagent") { "subagent-heavy" }
                else { continue };
            if !pct.is_empty() { profile.push(format!("{pct}% {tag}")); }
        } else if let Some((label, val)) = ["Top skills", "Top subagents", "Top MCP servers"]
            .iter()
            .find_map(|k| line.strip_prefix(&format!("{k}:")).map(|v| (k.to_string(), v.trim().to_string())))
        {
            if let Some(e) = tops.iter_mut().find(|(l, _)| *l == label) {
                e.1 = val; // last (7d) wins
            } else {
                tops.push((label, val));
            }
        }
    }

    let mut out = String::new();
    for l in &limits { out.push_str(l); out.push('\n'); }
    if let Some(p) = plan { out.push_str(&format!("kv|Plan|{p}\n")); }
    for (w, v) in &windows { out.push_str(&format!("kv|{w}|{v}\n")); }
    if !profile.is_empty() { out.push_str(&format!("kv|Profile (7d)|{}\n", profile.join(" · "))); }
    for (l, v) in &tops { out.push_str(&format!("kv|{l}|{}\n", v.replace(", ", " · "))); }
    out
}

/// Pipe `input` into a headless `claude -p` and return its (ANSI-stripped)
/// output, or "" on any failure/timeout.
async fn run_claude_stdin(input: &str, secs: u64) -> String {
    let mut cmd = tokio::process::Command::new("claude");
    cmd.arg("-p").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::platform::no_window(&mut cmd);
    let fut = async {
        let mut child = cmd.spawn()?;
        if let Some(mut si) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = si.write_all(input.as_bytes()).await;
            let _ = si.write_all(b"\n").await;
            // si drops here → stdin closes → claude runs the one command and exits.
        }
        child.wait_with_output().await
    };
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(Ok(o)) => {
            let s = String::from_utf8_lossy(&o.stdout).to_string();
            strip_ansi(s.trim()).to_string()
        }
        _ => String::new(),
    }
}

// ───────────────────────── date/duration helpers ─────────────────────────

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`] — epoch day → (year, month, day).
fn civil_from_days(z: i64) -> (i64, usize, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m as usize, d)
}

/// epoch day → "Mar 9" (the year is implied by the card's "since").
fn fmt_day_short(epoch_day: i64) -> String {
    const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (_, m, d) = civil_from_days(epoch_day);
    format!("{} {d}", MON[m.clamp(1, 12) - 1])
}

/// Epoch day of a transcript line's `"timestamp"`, read straight out of the raw
/// JSON text — the per-day pass runs over every line of every transcript, so it
/// cannot afford to parse them.
fn line_day(line: &str) -> Option<i64> {
    let i = line.find("\"timestamp\":\"")? + 13;
    day_key_epoch(line.get(i..i + 10)?)
}

/// "YYYY-MM-DD" → epoch day.
fn day_key_epoch(key: &str) -> Option<i64> {
    let y = key.get(0..4)?.parse().ok()?;
    let m = key.get(5..7)?.parse().ok()?;
    let d = key.get(8..10)?.parse().ok()?;
    Some(days_from_civil(y, m, d))
}

/// ISO "YYYY-MM-DDTHH:MM:SS…" → epoch seconds (sub-second/zone ignored; the
/// transcripts are always UTC "Z").
fn iso_epoch_secs(ts: &str) -> Option<i64> {
    let day = day_key_epoch(ts.get(0..10)?)?;
    let h: i64 = ts.get(11..13)?.parse().ok()?;
    let mi: i64 = ts.get(14..16)?.parse().ok()?;
    let s: i64 = ts.get(17..19)?.parse().ok()?;
    Some(day * 86400 + h * 3600 + mi * 60 + s)
}

/// Today as an epoch day (UTC — the transcripts' clock).
fn today_epoch_day() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86400) as i64)
        .unwrap_or(0)
}

/// Monday-based weekday (Mon=0…Sun=6) of an epoch day. Day 0 (1970-01-01) was
/// a Thursday.
fn weekday_mon0(epoch_day: i64) -> i64 {
    (epoch_day.rem_euclid(7) + 3) % 7
}

/// (current, best) streak of consecutive active days. `days` must be sorted
/// ascending + unique. The current streak counts only if it reaches today or
/// yesterday (an idle gap breaks it).
fn streaks(days: &[i64], today: i64) -> (u64, u64) {
    let mut best = 0u64;
    let mut run = 0u64;
    let mut prev = i64::MIN;
    for &d in days {
        run = if d == prev + 1 { run + 1 } else { 1 };
        if run > best { best = run; }
        prev = d;
    }
    let mut current = 0u64;
    if let Some(&last) = days.last() {
        if last >= today - 1 {
            current = 1;
            let mut expect = last - 1;
            for &d in days.iter().rev().skip(1) {
                if d == expect { current += 1; expect -= 1; } else { break; }
            }
        }
    }
    (current, best)
}

/// Seconds → "35d 0h" / "9h 24m" / "42m".
pub(crate) fn fmt_dur(secs: i64) -> String {
    let d = secs / 86400;
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    if d > 0 { format!("{d}d {h}h") } else if h > 0 { format!("{h}h {m}m") } else { format!("{}m", m.max(1)) }
}

/// Uppercase the first ASCII letter ("week (all models)" → "Week (all models)").
fn cap_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// Strip one trailing " (…)" parenthetical ("resets Jul 3 at 9:19am (Asia/Shanghai)"
/// → "resets Jul 3 at 9:19am") — the timezone eats card width for nothing.
fn strip_trailing_paren(s: &str) -> String {
    if s.ends_with(')') {
        if let Some(i) = s.rfind(" (") {
            return s[..i].to_string();
        }
    }
    s.to_string()
}

/// All `*.jsonl` transcripts under `<root>/<project>/` (one project dir per cwd).
fn jsonl_transcripts(root: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let Ok(projects) = std::fs::read_dir(root) else { return out };
    for proj in projects.flatten() {
        let Ok(entries) = std::fs::read_dir(proj.path()) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().and_then(|x| x.to_str()) == Some("jsonl") {
                out.push(p);
            }
        }
    }
    out
}

/// `claude --version` → "2.1.198" (first token; "" if the CLI is missing).
pub(crate) async fn claude_version() -> String {
    let out = run_claude(&["--version"], 8).await;
    out.split_whitespace().next().unwrap_or("").trim_start_matches('v').to_string()
}

/// Claude Code's per-project transcript dir for a workdir — the CLI's own
/// munge: every non-alphanumeric byte becomes `-`.
pub(crate) fn project_dir(workdir: &str) -> PathBuf {
    let munged: String = workdir.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    home().join(".claude/projects").join(munged)
}

/// Estimate the CURRENT context size of a resumed session: the last assistant
/// turn's input-side usage (input + cache read + cache creation) from its
/// transcript under `~/.claude/projects/<munged workdir>/<session>.jsonl`.
/// Tails the last 256 KiB — transcripts can be huge and the answer is at the
/// end. None on any miss (no transcript, format drift, weird session id).
pub(crate) fn session_context_tokens(workdir: &str, session_id: &str) -> Option<u64> {
    // Session ids are our own UUIDs; refuse anything path-ish anyway.
    if session_id.is_empty() || !session_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
        return None;
    }
    let path = project_dir(workdir).join(format!("{session_id}.jsonl"));

    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(&path).ok()?;
    let len = f.metadata().ok()?.len();
    const TAIL: u64 = 256 * 1024;
    let start = len.saturating_sub(TAIL);
    f.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    f.take(TAIL).read_to_end(&mut bytes).ok()?;
    // Lossy: a mid-file seek can land inside a UTF-8 sequence.
    let buf = String::from_utf8_lossy(&bytes);
    let body = if start > 0 {
        // Drop the partial first line from the mid-file seek.
        buf.split_once('\n').map(|(_, rest)| rest).unwrap_or("")
    } else {
        &buf
    };
    for line in body.lines().rev() {
        if !line.contains("\"type\":\"assistant\"") { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v["type"].as_str() != Some("assistant") { continue; }
        let u = &v["message"]["usage"];
        let ctx: u64 = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
            .iter().filter_map(|k| u[*k].as_u64()).sum();
        if ctx > 0 { return Some(ctx); }
    }
    None
}

// ───────────────────────── /resume: session listing ─────────────────────────

/// One resumable transcript in a project dir — enough for the `/resume` picker.
pub(crate) struct SessionMeta {
    pub id: String,
    /// Seconds since the transcript was last written.
    pub age_secs: i64,
    /// Claude's own rolling summary, else the first real user prompt ("" if
    /// neither) — the picker's one-line description.
    pub preview: String,
}

/// The workdir's resumable Claude Code sessions, newest-first — the same
/// transcripts the TUI's own `/resume` picker lists for that directory.
pub(crate) fn list_project_sessions(workdir: &str) -> Vec<SessionMeta> {
    let dir = project_dir(workdir);
    let mut rows: Vec<(String, std::time::SystemTime)> = vec![];
    let Ok(entries) = std::fs::read_dir(&dir) else { return vec![] };
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()) != Some("jsonl") { continue; }
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else { continue };
        if !stem.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') { continue; }
        let Some(mtime) = e.metadata().ok().and_then(|md| md.modified().ok()) else { continue };
        rows.push((stem.to_string(), mtime));
    }
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    let now = std::time::SystemTime::now();
    rows.into_iter()
        .map(|(id, mtime)| {
            let age_secs = now.duration_since(mtime).map(|d| d.as_secs() as i64).unwrap_or(0);
            let head = read_head(&dir.join(format!("{id}.jsonl")), 96 * 1024);
            SessionMeta { id, age_secs, preview: preview_from_head(&head) }
        })
        .collect()
}

/// First `max` bytes of a file, lossy ("" on any miss).
fn read_head(path: &Path, max: u64) -> String {
    use std::io::Read;
    let Ok(f) = std::fs::File::open(path) else { return String::new() };
    let mut bytes = Vec::new();
    let _ = f.take(max).read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Pull a one-line preview out of a transcript head: a `summary` line when the
/// session carries one (continued/compacted sessions do), else the first real
/// user prompt — skipping meta lines, command stubs and interrupt notices, and
/// stripping the daemon's own injected context blocks down to the trigger text.
pub(crate) fn preview_from_head(head: &str) -> String {
    for line in head.lines() {
        if line.starts_with("{\"type\":\"summary\"") {
            if let Some(s) = serde_json::from_str::<serde_json::Value>(line)
                .ok()
                .and_then(|v| v["summary"].as_str().map(str::to_string))
            {
                if !s.trim().is_empty() { return clip(&s, 48); }
            }
            continue;
        }
        if !line.contains("\"type\":\"user\"") { continue; }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        if v["type"].as_str() != Some("user") || v["isMeta"].as_bool() == Some(true) { continue; }
        let c = &v["message"]["content"];
        let text = match c {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Array(items) => items
                .iter()
                .find_map(|b| (b["type"].as_str() == Some("text")).then(|| b["text"].as_str().unwrap_or("").to_string()))
                .unwrap_or_default(),
            _ => String::new(),
        };
        // The daemon prefixes group turns with bracketed context blocks; the
        // real trigger is whatever follows the last END marker.
        let t = ["[END RECENT CONVERSATION — now handle the triggering message below.]", "[END AVAILABLE APPS & ROOMS]"]
            .iter()
            .fold(text.trim(), |acc, marker| acc.rsplit(marker).next().unwrap_or(acc).trim());
        if t.is_empty() || t.starts_with("Caveat:") || t.starts_with('<') || t.starts_with("[Request interrupted") {
            continue;
        }
        return clip(t, 48);
    }
    String::new()
}

/// Seconds → "just now" / "5m ago" / "3h ago" / "2d ago".
pub(crate) fn fmt_age(secs: i64) -> String {
    if secs < 60 { "just now".into() }
    else if secs < 3600 { format!("{}m ago", secs / 60) }
    else if secs < 86400 { format!("{}h ago", secs / 3600) }
    else { format!("{}d ago", secs / 86400) }
}

/// An interactive `claude` (TUI) session alive right now.
pub(crate) struct LiveTui {
    pub session_id: String,
    pub cwd: String,
    /// "busy" | "idle" | "" (older CLIs don't report one).
    pub status: String,
}

/// Live TUI sessions from Claude Code's own registry
/// (`~/.claude/sessions/<pid>.json`). Every claude process writes one — real
/// terminals as `entrypoint:"cli"`, the daemon's own stream-json turns as
/// `"sdk-cli"` — so only `cli` entries count (else a bot's in-flight turn tags
/// itself "open in a TUI"). Exited TUIs leave their file behind, so an entry
/// only counts while its pid is still alive. Windows: no signal-0 probe, so
/// the live tags simply don't show there — `/resume` itself still works.
pub(crate) fn live_tui_sessions() -> Vec<LiveTui> {
    let mut out: Vec<LiveTui> = vec![];
    let Ok(entries) = std::fs::read_dir(home().join(".claude/sessions")) else { return out };
    for e in entries.flatten() {
        let Ok(text) = std::fs::read_to_string(e.path()) else { continue };
        let Some((pid, l)) = parse_live_entry(&text) else { continue };
        if !pid_alive(pid) { continue; }
        // Two registry entries can claim one session (a TUI relaunched via
        // `--resume`); busy beats idle.
        if let Some(prev) = out.iter_mut().find(|x| x.session_id == l.session_id) {
            if prev.status != "busy" && l.status == "busy" { *prev = l; }
        } else {
            out.push(l);
        }
    }
    out
}

/// Parse one live-registry entry; None for anything that isn't a real
/// terminal session (non-interactive kinds, sdk-cli entrypoints, drift).
pub(crate) fn parse_live_entry(text: &str) -> Option<(u32, LiveTui)> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v["kind"].as_str() != Some("interactive") || v["entrypoint"].as_str() != Some("cli") { return None; }
    let pid = v["pid"].as_u64()? as u32;
    let session_id = v["sessionId"].as_str()?.to_string();
    let cwd = v["cwd"].as_str().unwrap_or("").to_string();
    let status = v["status"].as_str().unwrap_or("").to_string();
    Some((pid, LiveTui { session_id, cwd, status }))
}

#[cfg(unix)]
fn pid_alive(pid: u32) -> bool {
    // Same-user probe: signal 0 delivers nothing, just checks existence.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}
#[cfg(not(unix))]
fn pid_alive(_pid: u32) -> bool {
    false
}

/// `/resume <arg>` resolution over the (newest-first) session list.
pub(crate) enum Resolve<'a> {
    One(&'a SessionMeta),
    NotFound,
    Ambiguous(usize),
}

pub(crate) fn resolve_session<'a>(metas: &'a [SessionMeta], arg: &str) -> Resolve<'a> {
    if arg.eq_ignore_ascii_case("last") {
        return metas.first().map(Resolve::One).unwrap_or(Resolve::NotFound);
    }
    let hits: Vec<&SessionMeta> = metas.iter().filter(|m| m.id.starts_with(arg)).collect();
    match hits.len() {
        0 => Resolve::NotFound,
        1 => Resolve::One(hits[0]),
        n => Resolve::Ambiguous(n),
    }
}

/// 1_234_567 → "1.2M" (K/M/B, trailing `.0` stripped).
pub(crate) fn humanize(n: u64) -> String {
    let f = n as f64;
    let s = if f >= 1e9 { format!("{:.1}B", f / 1e9) }
        else if f >= 1e6 { format!("{:.1}M", f / 1e6) }
        else if f >= 1e3 { format!("{:.1}K", f / 1e3) }
        else { return n.to_string() };
    s.replace(".0B", "B").replace(".0M", "M").replace(".0K", "K")
}

/// "claude-opus-4-5-20251101" → "opus-4-5".
fn short_model(m: &str) -> String {
    let m = m.strip_prefix("claude-").unwrap_or(m);
    if let Some((head, tail)) = m.rsplit_once('-') {
        if tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit()) {
            return head.to_string();
        }
    }
    m.to_string()
}

/// ISO date → "Jan 23, 2026".
fn fmt_date(iso: &str) -> String {
    if iso.len() < 10 { return String::new(); }
    const MON: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let mi = iso[5..7].parse::<usize>().unwrap_or(1).clamp(1, 12) - 1;
    let day = iso[8..10].parse::<u32>().unwrap_or(1);
    format!("{} {}, {}", MON[mi], day, &iso[0..4])
}

// ───────────────────────── terminal-only mocks ─────────────────────────

const MOCK: &[(&str, &str)] = &[
    ("vim", "toggle Vim editing mode in the prompt"),
    ("theme", "change the color theme"),
    ("color", "set the prompt bar color"),
    ("terminal-setup", "configure terminal keybindings"),
    ("tui", "set the terminal UI renderer"),
    ("scroll-speed", "adjust mouse wheel scroll speed"),
    ("voice", "toggle voice dictation"),
    ("chrome", "configure Claude in Chrome"),
    ("desktop", "continue the session in the Desktop app"),
    ("mobile", "show a QR code for the mobile app"),
    ("radio", "open Claude FM lo-fi radio"),
    ("stickers", "order Claude Code stickers"),
    ("passes", "share a free week of Claude Code"),
    ("powerup", "interactive feature lessons"),
    ("focus", "toggle focus view"),
    ("fast", "toggle fast mode"),
    ("diff", "open the interactive diff viewer"),
    ("heapdump", "write a JS heap snapshot"),
    ("exit", "exit the CLI"),
    ("ide", "manage IDE integrations"),
    ("install-github-app", "set up the GitHub Actions app"),
    ("install-slack-app", "install the Slack app"),
    ("web-setup", "connect a GitHub account to Claude Code web"),
    ("upgrade", "open the plan upgrade page"),
    ("copy", "copy the last response to the clipboard"),
    ("keybindings-help", "customize keyboard shortcuts"),
];

fn mock_desc(name: &str) -> Option<&'static str> {
    MOCK.iter().find(|(c, _)| *c == name).map(|(_, d)| *d)
}

fn mock_reply(name: &str) -> String {
    let desc = mock_desc(name).unwrap_or("a terminal-only setting");
    format!("🖥️ `/{name}` — {desc}.\nThis is a Claude Code terminal-UI command, so it only works in the interactive `claude` on the host — not through chat.")
}

// ───────────────────────── helpers ─────────────────────────

async fn run_claude(args: &[&str], secs: u64) -> String {
    let mut cmd = tokio::process::Command::new("claude");
    cmd.args(args).stdin(Stdio::null());
    crate::platform::no_window(&mut cmd);
    let fut = cmd.output();
    match tokio::time::timeout(Duration::from_secs(secs), fut).await {
        Ok(Ok(o)) => {
            let mut s = String::from_utf8_lossy(&o.stdout).to_string();
            if s.trim().is_empty() { s = String::from_utf8_lossy(&o.stderr).to_string(); }
            cap_chars(strip_ansi(&s).trim(), 3000)
        }
        Ok(Err(e)) => format!("(couldn't run `claude {}`: {e})", args.join(" ")),
        Err(_) => format!("(`claude {}` timed out)", args.join(" ")),
    }
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

fn read_capped(path: &Path, max: usize) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| cap_chars(&s, max))
}

fn cap_chars(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        format!("{}\n… (truncated)", s.chars().take(max).collect::<String>())
    } else {
        s.to_string()
    }
}

fn fence(lang: &str, body: &str) -> String {
    format!("```{lang}\n{}\n```", body.trim_end())
}

fn fence_block(title: &str, lang: &str, body: &str) -> String {
    format!("{title}\n{}", fence(lang, body))
}

fn clip(s: &str, max: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > max { format!("{}…", one.chars().take(max - 1).collect::<String>()) } else { one }
}

/// Pull a `description:` value from a markdown file's YAML frontmatter.
fn frontmatter_desc(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut lines = text.lines();
    if lines.next()?.trim_end() != "---" { return None; }
    for l in lines {
        if l.trim_end() == "---" { break; }
        if let Some(rest) = l.strip_prefix("description:") {
            let v = rest.trim().trim_matches('"').trim_matches('\'').trim();
            if !v.is_empty() { return Some(v.to_string()); }
        }
    }
    None
}

pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // skip a CSI/escape sequence up to its final letter
            while let Some(n) = chars.next() {
                if n.is_ascii_alphabetic() { break; }
            }
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // Captured verbatim from `echo "/usage" | claude -p` (Claude Code 2.1.x).
    const USAGE_SAMPLE: &str = "\
You are currently using your subscription to power your Claude Code usage

Current session: 5% used · resets Jul 3 at 9:19am (Asia/Shanghai)
Current week (all models): 23% used · resets Jul 3 at 8:59pm (Asia/Shanghai)
Current week (Fable): 20% used · resets Jul 3 at 8:59pm (Asia/Shanghai)

What's contributing to your limits usage?
Approximate, based on local sessions on this machine — does not include other devices or claude.ai. Behaviors are independent characteristics, not a breakdown.

Last 24h · 1634 requests · 11 sessions
  94% of your usage was at >150k context
  51% of your usage came from sessions active for 8+ hours
  41% of your usage came from subagent-heavy sessions
  Top skills: /claude-api 1%
  Top subagents: Explore 1%, Plan 1%
  Top MCP servers: browser-use 2%

Last 7d · 7983 requests · 30 sessions
  91% of your usage was at >150k context
  72% of your usage came from sessions active for 8+ hours
  71% of your usage came from subagent-heavy sessions
  Top subagents: workflow-subagent 2%, Explore 1%, general-purpose 1%, Plan 1%
  Top MCP servers: browser-use 1%
";

    #[test]
    fn parses_usage_limits() {
        let body = parse_usage_text(USAGE_SAMPLE);
        assert!(body.contains("limit|Session|5|resets Jul 3 at 9:19am\n"), "{body}");
        assert!(body.contains("limit|Week (all models)|23|resets Jul 3 at 8:59pm\n"), "{body}");
        assert!(body.contains("limit|Week (Fable)|20|resets Jul 3 at 8:59pm\n"), "{body}");
        assert!(body.contains("kv|Plan|subscription\n"), "{body}");
        assert!(body.contains("kv|Last 24h|1634 requests · 11 sessions\n"), "{body}");
        assert!(body.contains("kv|Last 7d|7983 requests · 30 sessions\n"), "{body}");
        // Behavior profile + Top rows come from the LAST (7d) block.
        assert!(body.contains("kv|Profile (7d)|91% >150k ctx · 72% 8h+ sessions · 71% subagent-heavy\n"), "{body}");
        assert!(body.contains("kv|Top subagents|workflow-subagent 2% · Explore 1% · general-purpose 1% · Plan 1%\n"), "{body}");
        assert!(body.contains("kv|Top MCP servers|browser-use 1%\n"), "{body}");
        // Top skills only appeared in the 24h block — still kept.
        assert!(body.contains("kv|Top skills|/claude-api 1%\n"), "{body}");
    }

    #[test]
    fn parse_survives_garbage() {
        assert_eq!(parse_usage_text(""), "");
        assert_eq!(parse_usage_text("error: not logged in\nsomething else"), "");
    }

    #[test]
    fn civil_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(day_key_epoch("1970-01-02"), Some(1));
        assert_eq!(days_from_civil(2026, 7, 3) - days_from_civil(2026, 7, 2), 1);
        // 1970-01-05 was a Monday.
        assert_eq!(weekday_mon0(4), 0);
        assert_eq!(weekday_mon0(0), 3); // Thursday
        assert_eq!(iso_epoch_secs("1970-01-01T00:01:30.000Z"), Some(90));
    }

    #[test]
    fn streaks_math() {
        // active d10..d12 and d14; "today" = d14 → current 1, best 3
        assert_eq!(streaks(&[10, 11, 12, 14], 14), (1, 3));
        // run reaching yesterday still counts as current
        assert_eq!(streaks(&[10, 11, 12, 13], 14), (4, 4));
        // stale run → no current streak
        assert_eq!(streaks(&[10, 11, 12], 20), (0, 3));
        assert_eq!(streaks(&[], 20), (0, 0));
    }

    #[test]
    fn durations() {
        assert_eq!(fmt_dur(3_025_524), "35d 0h");
        assert_eq!(fmt_dur(33_840), "9h 24m");
        assert_eq!(fmt_dur(30), "1m");
    }

    #[test]
    fn ages() {
        assert_eq!(fmt_age(5), "just now");
        assert_eq!(fmt_age(300), "5m ago");
        assert_eq!(fmt_age(7200), "2h ago");
        assert_eq!(fmt_age(200_000), "2d ago");
    }

    #[test]
    fn preview_prefers_summary_then_first_real_prompt() {
        // Summary line wins even when a user line follows.
        let head = r#"{"type":"summary","summary":"Fixing the flaky auth test"}
{"type":"user","message":{"role":"user","content":"hello"}}"#;
        assert_eq!(preview_from_head(head), "Fixing the flaky auth test");

        // Meta/mode/snapshot lines and command stubs are skipped; the first
        // real prompt is picked, whitespace collapsed.
        let head = r#"{"type":"mode","mode":"normal"}
{"type":"file-history-snapshot","messageId":"x"}
{"type":"user","isMeta":true,"message":{"role":"user","content":"Caveat: injected"}}
{"type":"user","message":{"role":"user","content":"<command-name>/usage</command-name>"}}
{"type":"user","message":{"role":"user","content":"fix the   login bug"}}"#;
        assert_eq!(preview_from_head(head), "fix the login bug");

        // Daemon-injected context blocks are stripped down to the trigger; an
        // array-form content still yields its text block.
        let head = r#"{"type":"user","message":{"role":"user","content":"[RECENT CONVERSATION]\nnoise\n[END RECENT CONVERSATION — now handle the triggering message below.]\n\nship the release"}}"#;
        assert_eq!(preview_from_head(head), "ship the release");
        let head = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"array prompt"}]}}"#;
        assert_eq!(preview_from_head(head), "array prompt");
        assert_eq!(preview_from_head("garbage\nlines"), "");
    }

    #[test]
    fn resolves_sessions_by_prefix_and_last() {
        let metas = vec![
            SessionMeta { id: "aabb1111-x".into(), age_secs: 10, preview: String::new() },
            SessionMeta { id: "aacc2222-y".into(), age_secs: 20, preview: String::new() },
        ];
        assert!(matches!(resolve_session(&metas, "last"), Resolve::One(m) if m.id == "aabb1111-x"));
        assert!(matches!(resolve_session(&metas, "aacc"), Resolve::One(m) if m.id == "aacc2222-y"));
        assert!(matches!(resolve_session(&metas, "aa"), Resolve::Ambiguous(2)));
        assert!(matches!(resolve_session(&metas, "zz"), Resolve::NotFound));
        assert!(matches!(resolve_session(&[], "last"), Resolve::NotFound));
    }

    #[test]
    fn parses_live_registry_entries() {
        // Captured shape from ~/.claude/sessions/<pid>.json (Claude Code 2.1.x).
        let entry = r#"{"pid":80413,"sessionId":"149fe1e7-d58a-4f47-a194-d5f030927da2","cwd":"/Users/ops/Desktop","startedAt":1785033002454,"version":"2.1.220","kind":"interactive","entrypoint":"cli","status":"busy","updatedAt":1785033278170}"#;
        let (pid, l) = parse_live_entry(entry).expect("parses");
        assert_eq!(pid, 80413);
        assert_eq!(l.session_id, "149fe1e7-d58a-4f47-a194-d5f030927da2");
        assert_eq!(l.cwd, "/Users/ops/Desktop");
        assert_eq!(l.status, "busy");
        // Non-interactive kinds, the daemon's own sdk-cli turns (they register
        // too — kind "interactive", entrypoint "sdk-cli"), and drift are all
        // rejected; a missing status is tolerated.
        assert!(parse_live_entry(r#"{"pid":1,"sessionId":"x","kind":"print","entrypoint":"cli"}"#).is_none());
        assert!(parse_live_entry(r#"{"pid":3,"sessionId":"z","kind":"interactive","entrypoint":"sdk-cli"}"#).is_none());
        assert!(parse_live_entry("not json").is_none());
        let (_, l) = parse_live_entry(r#"{"pid":2,"sessionId":"y","cwd":"/w","kind":"interactive","entrypoint":"cli"}"#).unwrap();
        assert_eq!(l.status, "");
    }

    // ── machine-dependent smokes: read THIS machine's ~/.claude, run manually
    //    with `cargo test -- --ignored --nocapture` ──

    #[test]
    #[ignore = "reads this machine's ~/.claude transcripts"]
    fn stats_smoke_print() {
        // The repo root, not the crate dir — that's the workdir the daemon runs
        // in, so it's the one with transcripts behind the "This session" tiles.
        let cwd = std::env::current_dir().unwrap();
        let workdir = cwd.parent().unwrap_or(&cwd).display().to_string();
        // Name the session the cost tiles priced, so the number can be checked
        // against Claude Code's own `/usage` for that exact transcript.
        if let Some(m) = list_project_sessions(&workdir).into_iter().next() {
            if let Some(c) = session_cost(&workdir, Some(&m.id)) {
                println!("session {} → {} · {} · +{}/-{}", m.id, fmt_usd(c.usd), fmt_dur(c.wall), c.added, c.removed);
            }
        }
        let s = stats("", &workdir, None);
        println!("{s}");
        assert!(s.contains("{% stats "));
    }

    #[test]
    #[ignore = "reads this machine's settings.json"]
    fn settings_smoke_print() {
        let s = dump_settings(".");
        println!("{s}");
        assert!(s.contains("{% stats title=\"Settings\""));
    }

    #[tokio::test]
    #[ignore = "spawns a real `claude -p` (~4s)"]
    async fn limits_smoke_print() {
        let s = fetch_limits().await;
        println!("{s}");
    }

    #[test]
    #[ignore = "reads this machine's ~/.claude transcripts + live registry"]
    fn resume_listing_smoke() {
        let metas = list_project_sessions("/Users/ops/Desktop/mafold");
        println!("{} sessions; newest 6:", metas.len());
        for m in metas.iter().take(6) {
            println!("  {} · {} · {:?}", &m.id[..8], fmt_age(m.age_secs), m.preview);
        }
        // Newest-first ordering.
        assert!(metas.windows(2).all(|w| w[0].age_secs <= w[1].age_secs));
        for l in live_tui_sessions() {
            println!("live: {} · {} · {:?}", &l.session_id[..8], l.cwd, l.status);
        }
    }

    #[test]
    #[ignore = "reads this machine's ~/.claude transcripts"]
    fn session_context_smoke() {
        // The LARGEST transcript of this repo's project dir (tiny ones can be
        // aborted turns with all-zero usage) exercises the workdir munge +
        // tail-scan; its filename is the session id.
        let dir = home().join(".claude/projects/-Users-ops-Desktop-mafold");
        let Some(sid) = std::fs::read_dir(&dir).ok().and_then(|es| {
            es.flatten()
                .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
                .max_by_key(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
                .map(|e| e.path().file_stem().unwrap().to_string_lossy().to_string())
        }) else { return };
        let ctx = session_context_tokens("/Users/ops/Desktop/mafold", &sid);
        println!("session {sid} → context {ctx:?}");
        assert!(ctx.is_some());
    }
}
