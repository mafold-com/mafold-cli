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
pub async fn handle(name: &str, _arg: &str, workdir: &str) -> Outcome {
    match name {
        // ── usage stats (rich card): local transcript scan + live rate limits ──
        "stats" | "usage" | "cost" => Outcome::Reply(stats(&fetch_limits().await)),
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

/// `/stats` — Claude Code usage computed LIVE from the session transcripts under
/// `~/.claude/projects/<project>/*.jsonl`, rendered as a `{% stats %}` card
/// (rate-limit bars + totals grid + activity heatmap + sparkline + per-model
/// split + behavior key-values).
///
/// We do NOT read `~/.claude/stats-cache.json`: that aggregate is only flushed
/// periodically and routinely lags the real history by weeks, which made `/usage`
/// show stale numbers. The transcripts are written every turn, so reading them is
/// always current. Each assistant turn records its real token `usage` + `model`;
/// we sum per-model tokens, count tool-call blocks, active days, busiest hour,
/// streaks, the longest session span, and per-day activity (heatmap + sparkline).
/// Cost: one full pass over the history per call (~1s); only
/// `"type":"assistant"` lines are JSON-parsed. `limits_body` is the pre-fetched
/// `limit|`/`kv|` lines from [`fetch_limits`] ("" = section omitted).
fn stats(limits_body: &str) -> String {
    use std::collections::{HashMap, HashSet};

    let files = jsonl_transcripts(&home().join(".claude/projects"));
    if files.is_empty() {
        return "📊 No usage data yet (no transcripts under `~/.claude/projects/`).".into();
    }

    let mut model_tokens: HashMap<String, u64> = HashMap::new();
    let mut total_tokens: u64 = 0;
    let mut tools: u64 = 0;
    let mut messages: u64 = 0;
    let mut sessions: HashSet<String> = HashSet::new();
    let mut sess_span: HashMap<String, (i64, i64)> = HashMap::new(); // sessionId → (first, last) epoch secs
    let mut per_day: HashMap<String, u64> = HashMap::new(); // YYYY-MM-DD → assistant turns
    let mut per_hour: [u64; 24] = [0; 24];
    let mut first_date: Option<String> = None;

    for f in &files {
        let Ok(bytes) = std::fs::read(f) else { continue };
        // Lossy so a single bad byte never drops a whole transcript.
        for line in String::from_utf8_lossy(&bytes).lines() {
            // Cheap pre-filter: skip the (many) non-assistant lines without parsing.
            if !line.contains("\"type\":\"assistant\"") { continue; }
            let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
            if v["type"].as_str() != Some("assistant") { continue; }
            let m = &v["message"];
            let u = &m["usage"];

            // Token total — same four buckets the old cache summed.
            let tok: u64 = ["input_tokens", "output_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"]
                .iter().filter_map(|k| u[*k].as_u64()).sum();
            if tok > 0 {
                total_tokens += tok;
                if let Some(model) = m["model"].as_str() {
                    *model_tokens.entry(short_model(model)).or_default() += tok;
                }
            }

            messages += 1;
            if let Some(content) = m["content"].as_array() {
                tools += content.iter().filter(|b| b["type"].as_str() == Some("tool_use")).count() as u64;
            }
            if let Some(sid) = v["sessionId"].as_str() { sessions.insert(sid.to_string()); }
            if let Some(ts) = v["timestamp"].as_str() {
                if ts.len() >= 10 {
                    *per_day.entry(ts[..10].to_string()).or_default() += 1;
                    if ts.len() >= 13 {
                        if let Ok(h) = ts[11..13].parse::<usize>() {
                            if h < 24 { per_hour[h] += 1; }
                        }
                    }
                    if first_date.as_deref().is_none_or(|f| ts < f) {
                        first_date = Some(ts.to_string());
                    }
                    if let (Some(sid), Some(secs)) = (v["sessionId"].as_str(), iso_epoch_secs(ts)) {
                        let e = sess_span.entry(sid.to_string()).or_insert((secs, secs));
                        if secs < e.0 { e.0 = secs; }
                        if secs > e.1 { e.1 = secs; }
                    }
                }
            }
        }
    }

    // Per-model token totals (top 5).
    let mut models: Vec<(String, u64)> = model_tokens.into_iter().collect();
    models.sort_by(|a, b| b.1.cmp(&a.1));
    models.truncate(5);

    // Active days + daily-turn sparkline (chronological, last 45 days).
    let mut day_keys: Vec<String> = per_day.keys().cloned().collect();
    day_keys.sort();
    let days = day_keys.len() as u64;
    let mut spark: Vec<u64> = day_keys.iter().map(|d| per_day[d]).collect();
    if spark.len() > 45 { spark = spark[spark.len() - 45..].to_vec(); }

    // Streaks (consecutive active days, UTC — same clock as the timestamps).
    let today = today_epoch_day();
    let day_epochs: Vec<i64> = day_keys.iter().filter_map(|k| day_key_epoch(k)).collect();
    let (cur_streak, best_streak) = streaks(&day_epochs, today);

    // Longest single session by wall-clock span (daemon-resumed sessions can
    // legitimately span days).
    let longest_sess = sess_span.values().map(|(a, b)| b - a).max().unwrap_or(0);

    // Heatmap: continuous per-day series ending today, last 20 weeks (the card
    // trims further to its width). offset = Monday-based weekday of the start.
    let day_counts: HashMap<i64, u64> = per_day.iter()
        .filter_map(|(k, v)| day_key_epoch(k).map(|d| (d, *v)))
        .collect();
    let start = day_epochs.first().copied().unwrap_or(today).max(today - 139);
    let heat: Vec<u64> = (start..=today).map(|d| day_counts.get(&d).copied().unwrap_or(0)).collect();

    // Busiest hour.
    let hour = (0..24usize).filter(|&h| per_hour[h] > 0).max_by_key(|&h| per_hour[h])
        .map(|h| format!("{h:02}:00"))
        .unwrap_or_default();
    let since = fmt_date(first_date.as_deref().unwrap_or(""));

    let mut body = String::new();
    // Rate-limit bars first (the thing people actually check).
    for l in limits_body.lines().filter(|l| l.starts_with("limit|")) {
        body.push_str(l);
        body.push('\n');
    }
    if cur_streak > 0 { body.push_str(&format!("tile|Streak|{cur_streak}d\n")); }
    if best_streak > cur_streak { body.push_str(&format!("tile|Best streak|{best_streak}d\n")); }
    if longest_sess > 0 { body.push_str(&format!("tile|Longest session|{}\n", fmt_dur(longest_sess))); }
    // Heatmap when there's enough history; sparkline only as the short-history
    // fallback (they'd otherwise show the SAME daily series twice).
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
    // Behavior key-values (plan, request windows, top skills/subagents/MCP) last.
    for l in limits_body.lines().filter(|l| l.starts_with("kv|")) {
        body.push_str(l);
        body.push('\n');
    }

    format!(
        "{{% stats sessions=\"{}\" messages=\"{}\" tools=\"{}\" tokens=\"{}\" days=\"{}\" since=\"{}\" hour=\"{}\" %}}\n{}{{% /stats %}}",
        humanize(sessions.len() as u64), humanize(messages), humanize(tools), humanize(total_tokens), days, since, hour, body,
    )
}

// ───────────────────────── rate limits (live) ─────────────────────────

/// Fetch the subscription rate-limit report by piping `/usage` into a headless
/// `claude -p` (~3.5s) and parse it into card body lines. The percentages exist
/// NOWHERE on disk — this spawn is the only programmatic source. Best-effort:
/// any failure/timeout/format drift returns "" and the card simply omits the
/// limits section.
async fn fetch_limits() -> String {
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
    let munged: String = workdir.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let path = home().join(".claude/projects").join(munged).join(format!("{session_id}.jsonl"));

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

    // ── machine-dependent smokes: read THIS machine's ~/.claude, run manually
    //    with `cargo test -- --ignored --nocapture` ──

    #[test]
    #[ignore = "reads this machine's ~/.claude transcripts"]
    fn stats_smoke_print() {
        let s = stats("");
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
