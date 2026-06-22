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
        // ── usage stats (rich card) ──
        "stats" | "usage" | "cost" => Outcome::Reply(stats()),
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

fn dump_settings(workdir: &str) -> String {
    let files = [
        ("user", home().join(".claude/settings.json")),
        ("project", PathBuf::from(workdir).join(".claude/settings.json")),
        ("project (local)", PathBuf::from(workdir).join(".claude/settings.local.json")),
    ];
    let mut out = String::from("⚙️ **Settings**\n");
    let mut any = false;
    for (label, p) in files {
        if let Some(body) = read_capped(&p, 3500) {
            any = true;
            out.push_str(&format!("\n**{label}** · `{}`\n{}\n", p.display(), fence("json", &body)));
        }
    }
    if !any { out.push_str("\n_No settings files found._"); }
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

/// `/stats` — Claude Code usage from `~/.claude/stats-cache.json`, rendered as a
/// `{% stats %}` card (totals grid + daily-activity sparkline + per-model split).
fn stats() -> String {
    let path = home().join(".claude/stats-cache.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return "📊 No usage data yet (`~/.claude/stats-cache.json` not found).".into();
    };
    let Ok(d) = serde_json::from_str::<serde_json::Value>(&text) else {
        return "📊 Couldn't read the stats cache.".into();
    };

    let sessions = d["totalSessions"].as_u64().unwrap_or(0);
    let messages = d["totalMessages"].as_u64().unwrap_or(0);

    // Per-model token totals (top 5).
    let mut models: Vec<(String, u64)> = vec![];
    let mut total_tokens: u64 = 0;
    if let Some(mu) = d["modelUsage"].as_object() {
        for (m, u) in mu {
            let tok: u64 = ["inputTokens", "outputTokens", "cacheReadInputTokens", "cacheCreationInputTokens"]
                .iter().filter_map(|k| u[*k].as_u64()).sum();
            total_tokens += tok;
            models.push((short_model(m), tok));
        }
    }
    models.sort_by(|a, b| b.1.cmp(&a.1));
    models.truncate(5);

    // Daily activity → tool-call total, active days, message-count sparkline.
    let mut tools: u64 = 0;
    let mut days = 0u64;
    let mut spark: Vec<u64> = vec![];
    if let Some(da) = d["dailyActivity"].as_array() {
        days = da.len() as u64;
        for e in da {
            tools += e["toolCallCount"].as_u64().unwrap_or(0);
            spark.push(e["messageCount"].as_u64().unwrap_or(0));
        }
    }
    if spark.len() > 45 { spark = spark[spark.len() - 45..].to_vec(); }

    // Busiest hour.
    let hour = d["hourCounts"].as_object()
        .and_then(|h| h.iter().max_by_key(|(_, v)| v.as_u64().unwrap_or(0)).map(|(k, _)| k.clone()))
        .and_then(|k| k.parse::<u32>().ok())
        .map(|h| format!("{h:02}:00"))
        .unwrap_or_default();
    let since = fmt_date(d["firstSessionDate"].as_str().unwrap_or(""));

    let mut body = String::new();
    for (m, tok) in &models {
        body.push_str(&format!("model|{}|{}|{}\n", m, humanize(*tok), tok));
    }
    if spark.len() > 1 {
        body.push_str(&format!("spark|{}\n", spark.iter().map(|n| n.to_string()).collect::<Vec<_>>().join(",")));
    }

    format!(
        "{{% stats sessions=\"{}\" messages=\"{}\" tools=\"{}\" tokens=\"{}\" days=\"{}\" since=\"{}\" hour=\"{}\" %}}\n{}{{% /stats %}}",
        humanize(sessions), humanize(messages), humanize(tools), humanize(total_tokens), days, since, hour, body,
    )
}

/// 1_234_567 → "1.2M" (K/M/B, trailing `.0` stripped).
fn humanize(n: u64) -> String {
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
