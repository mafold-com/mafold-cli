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
            Some(format!("\n{{% bash %}}\n{}{{% /bash %}}\n", block_esc(&cap_lines(out, 20))))
        }
        AgentEvent::Thinking(t) => {
            let t = t.trim();
            if t.is_empty() {
                return None;
            }
            Some(format!("\n{{% thinking %}}\n{}{{% /thinking %}}\n", block_esc(&cap_lines(t, 14))))
        }
        AgentEvent::Done { duration_ms, cost_usd, tokens } => result_tag(*duration_ms, *cost_usd, *tokens),
        AgentEvent::Session(_) => None,
    }
}

/// A compact `kind|label|detail` line for the `{% run %}` card body — the
/// collapsed per-turn trace. `None` for events that aren't process steps
/// (assistant text, session id, end-of-turn). AskUserQuestion blocks the turn,
/// so it's emitted out-of-band (a live `{% ask %}` card) and never buffered.
pub fn step_line(ev: &AgentEvent, names: &mut HashMap<String, String>) -> Option<String> {
    match ev {
        AgentEvent::ToolCall { id, name, input } => {
            names.insert(id.clone(), name.to_lowercase());
            Some(tool_step(name, input))
        }
        AgentEvent::ToolResult { id, text } => {
            if names.get(id).map(String::as_str) != Some("bash") {
                return None;
            }
            let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            if first.is_empty() {
                return None;
            }
            Some(format!("bash|Output|{}", cell_esc(first)))
        }
        AgentEvent::Thinking(t) => {
            let first = t.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
            if first.is_empty() {
                return None;
            }
            Some(format!("think|Thinking|{}", cell_esc(first)))
        }
        _ => None,
    }
}

/// A tool call → one collapsed `kind|label|detail` step line.
fn tool_step(name: &str, input: &Value) -> String {
    match name.to_lowercase().as_str() {
        "todowrite" => {
            let items = input["todos"].as_array();
            let total = items.map(|a| a.len()).unwrap_or(0);
            let done = items
                .map(|a| a.iter().filter(|t| t["status"].as_str() == Some("completed")).count())
                .unwrap_or(0);
            format!("todo|Plan|{done}/{total}")
        }
        "edit" | "multiedit" => {
            let file = input["file_path"].as_str().unwrap_or("");
            let (a, r) = if name.eq_ignore_ascii_case("multiedit") {
                let (mut a, mut r) = (0usize, 0usize);
                if let Some(edits) = input["edits"].as_array() {
                    for e in edits {
                        let (ea, er, _) = synth_hunk(e["old_string"].as_str().unwrap_or(""), e["new_string"].as_str().unwrap_or(""));
                        a += ea;
                        r += er;
                    }
                }
                (a, r)
            } else {
                let (a, r, _) = synth_hunk(input["old_string"].as_str().unwrap_or(""), input["new_string"].as_str().unwrap_or(""));
                (a, r)
            };
            format!("diff|{}|+{a} −{r}", cell_esc(file))
        }
        "write" => {
            let file = input["file_path"].as_str().unwrap_or("");
            let n = input["content"].as_str().map(|c| if c.is_empty() { 0 } else { c.lines().count() }).unwrap_or(0);
            format!("diff|{}|+{n}", cell_esc(file))
        }
        "task" => format!(
            "task|subagent · {}|{}",
            cell_esc(input["subagent_type"].as_str().unwrap_or("agent")),
            cell_esc(input["description"].as_str().unwrap_or("")),
        ),
        "webfetch" => format!("web|fetch|{}", cell_esc(input["url"].as_str().unwrap_or(""))),
        "websearch" => format!("web|search|{}", cell_esc(input["query"].as_str().unwrap_or(""))),
        "skill" => {
            let sname = input["command"].as_str().or_else(|| input["skill"].as_str()).or_else(|| input["name"].as_str()).unwrap_or("skill");
            let args = input["args"].as_str().or_else(|| input["arguments"].as_str()).unwrap_or("");
            format!("skill|/{}|{}", cell_esc(sname), cell_esc(args))
        }
        _ => format!("tool|{}|{}", cell_esc(name), cell_esc(&tool_detail(name, input))),
    }
}

/// Wrap a turn's buffered step lines into one collapsed `{% run %}` card.
pub fn run_card(body: &str, dur: Option<f64>, tokens: Option<u64>) -> String {
    let mut attrs = String::from(" status=\"done\"");
    if let Some(d) = dur {
        attrs.push_str(&format!(" took=\"{:.1}s\"", d / 1000.0));
    }
    if let Some(t) = tokens {
        attrs.push_str(&format!(" tokens=\"{}\"", fmt_count(t)));
    }
    format!("\n{{% run{attrs} %}}\n{}{{% /run %}}\n", block_esc(body))
}

/// A tool call → the right card.
fn tool_use_tag(name: &str, input: &Value) -> String {
    match name.to_lowercase().as_str() {
        "todowrite" => todo_tag(input),
        "edit" | "multiedit" => diff_tag_edit(&name.to_lowercase(), input),
        "write" => diff_tag_write(input),
        "task" => format!(
            "\n{{% task subagent=\"{}\" desc=\"{}\" /%}}\n",
            attr_esc(input["subagent_type"].as_str().unwrap_or("agent")),
            attr_esc(input["description"].as_str().unwrap_or("")),
        ),
        "webfetch" => format!("\n{{% web url=\"{}\" /%}}\n", attr_esc(input["url"].as_str().unwrap_or(""))),
        "websearch" => format!("\n{{% web query=\"{}\" /%}}\n", attr_esc(input["query"].as_str().unwrap_or(""))),
        "skill" => {
            let sname = input["command"].as_str()
                .or_else(|| input["skill"].as_str())
                .or_else(|| input["name"].as_str())
                .unwrap_or("skill");
            let args = input["args"].as_str().or_else(|| input["arguments"].as_str()).unwrap_or("");
            format!("\n{{% skill name=\"{}\" args=\"{}\" /%}}\n", attr_esc(sname), attr_esc(args))
        }
        "askuserquestion" => ask_tag(input),
        _ => format!("\n{{% tool name=\"{}\" detail=\"{}\" /%}}\n", attr_esc(name), attr_esc(&tool_detail(name, input))),
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
    format!("\n{{% ask %}}\n{}{{% /ask %}}\n", block_esc(&body))
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
    format!("\n{{% todo %}}\n{}{{% /todo %}}\n", block_esc(&body))
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
        "\n{{% diff file=\"{}\" added={} removed={} %}}\n{}{{% /diff %}}\n",
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
    Some(format!("\n{{% result{attrs} /%}}\n"))
}

/// A short, human-readable detail for a generic tool card (file, command, …).
fn tool_detail(name: &str, input: &Value) -> String {
    let raw = match name.to_lowercase().as_str() {
        "bash" => input["command"].as_str(),
        "edit" | "write" | "multiedit" | "read" | "notebookedit" => input["file_path"].as_str(),
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
mod run_tests {
    use super::*;
    use crate::harness::AgentEvent;
    use serde_json::json;

    #[test]
    fn buffers_steps_into_one_run_card() {
        let mut names = HashMap::new();
        // an Edit → a diff step with +/- counts
        let edit = AgentEvent::ToolCall {
            id: "1".into(), name: "Edit".into(),
            input: json!({"file_path": "src/agent.rs", "old_string": "a\nb", "new_string": "a\nc\nd"}),
        };
        let s1 = step_line(&edit, &mut names).unwrap();
        assert!(s1.starts_with("diff|src/agent.rs|+"), "got {s1}");

        // a Bash call + its result → a tool line then a bash Output line
        let bcall = AgentEvent::ToolCall { id: "2".into(), name: "Bash".into(), input: json!({"command": "cargo build"}) };
        assert_eq!(step_line(&bcall, &mut names).unwrap(), "tool|Bash|cargo build");
        let bres = AgentEvent::ToolResult { id: "2".into(), text: "  \nFinished in 4s\ntrailing".into() };
        assert_eq!(step_line(&bres, &mut names).unwrap(), "bash|Output|Finished in 4s");

        // text / session / done are NOT steps (text streams live; done → attrs)
        assert!(step_line(&AgentEvent::Text("hi".into()), &mut names).is_none());
        assert!(step_line(&AgentEvent::Done { duration_ms: Some(1.0), cost_usd: None, tokens: None }, &mut names).is_none());

        let card = run_card(&format!("{s1}\n{}\n", "bash|Output|Finished in 4s"), Some(12340.0), Some(4100));
        assert!(card.contains("{% run status=\"done\" took=\"12.3s\" tokens=\"4.1k\" %}"), "got {card}");
        assert!(card.contains("{% /run %}"));
    }
}
