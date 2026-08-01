//! Harness-agnostic renderer: maps normalized [`AgentEvent`]s to the chat's
//! markdoc text + agent cards (tool / diff / todo / bash / thinking / result).
//! Any harness that emits `AgentEvent`s gets identical card rendering for free.

use serde_json::Value;
use std::collections::HashMap;

use crate::harness::AgentEvent;

/// Render one event to a markdoc string (text or a card), or `None` to skip.
/// `names` tracks `tool_use_id → tool name` so a bash `tool_result` can be
/// matched to its call.
pub fn render(ev: &AgentEvent, names: &mut HashMap<String, String>) -> Option<String> {
    match ev {
        AgentEvent::Text(t) => Some(t.clone()),
        AgentEvent::ToolCall { id, name, input } => {
            names.insert(id.clone(), name.to_lowercase());
            Some(tool_use_tag(name, input))
        }
        AgentEvent::ToolResult { id, text } => {
            if names.get(id).map(String::as_str) != Some("bash") {
                return None;
            }
            let out = text.trim();
            if out.is_empty() {
                return None;
            }
            Some(format!("\n{{% mafold/bash %}}\n{}{{% /mafold/bash %}}\n", block_esc(&cap_lines(out, 20))))
        }
        AgentEvent::Thinking(t) => {
            let t = t.trim();
            if t.is_empty() {
                return None;
            }
            Some(format!("\n{{% mafold/thinking %}}\n{}{{% /mafold/thinking %}}\n", block_esc(&cap_lines(t, 14))))
        }
        AgentEvent::Done { duration_ms, cost_usd, tokens } => result_tag(*duration_ms, *cost_usd, *tokens),
        AgentEvent::Session(_) => None,
        // Heartbeat only — consumed by the render loop's generating card, never
        // rendered as content.
        AgentEvent::Pulse { .. } => None,
        // Not rendered as new content — the render loop stamps it into the
        // already-emitted ask card via `stamp_ask_answered`.
        AgentEvent::AskAnswered(_) => None,
    }
}

/// Stamp the user's answer into the pending (last unanswered) `{% mafold/ask %}` card
/// in `full` by rewriting its opening tag to `{% mafold/ask answered="…" %}`. The
/// message content itself is the durable record — a reloaded page or another
/// device renders the card answered instead of re-offering the buttons. A
/// stamped opener no longer matches the bare `{% mafold/ask %}` needle, so an
/// already-answered card can never be re-stamped. Returns false when no
/// unanswered ask card is present.
pub fn stamp_ask_answered(full: &mut String, answer: &str) -> bool {
    const OPEN: &str = "{% mafold/ask %}";
    let Some(pos) = full.rfind(OPEN) else { return false };
    let val = answered_attr(answer);
    full.replace_range(pos..pos + OPEN.len(), &format!("{{% mafold/ask answered=\"{val}\" %}}"));
    true
}

/// Finalized-message variant of [`stamp_ask_answered`] — for asks the model
/// emitted in its reply TEXT (no blocking hook; the turn already ended when
/// the answer arrives). Returns the stamped content, or None when the message
/// has no unanswered ask card.
pub fn stamp_unanswered_ask(content: &str, answer: &str) -> Option<String> {
    let mut out = content.to_string();
    stamp_ask_answered(&mut out, answer).then_some(out)
}

/// The `answered` attribute value: `attr_esc`'s cleanup but with a cap wide
/// enough for a multi-question answer (headers + labels), and never empty —
/// a blank reply still has to flip the card to answered.
fn answered_attr(answer: &str) -> String {
    let cleaned: String = answer
        .chars()
        .map(|c| match c { '"' => '\'', '\n' | '\r' | '\t' => ' ', _ => c })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return "✓".into();
    }
    if cleaned.chars().count() > 300 {
        format!("{}…", cleaned.chars().take(300).collect::<String>())
    } else {
        cleaned.to_string()
    }
}

/// The summary category for a tool call — drives the `{% mafold/run %}` group label.
/// `None` for anything that isn't a counted action (text, tool results, thinking,
/// the interactive ask).
pub fn tool_kind(ev: &AgentEvent) -> Option<&'static str> {
    match ev {
        AgentEvent::ToolCall { name, .. } => Some(match name.to_lowercase().as_str() {
            "bash" => "shell",
            "read" | "notebookedit" => "read",
            "edit" | "write" | "multiedit" | "apply_patch" => "edit",
            "glob" | "grep" => "search",
            "webfetch" | "websearch" => "web",
            "task" => "task",
            "todowrite" => "plan",
            "askuserquestion" => return None,
            _ => "tool",
        }),
        _ => None,
    }
}

/// A human label for a group of consecutive tool calls, from per-category counts:
/// "Ran 2 shell commands", "Read 1 file, ran 1 shell command", …
pub fn run_summary(counts: &HashMap<&'static str, usize>) -> String {
    let n = |c: &str| counts.get(c).copied().unwrap_or(0);
    let phrase = |n: usize, sing: &str, plur: &str| if n == 1 { format!("1 {sing}") } else { format!("{n} {plur}") };
    let mut parts: Vec<String> = Vec::new();
    if n("read") > 0 { parts.push(format!("read {}", phrase(n("read"), "file", "files"))); }
    if n("edit") > 0 { parts.push(format!("edited {}", phrase(n("edit"), "file", "files"))); }
    if n("shell") > 0 { parts.push(format!("ran {}", phrase(n("shell"), "shell command", "shell commands"))); }
    if n("search") > 0 { parts.push(format!("ran {}", phrase(n("search"), "search", "searches"))); }
    if n("web") > 0 { parts.push(format!("ran {}", phrase(n("web"), "web search", "web searches"))); }
    if n("task") > 0 { parts.push(format!("ran {}", phrase(n("task"), "subagent", "subagents"))); }
    if n("plan") > 0 { parts.push("updated the plan".into()); }
    if n("tool") > 0 { parts.push(format!("ran {}", phrase(n("tool"), "tool", "tools"))); }
    if parts.is_empty() {
        return "Details".into();
    }
    let joined = parts.join(", ");
    let mut ch = joined.chars();
    ch.next().map(|f| f.to_uppercase().collect::<String>() + ch.as_str()).unwrap_or(joined)
}

/// Wrap one consecutive group of primitive cards in a collapsible `{% mafold/run %}`
/// with its human summary. `body` is already valid markdoc (the nested cards),
/// so it is NOT escaped; each primitive escapes its own body.
pub fn run_card(summary: &str, body: &str) -> String {
    format!("\n{{% mafold/run summary=\"{}\" %}}\n{}{{% /mafold/run %}}\n", attr_esc(summary), body)
}

/// A tool call → the right card.
fn tool_use_tag(name: &str, input: &Value) -> String {
    match name.to_lowercase().as_str() {
        "todowrite" => todo_tag(input),
        "edit" | "multiedit" => diff_tag_edit(&name.to_lowercase(), input),
        "write" => diff_tag_write(input),
        "task" => format!(
            "\n{{% mafold/task subagent=\"{}\" desc=\"{}\" /%}}\n",
            attr_esc(input["subagent_type"].as_str().unwrap_or("agent")),
            attr_esc(input["description"].as_str().unwrap_or("")),
        ),
        "webfetch" => format!("\n{{% mafold/web url=\"{}\" /%}}\n", attr_esc(input["url"].as_str().unwrap_or(""))),
        "websearch" => format!("\n{{% mafold/web query=\"{}\" /%}}\n", attr_esc(input["query"].as_str().unwrap_or(""))),
        "skill" => {
            let sname = input["command"].as_str()
                .or_else(|| input["skill"].as_str())
                .or_else(|| input["name"].as_str())
                .unwrap_or("skill");
            let args = input["args"].as_str().or_else(|| input["arguments"].as_str()).unwrap_or("");
            format!("\n{{% mafold/skill name=\"{}\" args=\"{}\" /%}}\n", attr_esc(sname), attr_esc(args))
        }
        "askuserquestion" => ask_tag(input),
        _ => format!("\n{{% mafold/tool name=\"{}\" detail=\"{}\" /%}}\n", attr_esc(name), attr_esc(&tool_detail(name, input))),
    }
}

/// AskUserQuestion → an interactive `ask` card. The body is line-encoded (mirrors
/// the `stats` card): one `q|<header>|<multi 0/1>|<question>` per question, each
/// followed by its `o|<label>|<description>` options. The web/iOS renderer turns
/// every option into a tappable button and posts the chosen label(s) back as the
/// user's next message — which `--resume` feeds straight into the same session.
fn ask_tag(input: &Value) -> String {
    let mut body = String::new();
    if let Some(questions) = input["questions"].as_array() {
        for q in questions {
            let header = cell_esc(q["header"].as_str().unwrap_or(""));
            let multi = if q["multiSelect"].as_bool().unwrap_or(false) { 1 } else { 0 };
            let question = cell_esc(q["question"].as_str().unwrap_or(""));
            body.push_str(&format!("q|{header}|{multi}|{question}\n"));
            if let Some(opts) = q["options"].as_array() {
                for o in opts {
                    let label = cell_esc(o["label"].as_str().unwrap_or(""));
                    let desc = cell_esc(o["description"].as_str().unwrap_or(""));
                    body.push_str(&format!("o|{label}|{desc}\n"));
                }
            }
        }
    }
    format!("\n{{% mafold/ask %}}\n{}{{% /mafold/ask %}}\n", block_esc(&body))
}

/// One pipe-delimited cell: newlines/tabs/pipes collapse to spaces (the `|`
/// delimiter must stay unambiguous), trimmed and length-capped.
fn cell_esc(s: &str) -> String {
    let one: String = s
        .chars()
        .map(|c| match c { '\n' | '\r' | '\t' | '|' => ' ', _ => c })
        .collect();
    let one = one.trim();
    if one.chars().count() > 200 {
        format!("{}…", one.chars().take(200).collect::<String>())
    } else {
        one.to_string()
    }
}

fn todo_tag(input: &Value) -> String {
    let mut body = String::new();
    if let Some(items) = input["todos"].as_array() {
        for t in items {
            let content = t["content"].as_str().or_else(|| t["activeForm"].as_str()).unwrap_or("");
            let mark = match t["status"].as_str().unwrap_or("pending") {
                "completed" => 'x',
                "in_progress" => '~',
                _ => ' ',
            };
            body.push_str(&format!("[{mark}] {}\n", line_esc(content)));
        }
    }
    format!("\n{{% mafold/todo %}}\n{}{{% /mafold/todo %}}\n", block_esc(&body))
}

fn diff_tag_edit(lname: &str, input: &Value) -> String {
    let file = input["file_path"].as_str().unwrap_or("");
    let (added, removed, body) = if lname == "multiedit" {
        let mut a = 0; let mut r = 0; let mut body = String::new();
        if let Some(edits) = input["edits"].as_array() {
            for e in edits {
                let (ea, er, eb) = synth_hunk(e["old_string"].as_str().unwrap_or(""), e["new_string"].as_str().unwrap_or(""));
                a += ea; r += er; body.push_str(&eb);
            }
        }
        (a, r, body)
    } else {
        synth_hunk(input["old_string"].as_str().unwrap_or(""), input["new_string"].as_str().unwrap_or(""))
    };
    diff_tag(file, added, removed, &body)
}

fn diff_tag_write(input: &Value) -> String {
    let file = input["file_path"].as_str().unwrap_or("");
    let content = input["content"].as_str().unwrap_or("");
    let lines: Vec<&str> = if content.is_empty() { vec![] } else { content.lines().collect() };
    let mut body = String::new();
    for l in &lines { body.push('+'); body.push_str(l); body.push('\n'); }
    diff_tag(file, lines.len(), 0, &body)
}

fn diff_tag(file: &str, added: usize, removed: usize, body: &str) -> String {
    format!(
        "\n{{% mafold/diff file=\"{}\" added={} removed={} %}}\n{}{{% /mafold/diff %}}\n",
        attr_esc(file), added, removed, block_esc(&cap_lines(body, 24)),
    )
}

fn synth_hunk(old: &str, new: &str) -> (usize, usize, String) {
    let oldl: Vec<&str> = if old.is_empty() { vec![] } else { old.lines().collect() };
    let newl: Vec<&str> = if new.is_empty() { vec![] } else { new.lines().collect() };
    let mut body = String::new();
    for l in &oldl { body.push('-'); body.push_str(l); body.push('\n'); }
    for l in &newl { body.push('+'); body.push_str(l); body.push('\n'); }
    (newl.len(), oldl.len(), body)
}

fn result_tag(dur: Option<f64>, cost: Option<f64>, tokens: Option<u64>) -> Option<String> {
    let cost = cost.filter(|c| *c > 0.0);
    if dur.is_none() && cost.is_none() && tokens.is_none() {
        return None;
    }
    let mut attrs = String::new();
    if let Some(d) = dur { attrs.push_str(&format!(" duration=\"{:.1}s\"", d / 1000.0)); }
    if let Some(t) = tokens { attrs.push_str(&format!(" tokens=\"{}\"", fmt_count(t))); }
    if let Some(c) = cost { attrs.push_str(&format!(" cost=\"${c:.4}\"")); }
    Some(format!("\n{{% mafold/result{attrs} /%}}\n"))
}

/// A short, human-readable detail for a generic tool card (file, command, …).
fn tool_detail(name: &str, input: &Value) -> String {
    let raw = match name.to_lowercase().as_str() {
        "bash" => input["command"].as_str(),
        "edit" | "write" | "multiedit" | "read" | "notebookedit" | "apply_patch" => input["file_path"].as_str(),
        "glob" | "grep" => input["pattern"].as_str(),
        "webfetch" => input["url"].as_str(),
        "task" => input["description"].as_str(),
        "todowrite" => Some("updating plan"),
        _ => None,
    };
    raw.unwrap_or("").to_string()
}

fn fmt_count(n: u64) -> String {
    if n >= 1000 { format!("{:.1}k", n as f64 / 1000.0) } else { n.to_string() }
}

fn cap_lines(s: &str, max: usize) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= max {
        let mut out = lines.join("\n");
        if !out.is_empty() { out.push('\n'); }
        out
    } else {
        let shown = lines[..max].join("\n");
        format!("{shown}\n… (+{} more lines)\n", lines.len() - max)
    }
}

fn block_esc(s: &str) -> String {
    let mut out = s.replace("{%", "{ %").replace("%}", "% }");
    if out.chars().count() > 4000 {
        out = out.chars().take(4000).collect::<String>();
        out.push_str("\n…\n");
    }
    out
}

fn line_esc(s: &str) -> String {
    let one: String = s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    let one = one.trim();
    if one.chars().count() > 120 {
        format!("{}…", one.chars().take(120).collect::<String>())
    } else {
        one.to_string()
    }
}

fn attr_esc(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| match c { '"' => '\'', '\n' | '\r' | '\t' => ' ', _ => c })
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.chars().count() > 80 {
        format!("{}…", cleaned.chars().take(80).collect::<String>())
    } else {
        cleaned.to_string()
    }
}

#[cfg(test)]
mod stamp_tests {
    use super::{stamp_ask_answered, stamp_unanswered_ask};

    const CARD: &str = "hi\n{% mafold/ask %}\nq|Deploy|0|Ship?\no|Yes|now\no|Hold|later\n{% /mafold/ask %}\n";

    #[test]
    fn rewrites_opener_with_answered_attr() {
        let mut full = CARD.to_string();
        assert!(stamp_ask_answered(&mut full, "Yes"));
        assert!(full.contains("{% mafold/ask answered=\"Yes\" %}\nq|Deploy|0|Ship?"));
        assert!(!full.contains("{% mafold/ask %}"));
    }

    #[test]
    fn stamps_last_ask_only_and_escapes() {
        let mut full = format!("{CARD}text\n{CARD}");
        assert!(stamp_ask_answered(&mut full, "Say \"go\"\nRegion: EU"));
        // first card untouched, second stamped; quotes → ', newlines → space
        assert!(full.starts_with(CARD));
        assert!(full.contains("{% mafold/ask answered=\"Say 'go' Region: EU\" %}"));
    }

    #[test]
    fn empty_answer_still_marks_answered() {
        let mut full = CARD.to_string();
        assert!(stamp_ask_answered(&mut full, "  \n "));
        assert!(full.contains("{% mafold/ask answered=\"✓\" %}"));
    }

    #[test]
    fn no_card_is_a_noop() {
        let mut full = "plain text".to_string();
        assert!(!stamp_ask_answered(&mut full, "Yes"));
        assert_eq!(full, "plain text");
    }

    #[test]
    fn finalized_stamps_once_only() {
        let first = stamp_unanswered_ask(CARD, "Hold").expect("unanswered card should stamp");
        assert!(first.contains("{% mafold/ask answered=\"Hold\" %}"));
        // a stamped opener no longer matches the bare needle → never re-stamped
        assert!(stamp_unanswered_ask(&first, "Yes").is_none());
        assert!(stamp_unanswered_ask("no card here", "Yes").is_none());
    }
}
