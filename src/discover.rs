//! Discover the local Claude Code skills + slash commands so the agent can
//! publish them as the bot's command panel (the chat "/" menu). Anyone chatting
//! the bot then sees — and can tap — every skill/command available on the
//! machine the daemon runs on. Routing is automatic: a typed `/name …` that
//! isn't a daemon control command is forwarded verbatim to `claude -p`, which
//! resolves the skill/command in headless mode (slash commands == skills there).
//!
//! Best-effort: a missing directory or an unparseable file is skipped, never
//! fatal. We read YAML frontmatter by hand (no serde_yaml dep) — only the
//! scalar `name` / `description` / `argument-hint` fields, including folded
//! (`>`) and literal (`|`) blocks.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Names handled by the daemon itself (prepended as control commands) — kept out
/// of the discovered/built-in list so the menu never shows duplicates.
const RESERVED: &[&str] = &["clear", "new", "stop", "model", "status", "cwd", "help", "account", "login"];

/// Built-in Claude Code slash commands (v2.1.x). These have no file to scan, so
/// they're listed here. The daemon emulates many of them (config dumps, /login,
/// /logout); terminal-only ones reply with a note; the rest forward to `claude`.
/// (Removed in current versions and excluded: `pr-comments`, `vim`.)
const BUILTINS: &[(&str, &str)] = &[
    (
        "add-dir",
        "Add a working directory for file access this session",
    ),
    (
        "advisor",
        "Enable/disable the advisor tool for model guidance",
    ),
    ("agents", "Manage / list subagent configurations"),
    (
        "autofix-pr",
        "Watch a PR and push fixes on CI failures/comments",
    ),
    ("background", "Detach the session as a background agent"),
    (
        "batch",
        "Decompose large codebase changes into parallel tasks",
    ),
    ("branch", "Create a conversation branch at this point"),
    ("btw", "Ask a quick side question without bloating history"),
    ("cd", "Move the session to a new working directory"),
    ("chrome", "Configure Claude in Chrome"),
    (
        "claude-api",
        "Load the Claude API reference + language helpers",
    ),
    ("code-review", "Review the diff for bugs and cleanups"),
    ("color", "Set the prompt bar color"),
    ("compact", "Summarize the conversation to free up context"),
    ("config", "Show settings (settings.json)"),
    ("context", "Visualize current context usage"),
    ("copy", "Copy the last response to the clipboard"),
    ("cost", "Show session cost and usage"),
    ("debug", "Enable debug logging and troubleshoot"),
    (
        "deep-research",
        "Fan out web searches and synthesize a report",
    ),
    ("desktop", "Continue the session in the Desktop app"),
    ("diff", "Open the interactive diff viewer"),
    ("doctor", "Diagnose and verify the Claude Code install"),
    ("effort", "Set the model effort level"),
    ("exit", "Exit the CLI"),
    ("export", "Export the conversation as plain text"),
    ("fast", "Toggle fast mode"),
    ("feedback", "Submit feedback or report a bug"),
    (
        "fewer-permission-prompts",
        "Scan transcripts to reduce permission prompts",
    ),
    ("focus", "Toggle focus view"),
    ("fork", "Spawn a forked subagent on a directive"),
    ("goal", "Set a goal for Claude to work toward"),
    ("heapdump", "Write a JS heap snapshot for memory diagnosis"),
    ("hooks", "Show hook configurations for tool events"),
    ("ide", "Manage IDE integrations"),
    ("init", "Initialize the project with a CLAUDE.md guide"),
    ("insights", "Generate a report analyzing your sessions"),
    ("install-github-app", "Set up the Claude GitHub Actions app"),
    ("install-slack-app", "Install the Claude Slack app"),
    ("keybindings", "Show the keyboard shortcuts file"),
    ("login", "Sign in to your Anthropic account"),
    ("logout", "Sign out from your Anthropic account"),
    ("loop", "Run a prompt repeatedly on a schedule"),
    ("mcp", "Show MCP server connections"),
    ("memory", "Show CLAUDE.md / auto-memory"),
    ("mobile", "Show a QR code to download the mobile app"),
    ("passes", "Share a free week of Claude Code"),
    ("permissions", "Show tool permission rules"),
    ("plan", "Enter plan mode from the prompt"),
    ("plugin", "Manage / list Claude Code plugins"),
    ("powerup", "Discover features via interactive lessons"),
    ("privacy-settings", "View privacy settings"),
    ("radio", "Open Claude FM lo-fi radio"),
    ("recap", "Generate a one-line session summary"),
    ("release-notes", "View the changelog"),
    ("reload-plugins", "Reload active plugins without restarting"),
    ("reload-skills", "Re-scan skill directories for new skills"),
    (
        "remote-control",
        "Make the session available for remote control",
    ),
    (
        "remote-env",
        "Choose the default environment for cloud agents",
    ),
    ("rename", "Rename the current session"),
    ("resume", "Resume a conversation by ID or name"),
    ("review", "Review a pull request"),
    ("rewind", "Rewind the conversation/code to a previous point"),
    ("run", "Launch and drive the project app"),
    (
        "run-skill-generator",
        "Teach /run and /verify to build/launch the app",
    ),
    ("sandbox", "Toggle sandbox mode"),
    ("schedule", "Create/list/run scheduled cloud routines"),
    ("scroll-speed", "Adjust mouse wheel scroll speed"),
    (
        "security-review",
        "Analyze pending changes for security issues",
    ),
    ("setup-bedrock", "Configure Amazon Bedrock authentication"),
    ("setup-vertex", "Configure Google Vertex AI authentication"),
    ("simplify", "Review code for cleanup and apply fixes"),
    ("skills", "List available skills"),
    ("stats", "Show usage stats"),
    ("statusline", "Show / configure the status line"),
    ("stickers", "Order Claude Code stickers"),
    ("tasks", "View background tasks"),
    ("team-onboarding", "Generate a team onboarding guide"),
    (
        "teleport",
        "Pull a Claude Code web session into the terminal",
    ),
    ("terminal-setup", "Configure terminal keybindings"),
    ("theme", "Change the color theme"),
    ("tui", "Set the terminal UI renderer"),
    ("ultraplan", "Draft a plan in an ultraplan session"),
    (
        "ultrareview",
        "Run a deep multi-agent code review in the cloud",
    ),
    ("upgrade", "Open the plan upgrade page"),
    ("usage", "Show session cost and plan usage limits"),
    ("usage-credits", "Configure usage credits for work limits"),
    ("verify", "Confirm a code change works (build/run/observe)"),
    ("voice", "Toggle voice dictation"),
    ("web-setup", "Connect a GitHub account to Claude Code web"),
    ("workflows", "Open the workflow progress view"),
];

/// One discovered command/skill, ready to become a Mafold BotCommand.
struct Found {
    description: String,
    arg_hint: Option<String>,
}

/// Discover everything and return a JSON array of `{command, description,
/// arg_hint?}` (the shape `setBotCommands` expects). Sorted by name; the caller
/// prepends the daemon control commands.
pub fn all(workdir: &str) -> Value {
    // Keyed by command name; first writer wins, so scan most-specific first
    // (project → user → plugins → built-ins) and a project override shadows a
    // same-named user skill.
    let mut found: BTreeMap<String, Found> = BTreeMap::new();

    // 1. Project-local (the workdir the agent operates in).
    if let Ok(wd) = Path::new(workdir).canonicalize() {
        scan_commands(&wd.join(".claude/commands"), "", &mut found);
        scan_skills(&wd.join(".claude/skills"), "", &mut found);
    }
    // 2. User-level.
    let home = home_dir();
    scan_commands(&home.join(".claude/commands"), "", &mut found);
    scan_skills(&home.join(".claude/skills"), "", &mut found);
    // 3. Installed plugins.
    scan_plugins(&home, &mut found);
    // 4. Built-ins.
    for (cmd, desc) in BUILTINS {
        found.entry((*cmd).to_string()).or_insert(Found {
            description: (*desc).to_string(),
            arg_hint: None,
        });
    }

    let arr: Vec<Value> = found
        .into_iter()
        // Drop internal/private entries (e.g. `vercel:_conventions`) — a leading
        // underscore on the final segment is the convention for "not user-facing".
        .filter(|(command, _)| !command.rsplit(':').next().unwrap_or("").starts_with('_'))
        // Drop names the daemon already publishes as control commands.
        .filter(|(command, _)| !RESERVED.contains(&command.as_str()))
        .map(|(command, f)| {
            let mut o = json!({ "command": command, "description": clip(&f.description, 160) });
            if let Some(h) = f.arg_hint {
                o["arg_hint"] = json!(clip(&h, 60));
            }
            o
        })
        .collect();
    Value::Array(arr)
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// Scan a `commands/` dir for `*.md` slash commands. `prefix` namespaces plugin
/// commands (`vercel:`); empty for user/project. Nested dirs join with `:`
/// (Claude Code's `/group:cmd` convention).
fn scan_commands(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Found>) {
    walk_md(dir, dir, prefix, out);
}

fn walk_md(root: &Path, dir: &Path, prefix: &str, out: &mut BTreeMap<String, Found>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_md(root, &path, prefix, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") {
            // name = path under root, without extension, '/'→':'.
            let rel = path.strip_prefix(root).unwrap_or(&path);
            let stem = rel
                .with_extension("")
                .to_string_lossy()
                .replace(std::path::MAIN_SEPARATOR, ":");
            let name = format!("{prefix}{stem}");
            let fm = read_frontmatter(&path);
            let description = fm
                .get("description")
                .cloned()
                .unwrap_or_else(|| format!("/{name}"));
            let arg_hint = fm
                .get("argument-hint")
                .or_else(|| fm.get("argument_hint"))
                .cloned();
            out.entry(name).or_insert(Found {
                description,
                arg_hint,
            });
        }
    }
}

/// Scan a `skills/` dir — each `<skill>/SKILL.md` becomes one command.
fn scan_skills(dir: &Path, prefix: &str, out: &mut BTreeMap<String, Found>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let skill_md = entry.path().join("SKILL.md");
        if !skill_md.is_file() {
            continue;
        }
        let fm = read_frontmatter(&skill_md);
        let dir_name = entry.file_name().to_string_lossy().into_owned();
        let base = fm.get("name").cloned().unwrap_or(dir_name);
        let name = format!("{prefix}{base}");
        let description = fm
            .get("description")
            .cloned()
            .unwrap_or_else(|| format!("/{name}"));
        out.entry(name).or_insert(Found {
            description,
            arg_hint: None,
        });
    }
}

/// Scan installed plugins (`~/.claude/plugins/installed_plugins.json`) → their
/// `commands/` + `skills/`, namespaced `plugin:name`.
fn scan_plugins(home: &Path, out: &mut BTreeMap<String, Found>) {
    let manifest = home.join(".claude/plugins/installed_plugins.json");
    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return;
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return;
    };
    let Some(plugins) = v["plugins"].as_object() else {
        return;
    };
    for (full_id, installs) in plugins {
        // "vercel@claude-plugins-official" → plugin short name "vercel".
        let short = full_id.split('@').next().unwrap_or(full_id);
        let prefix = format!("{short}:");
        // Newest install wins (last with a usable installPath).
        let install_path = installs
            .as_array()
            .and_then(|arr| arr.iter().rev().find_map(|i| i["installPath"].as_str()));
        let Some(p) = install_path else { continue };
        let root = PathBuf::from(p);
        scan_commands(&root.join("commands"), &prefix, out);
        scan_skills(&root.join("skills"), &prefix, out);
    }
}

/// Read the leading `--- … ---` YAML frontmatter into a flat key→value map.
/// Handles single-line scalars (quoted or bare) and folded/literal blocks
/// (`>` / `|`). Nested mappings and lists are ignored (we only want scalars).
fn read_frontmatter(path: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let Ok(text) = std::fs::read_to_string(path) else {
        return map;
    };
    let mut lines = text.lines();
    if lines.next().map(str::trim_end) != Some("---") {
        return map;
    }
    // Collect frontmatter lines up to the closing '---'.
    let body: Vec<&str> = lines.take_while(|l| l.trim_end() != "---").collect();

    let mut i = 0;
    while i < body.len() {
        let line = body[i];
        i += 1;
        let trimmed = line.trim_start();
        // Top-level key only (no indentation) of the form `key: …`.
        if line.starts_with(char::is_whitespace) || trimmed.starts_with('#') {
            continue;
        }
        let Some(colon) = trimmed.find(':') else {
            continue;
        };
        let key = trimmed[..colon].trim().to_lowercase();
        let mut rest = trimmed[colon + 1..].trim().to_string();

        if rest.starts_with('>') || rest.starts_with('|') {
            // Folded/literal block: gather the following more-indented lines.
            let mut parts: Vec<String> = Vec::new();
            while i < body.len() {
                let next = body[i];
                if next.trim().is_empty() {
                    i += 1;
                    continue;
                }
                if !next.starts_with(char::is_whitespace) {
                    break; // a new top-level key
                }
                parts.push(next.trim().to_string());
                i += 1;
            }
            rest = parts.join(" ");
        } else {
            rest = unquote(&rest);
            // Skip block openers with no inline value (`key:` then a list).
            if rest.is_empty() {
                continue;
            }
        }
        if !key.is_empty() {
            map.insert(key, rest);
        }
    }
    map
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    if (s.starts_with('"') && s.ends_with('"') && s.len() >= 2)
        || (s.starts_with('\'') && s.ends_with('\'') && s.len() >= 2)
    {
        s[1..s.len() - 1].to_string()
    } else {
        s.to_string()
    }
}

/// Collapse whitespace and cap length for a clean one-line menu entry.
fn clip(s: &str, max: usize) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > max {
        format!("{}…", one.chars().take(max - 1).collect::<String>())
    } else {
        one
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test against the machine's real ~/.claude — prints the discovered
    /// menu and checks each entry has a non-empty command + description.
    #[test]
    fn discovers_local_skills() {
        let Value::Array(items) = all(".") else {
            panic!("expected an array")
        };
        eprintln!("discovered {} commands:", items.len());
        for it in &items {
            let cmd = it["command"].as_str().unwrap_or("");
            let desc = it["description"].as_str().unwrap_or("");
            assert!(!cmd.is_empty(), "empty command in {it}");
            assert!(!desc.is_empty(), "empty description for /{cmd}");
            eprintln!("  /{cmd} — {desc}");
        }
    }
}
