//! Producer-agnostic renderer: maps normalized [`AgentEvent`]s to the chat's
//! markdoc text + agent cards (tool / diff / todo / bash / thinking / result).
//! Anything that emits `AgentEvent`s gets identical card rendering for free —
//! the self-hosted daemon's harnesses and the api's own brains alike.

use serde_json::Value;
use std::collections::HashMap;

use crate::event::AgentEvent;

/// Render one event to a markdoc string (text or a card), or `None` to skip.
/// `names` tracks `tool_use_id → tool name` so a bash `tool_result` can be
/// matched to its call.
///
/// Tool calls inside a run group don't come through here — they're held as
/// [`ToolStep`]s so their result lands on the same card (see [`render_group`]).
/// This path stays for the events that AREN'T paired: narration, thinking, the
/// blocking ask, the end-of-turn stamp, and an ORPHAN result whose call is no
/// longer in the open group — that last one keeps its old shape (a standalone
/// output card) rather than being dropped, because a result with nowhere to go
/// is still a result the user needs to see.
pub fn render(ev: &AgentEvent, names: &mut HashMap<String, String>) -> Option<String> {
    match ev {
        AgentEvent::Text(t) => Some(t.clone()),
        AgentEvent::ToolCall { id, name, input } => {
            names.insert(id.clone(), name.to_lowercase());
            Some(tool_use_tag(name, input, None))
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
        // Removal, not content — the transcript un-says it in place
        // (`Transcript::rewind_text`); there is nothing to render.
        AgentEvent::TextRewind(_) => None,
        // Not text: the render loop uploads it and attaches it to the message,
        // so it renders as message MEDIA — the same bubble path a person's
        // photo takes — instead of as a card in the transcript.
        AgentEvent::Image { .. } => None,
        // Heartbeat only — consumed by the render loop's generating card, never
        // rendered as content.
        AgentEvent::Pulse { .. } => None,
        // Rendered as a line, NOT as the `{% mafold/compact %}` card. That card
        // draws a before→after bar and needs BOTH counts; the harness's
        // compaction event carries only the "before" (there is no post-compaction
        // count in it). Passing `before` alone makes the card read `after` as 0
        // and claim it freed 100% of the context — a number that is simply not
        // true. The card stays for `/compact`, which does know both.
        AgentEvent::Compacted { pre_tokens } => Some(match pre_tokens {
            Some(n) => format!("\n_🗜️ Context auto-compacted ({} before)_\n", fmt_count(*n)),
            None => "\n_🗜️ Context auto-compacted_\n".to_string(),
        }),
        AgentEvent::RateLimited { kind, resets_at } => Some(format!(
            "\n_⏳ Usage limit reached ({kind}){}_\n",
            reset_hint(*resets_at, now_unix())
        )),
        // Not rendered as new content — the render loop stamps it into the
        // already-emitted ask card via `stamp_ask_answered`.
        AgentEvent::AskAnswered(_) => None,
    }
}

/// The live "still generating" indicator, appended to every running snapshot
/// and absent from the final one.
///
/// It is CONTENT, not a client-side animation synthesized from `finalized_at` —
/// clients are dumb renderers. Which means its honesty lives entirely in these
/// props, and every producer has to send the same ones:
///
///   * `started`  seeds the elapsed clock.
///   * `beat`     the producer's activity counter. It must bump on stream
///                ACTIVITY, not on content: a turn can spend half a minute
///                producing nothing visible (thinking, writing a tool's
///                arguments) and a frozen beat is what tells the card the
///                difference between that and a hang. Omit it entirely and the
///                card can observe nothing, so it assumes "live" forever — a
///                producer that died mid-reply then looks exactly like one
///                still working.
///   * `beatAt`   WHEN the beat last advanced. `beat` alone is a bare counter:
///                a card mounting on a draft whose producer died an hour ago
///                sees a number, cannot tell it is stale, and must watch for
///                minutes to find out — and every remount in a virtualized list
///                restarts that wait. Only the producer knows this timestamp.
///   * `tokens`   the real output-token count where the producer has one, an
///                estimate otherwise.
///   * `shells`   background shells started this turn; `0` renders nothing.
///
/// Old cards ignore unknown attrs and old producers emitted the bare tag, so
/// both directions degrade safely.
pub fn generating_tag(started_ms: u64, beat: u64, beat_at_ms: u64, tokens: u64, shells: u64) -> String {
    format!(
        "\n{{% mafold/generating started={started_ms} beat={beat} beatAt={beat_at_ms} tokens={tokens} shells={shells} /%}}\n"
    )
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

/// One atomic step in a run group: a tool CALL and, once it lands, its RESULT.
///
/// The pair is one card. Harnesses stream every call in an assistant message and
/// every result in the message after it, so rendering each event as it arrives
/// produces `bash · bash · output · output` — two calls, then two outputs, with
/// nothing tying either output to the command that produced it. A step instead
/// keeps its slot open: the call paints immediately (the transcript still shows
/// middle states), and the result fills in underneath it when it arrives.
///
/// Re-rendering an already-committed group is free — the daemon pushes the whole
/// markdoc snapshot on every flush (the Telegram-draft model), so a landed
/// result simply changes what the next snapshot says.
#[derive(Debug, Clone)]
pub struct ToolStep {
    pub name: String,
    pub input: Value,
    /// The result text — `None` while the tool is still running.
    pub out: Option<String>,
}

impl ToolStep {
    pub fn new(name: &str, input: &Value) -> Self {
        Self { name: name.to_string(), input: input.clone(), out: None }
    }
    /// The result landed. First one wins: a harness that re-sends a result must
    /// not append a second copy to a card that already shows it.
    pub fn land(&mut self, text: &str) {
        if self.out.is_none() {
            self.out = Some(text.to_string());
        }
    }
    pub fn tag(&self) -> String {
        tool_use_tag(&self.name, &self.input, self.out.as_deref())
    }
}

/// One entry in a run group: a finished card (thinking, a legacy orphan result)
/// or a tool step whose result may still be pending.
#[derive(Debug, Clone)]
pub enum GroupItem {
    Card(String),
    Step(ToolStep),
}

/// A run group's body — every item in arrival order, each step carrying its own
/// result. Called on every snapshot push, so it must stay a pure function of the
/// group: the same items always render the same markdoc.
pub fn render_group(items: &[GroupItem]) -> String {
    let mut out = String::new();
    for it in items {
        match it {
            GroupItem::Card(s) => out.push_str(s),
            GroupItem::Step(s) => out.push_str(&s.tag()),
        }
    }
    out
}

/// A tool call (plus its result, when it has landed) → the right card.
fn tool_use_tag(name: &str, input: &Value, out: Option<&str>) -> String {
    let lname = name.to_lowercase();
    let (summary, body) = result_parts(&lname, out);
    match lname.as_str() {
        "todowrite" => todo_tag(input),
        // The diff IS the result — an "updated the file" line under it says
        // nothing the +N −M in its header doesn't already say.
        "edit" | "multiedit" => diff_tag_edit(&lname, name, input),
        "write" => diff_tag_write(name, input),
        "task" => card(
            "task",
            &format!(
                "subagent=\"{}\" desc=\"{}\"{}",
                attr_esc(input["subagent_type"].as_str().unwrap_or("agent")),
                attr_esc(input["description"].as_str().unwrap_or("")),
                summary,
            ),
            &body,
        ),
        "webfetch" => card("web", &format!("url=\"{}\"{}", attr_esc(input["url"].as_str().unwrap_or("")), summary), ""),
        "websearch" => card("web", &format!("query=\"{}\"{}", attr_esc(input["query"].as_str().unwrap_or("")), summary), ""),
        "skill" => {
            let sname = input["command"].as_str()
                .or_else(|| input["skill"].as_str())
                .or_else(|| input["name"].as_str())
                .unwrap_or("skill");
            let args = input["args"].as_str().or_else(|| input["arguments"].as_str()).unwrap_or("");
            card("skill", &format!("name=\"{}\" args=\"{}\"{}", attr_esc(sname), attr_esc(args), summary), "")
        }
        "askuserquestion" => ask_tag(input),
        _ => card(
            "tool",
            &format!("name=\"{}\" detail=\"{}\"{}", attr_esc(name), attr_esc(&tool_detail(name, input)), summary),
            &body,
        ),
    }
}

/// `{% mafold/<tag> <attrs> /%}` — self-closing, or a container when there's a
/// body to carry. One shape for every card the renderer emits, so "does this one
/// have a body" is a question about the DATA, never about the format string.
fn card(tag: &str, attrs: &str, body: &str) -> String {
    if body.is_empty() {
        format!("\n{{% mafold/{tag} {attrs} /%}}\n")
    } else {
        format!("\n{{% mafold/{tag} {attrs} %}}\n{}{{% /mafold/{tag} %}}\n", block_esc(body))
    }
}

/// A landed result → (`out="…"` attribute, card body).
///
/// Which of the two a tool gets is about what its result IS. A shell's stdout is
/// content and belongs in the body, verbatim and capped. A Read's result is the
/// file the model already has — echoing it back is noise, so it collapses to
/// "126 lines". Nothing at all is also an answer: a command that printed nothing
/// says so, rather than looking like it never finished.
fn result_parts(lname: &str, out: Option<&str>) -> (String, String) {
    let Some(text) = out else {
        return (String::new(), String::new());
    };
    let t = text.trim();
    let attr = |s: String| (format!(" out=\"{}\"", attr_esc(&s)), String::new());
    match lname {
        "bash" => {
            if t.is_empty() {
                attr("no output".into())
            } else {
                (String::new(), cap_lines(t, 20))
            }
        }
        "read" | "notebookedit" => attr(count_phrase(t, "line", "lines")),
        "glob" | "grep" => {
            let first = t.lines().next().unwrap_or("").trim();
            // Claude Code's own phrasing ("Found 12 files") when the harness
            // already summarized; the count when it handed back raw matches.
            if first.starts_with("Found ") && first.chars().count() <= 60 {
                attr(first.to_string())
            } else {
                attr(count_phrase(t, "result", "results"))
            }
        }
        // The diff card already carries the outcome.
        "edit" | "write" | "multiedit" | "apply_patch" | "todowrite" => (String::new(), String::new()),
        "task" => (String::new(), cap_lines(t, 6)),
        _ if t.is_empty() => (String::new(), String::new()),
        // An unknown tool: short results read as output, long ones as a count —
        // one rule, no per-tool table to keep in sync with anybody's tool set.
        _ if t.lines().count() > 6 => attr(count_phrase(t, "line", "lines")),
        _ => (String::new(), cap_lines(t, 6)),
    }
}

/// "1 line" / "126 lines" — an empty result counts as zero, not one.
fn count_phrase(t: &str, sing: &str, plur: &str) -> String {
    let n = if t.is_empty() { 0 } else { t.lines().count() };
    if n == 1 { format!("1 {sing}") } else { format!("{n} {plur}") }
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

fn diff_tag_edit(lname: &str, name: &str, input: &Value) -> String {
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
    diff_tag(file, name, added, removed, &body)
}

fn diff_tag_write(name: &str, input: &Value) -> String {
    let file = input["file_path"].as_str().unwrap_or("");
    let content = input["content"].as_str().unwrap_or("");
    let lines: Vec<&str> = if content.is_empty() { vec![] } else { content.lines().collect() };
    let mut body = String::new();
    for l in &lines { body.push('+'); body.push_str(l); body.push('\n'); }
    diff_tag(file, name, lines.len(), 0, &body)
}

/// `tool` names WHICH edit this was (Write / Edit / MultiEdit) — the card leads
/// with it, the way every other step in the transcript leads with its tool name.
fn diff_tag(file: &str, tool: &str, added: usize, removed: usize, body: &str) -> String {
    format!(
        "\n{{% mafold/diff file=\"{}\" tool=\"{}\" added={} removed={} %}}\n{}{{% /mafold/diff %}}\n",
        attr_esc(file), attr_esc(tool), added, removed, block_esc(&cap_lines(body, 24)),
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
///
/// The fallback matters as much as the table: an UNKNOWN tool — an MCP server's,
/// a server-side brain's own — used to render as a bare name with `detail=""`,
/// so a card said "deploy" and nothing about what was deployed. Rather than
/// grow a per-tool entry for every tool anyone might ever add, fall back to the
/// argument names tool schemas actually converge on. `detail` leads: a producer
/// that knows what its card should say can just say it.
fn tool_detail(name: &str, input: &Value) -> String {
    let raw = match name.to_lowercase().as_str() {
        "bash" => input["command"].as_str(),
        "edit" | "write" | "multiedit" | "read" | "notebookedit" | "apply_patch" => input["file_path"].as_str(),
        "glob" | "grep" => input["pattern"].as_str(),
        "webfetch" => input["url"].as_str(),
        "task" => input["description"].as_str(),
        "todowrite" => Some("updating plan"),
        _ => input["detail"]
            .as_str()
            .or_else(|| input["path"].as_str())
            .or_else(|| input["file_path"].as_str())
            .or_else(|| input["command"].as_str())
            .or_else(|| input["query"].as_str())
            .or_else(|| input["url"].as_str())
            .or_else(|| input["name"].as_str()),
    };
    raw.unwrap_or("").to_string()
}

/// Strip card markup, leaving the prose.
///
/// A reply produced by an agent turn is mostly CARDS — a run group wrapping a
/// diff of the file it wrote, the build output, the result stamp. That is the
/// right thing to show a person and the wrong thing to feed back to a model on
/// the next turn: it is a large fraction of the context window, and a model
/// reading its own `{% mafold/diff %}` output learns to WRITE card markup as
/// text, which renders as literal markup in the bubble.
///
/// Self-closing tags drop; a container drops with its body. Nesting is handled
/// for the shapes the renderer emits (a `run` group holding differently-named
/// cards); a card nested inside another of the SAME name would close early,
/// which nothing produces today.
pub fn strip_cards(md: &str) -> String {
    let mut out = String::with_capacity(md.len());
    let mut rest = md;
    while let Some(i) = rest.find("{%") {
        out.push_str(&rest[..i]);
        let after = &rest[i..];
        let Some(close) = after.find("%}") else {
            // An unterminated tag is not a card — keep it verbatim.
            out.push_str(after);
            return out;
        };
        let tag_end = close + 2;
        let inner = after[2..close].trim();
        let self_closing = after[..tag_end].ends_with("/%}");
        let name = inner.trim_start_matches('/').split_whitespace().next().unwrap_or("");
        if self_closing || inner.starts_with('/') || name.is_empty() {
            rest = &after[tag_end..];
            continue;
        }
        // A container: drop through its matching close, or — when the message
        // was truncated mid-card — through the rest of the text.
        let closer = format!("{{% /{name} %}}");
        rest = match after[tag_end..].find(&closer) {
            Some(j) => &after[tag_end + j + closer.len()..],
            None => "",
        };
    }
    out.push_str(rest);
    out
}

fn fmt_count(n: u64) -> String {
    if n >= 1000 { format!("{:.1}k", n as f64 / 1000.0) } else { n.to_string() }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// " · resets in ~42m" for a usage limit, from its unix reset timestamp. Empty
/// when there is nothing useful to say — no timestamp, or a time already past
/// (a stale reset would otherwise render as "resets in ~0m", which reads as
/// "you're already back" when we don't actually know that).
fn reset_hint(resets_at: Option<i64>, now: i64) -> String {
    let Some(at) = resets_at else { return String::new() };
    let secs = at - now;
    if secs <= 0 {
        return String::new();
    }
    let mins = (secs + 59) / 60; // round UP: 30s left is "~1m", never "~0m"
    match (mins / 60, mins % 60) {
        (0, m) => format!(" · resets in ~{m}m"),
        (h, 0) => format!(" · resets in ~{h}h"),
        (h, m) => format!(" · resets in ~{h}h{m}m"),
    }
}

/// Close inline-code spans and fences the model left OPEN in its own prose.
///
/// Card tags live at the markdown level, so an unclosed `` ` `` or ``` ``` ```
/// in narration swallows everything after it into a code span — and every
/// `{% mafold/… %}` that follows renders as literal markup instead of a card.
/// On 2026-08-15 one odd backtick in a 66-character sentence dumped the 19.5k
/// of tool cards behind it into the bubble as raw text: 95% of the message.
/// A malformed character in one sentence must not be able to un-render
/// everything after it.
///
/// Only the model's OWN prose is healed — the segments at card-nesting depth 0.
/// Card bodies belong to the renderer (already neutralised by [`block_esc`]),
/// and a diff that legitimately contains one backtick is not a defect.
///
/// Parity is judged per PARAGRAPH, because a markdown code span cannot contain
/// a blank line: the repair is a single backtick appended to the paragraph that
/// left one open, which costs one stray character and saves every card below it.
pub fn heal_open_code(md: &str) -> String {
    // Fast path: nothing to balance, and the overwhelming majority of replies.
    if !md.contains('`') {
        return md.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    let mut depth: usize = 0;
    let mut fence: Option<String> = None;
    // Backticks in the paragraph being read, and where its last line landed.
    let mut ticks = 0usize;
    let mut last_line: Option<usize> = None;

    // Close the paragraph just ended: an odd count means a span is still open.
    macro_rules! close_para {
        () => {
            if ticks % 2 == 1 {
                if let Some(i) = last_line {
                    out[i].push('`');
                }
            }
            ticks = 0;
            last_line = None;
        };
    }

    for line in md.lines() {
        let t = line.trim_start();
        // The renderer always emits its tags at column 0 on a line of their own,
        // so a tag line is recognisable without parsing markdoc.
        let opens = t.starts_with("{% mafold/") && !t.trim_end().ends_with("/%}"); // LINT-IGNORE
        let closes = t.starts_with("{% /mafold/"); // LINT-IGNORE
        let self_closing = t.starts_with("{% mafold/") && t.trim_end().ends_with("/%}"); // LINT-IGNORE
        let tag_line = opens || closes || self_closing;

        if let Some(marker) = fence.clone() {
            // A fence the model opened and never closed would eat the cards
            // below it whole — close it at the boundary instead.
            if t.starts_with(&marker) {
                fence = None;
                out.push(line.to_string());
            } else if tag_line {
                out.push(marker);
                fence = None;
                out.push(line.to_string());
                if opens {
                    depth += 1;
                } else if closes {
                    depth = depth.saturating_sub(1);
                }
            } else {
                out.push(line.to_string());
            }
            continue;
        }

        if tag_line {
            close_para!();
            if opens {
                depth += 1;
            } else if closes {
                depth = depth.saturating_sub(1);
            }
            out.push(line.to_string());
            continue;
        }
        if depth > 0 {
            out.push(line.to_string()); // a card body — the renderer's, not ours
            continue;
        }
        if t.starts_with("```") || t.starts_with("~~~") {
            close_para!();
            fence = Some(t.chars().take_while(|c| *c == '`' || *c == '~').collect());
            out.push(line.to_string());
            continue;
        }
        if t.is_empty() {
            close_para!();
            out.push(line.to_string());
            continue;
        }
        ticks += line.matches('`').count();
        out.push(line.to_string());
        last_line = Some(out.len() - 1);
    }
    // The trailing paragraph, which no tag line or blank line ended. Spelled out
    // rather than `close_para!()` so the macro's resets don't read as dead
    // stores at the end of the function.
    if ticks % 2 == 1 {
        if let Some(i) = last_line {
            out[i].push('`');
        }
    }
    if let Some(marker) = fence {
        out.push(marker);
    }
    let mut healed = out.join("\n");
    if md.ends_with('\n') {
        healed.push('\n');
    }
    healed
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
mod pairing_tests {
    use super::{render_group, GroupItem, ToolStep};
    use serde_json::json;

    fn step(name: &str, input: serde_json::Value) -> ToolStep {
        ToolStep::new(name, &input)
    }

    /// A call with no result yet still paints — the transcript shows the middle
    /// states, it just doesn't claim an output that hasn't happened.
    #[test]
    fn a_pending_call_renders_alone() {
        let s = step("Bash", json!({ "command": "pnpm test" }));
        let tag = s.tag();
        assert!(tag.contains("{% mafold/tool name=\"Bash\" detail=\"pnpm test\" /%}"), "{tag}");
        assert!(!tag.contains("{% /mafold/tool %}"));
    }

    /// THE BUG: two shells in one message, then two outputs. Rendered per event
    /// that reads `bash · bash · output · output`; as slots, each output is on
    /// the card of the command that produced it.
    #[test]
    fn parallel_calls_keep_their_own_output() {
        let mut a = step("Bash", json!({ "command": "ls src" }));
        let mut b = step("Bash", json!({ "command": "ls docs" }));
        // Results arrive AFTER both calls — and out of order, for good measure.
        b.land("readme.md");
        a.land("main.rs");
        let md = render_group(&[GroupItem::Step(a), GroupItem::Step(b)]);
        let first = md.find("ls src").expect("first call");
        let second = md.find("ls docs").expect("second call");
        let out_a = md.find("main.rs").expect("first output");
        let out_b = md.find("readme.md").expect("second output");
        // Call order is arrival order; each output sits inside its OWN call's card.
        assert!(first < out_a && out_a < second && second < out_b, "{md}");
        assert_eq!(md.matches("{% /mafold/tool %}").count(), 2, "{md}");
    }

    /// A shell's stdout is content — verbatim in the body. A Read's result is
    /// the file the model already has — a count, not an echo.
    #[test]
    fn a_result_is_body_or_summary_by_what_it_is() {
        let mut sh = step("Bash", json!({ "command": "echo hi" }));
        sh.land("hi\nthere");
        let tag = sh.tag();
        assert!(tag.contains("%}\nhi\nthere\n{% /mafold/tool %}"), "{tag}");

        let mut rd = step("Read", json!({ "file_path": "src/main.rs" }));
        rd.land(&"x\n".repeat(126));
        let tag = rd.tag();
        assert!(tag.contains("out=\"126 lines\""), "{tag}");
        assert!(!tag.contains("{% /mafold/tool %}"), "a count needs no body: {tag}");
    }

    /// A command that printed nothing SAYS so — silence is otherwise
    /// indistinguishable from a command still running.
    #[test]
    fn an_empty_shell_result_still_reports() {
        let mut s = step("Bash", json!({ "command": "true" }));
        s.land("   \n");
        assert!(s.tag().contains("out=\"no output\""), "{}", s.tag());
    }

    /// A re-sent result must not stack a second copy onto a card already showing it.
    #[test]
    fn the_first_result_wins() {
        let mut s = step("Bash", json!({ "command": "date" }));
        s.land("first");
        s.land("second");
        let tag = s.tag();
        assert!(tag.contains("first") && !tag.contains("second"), "{tag}");
    }

    /// An edit renders as its diff, named by the tool that made it — and the
    /// harness's "file updated" acknowledgement adds nothing on top.
    #[test]
    fn an_edit_is_its_diff_not_an_acknowledgement() {
        let mut e = step("Write", json!({ "file_path": "a.txt", "content": "one\ntwo" }));
        e.land("The file a.txt has been updated.");
        let tag = e.tag();
        assert!(tag.contains("{% mafold/diff file=\"a.txt\" tool=\"Write\" added=2 removed=0 %}"), "{tag}");
        assert!(!tag.contains("has been updated"), "{tag}");
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

#[cfg(test)]
mod strip_tests {
    use super::strip_cards;

    /// A whole agent turn reduces to the sentences a person actually wrote —
    /// which is all the next turn's model should be re-reading.
    #[test]
    fn a_turn_reduces_to_its_prose() {
        let md = "Building a todo list.\n\
                  \n{% mafold/run summary=\"Edited 1 file\" %}\n\
                  \n{% mafold/diff file=\"todo.jsx\" tool=\"Write\" added=2 removed=0 %}\n\
                  +import React from 'react';\n+export default function App() {}\n\
                  {% /mafold/diff %}\n\
                  \n{% mafold/tool name=\"Bash\" detail=\"esbuild\" out=\"ok\" /%}\n\
                  {% /mafold/run %}\n\
                  \nDone — have a look.\n{% mafold/result duration=\"3.0s\" /%}\n"; // LINT-IGNORE
        let text = strip_cards(md);
        assert!(text.contains("Building a todo list."), "{text:?}");
        assert!(text.contains("Done — have a look."), "{text:?}");
        assert!(!text.contains("{%"), "card markup survived: {text:?}");
        assert!(!text.contains("import React"), "the diff body leaked: {text:?}");
    }

    #[test]
    fn plain_text_is_untouched() {
        let s = "just a sentence, 100% plain";
        assert_eq!(strip_cards(s), s);
    }

    /// A truncated card (the message was cut mid-stream) must not make the rest
    /// of the transcript disappear silently — and an unterminated `{%` is not a
    /// card at all, so it stays.
    #[test]
    fn broken_markup_degrades_predictably() {
        assert_eq!(strip_cards("before {% not-a-tag"), "before {% not-a-tag"); // LINT-IGNORE
        // An opener with no closer eats the remainder — the body IS card
        // content, and half a card is not prose.
        let out = strip_cards("head\n{% mafold/diff file=\"a\" %}\n+x\n"); // LINT-IGNORE
        assert_eq!(out, "head\n");
    }

    /// Nested cards: the outer container takes its whole body with it.
    #[test]
    fn a_group_takes_its_children() {
        let md = "{% mafold/run summary=\"x\" %}{% mafold/tool name=\"Read\" /%}{% /mafold/run %}tail"; // LINT-IGNORE
        assert_eq!(strip_cards(md), "tail");
    }
}

#[cfg(test)]
mod generating_tests {
    use super::generating_tag;

    /// The props are the card's only evidence. A tag missing `beat` makes the
    /// card assume "live" forever, so a dead producer is indistinguishable from
    /// a working one — the exact bug the api's bare `{% generating %}` had.
    #[test]
    fn the_tag_carries_every_liveness_prop() {
        let tag = generating_tag(1_785_900_000_000, 42, 1_785_900_009_000, 4200, 0);
        for prop in ["started=", "beat=", "beatAt=", "tokens=", "shells="] {
            assert!(tag.contains(prop), "{prop} missing from {tag}");
        }
        assert!(tag.contains("beat=42") && tag.contains("tokens=4200"), "{tag}");
    }

    /// It must be namespaced AND self-closing: `strip_trailing_generating`
    /// (which keeps a finalized bubble from ending on a spinner) matches
    /// `{% mafold/generating` and proves the card stands alone via its `/%}`.
    #[test]
    fn the_tag_is_namespaced_and_strippable() {
        let tag = generating_tag(1, 2, 3, 4, 5);
        assert!(tag.contains("{% mafold/generating "), "{tag}"); // LINT-IGNORE
        let trimmed = tag.trim_end();
        let inner = trimmed.strip_suffix("/%}").expect("self-closing");
        assert!(!inner.contains("%}"), "nothing may close before the card does: {tag}");
    }
}

#[cfg(test)]
mod notice_tests {
    use super::*;

    const NOW: i64 = 1_785_900_000;

    #[test]
    fn a_reset_within_the_hour_reads_in_minutes() {
        assert_eq!(reset_hint(Some(NOW + 30 * 60), NOW), " · resets in ~30m");
    }

    #[test]
    fn a_longer_reset_reads_in_hours() {
        assert_eq!(reset_hint(Some(NOW + 2 * 3600), NOW), " · resets in ~2h");
        assert_eq!(reset_hint(Some(NOW + 2 * 3600 + 15 * 60), NOW), " · resets in ~2h15m");
    }

    /// Rounding UP matters: 30 seconds left rendered as "~0m" reads as "you're
    /// already back", which is the one thing we know isn't true yet.
    #[test]
    fn a_sub_minute_reset_rounds_up_never_to_zero() {
        assert_eq!(reset_hint(Some(NOW + 30), NOW), " · resets in ~1m");
    }

    /// Nothing useful to say → say nothing, rather than print a stale or absent
    /// timestamp as if it were information.
    #[test]
    fn a_past_or_missing_reset_says_nothing() {
        assert_eq!(reset_hint(Some(NOW - 60), NOW), "");
        assert_eq!(reset_hint(None, NOW), "");
    }

    #[test]
    fn a_compaction_renders_with_the_size_it_compacted() {
        let mut names = HashMap::new();
        let out = render(&AgentEvent::Compacted { pre_tokens: Some(302_336) }, &mut names).unwrap();
        assert!(out.contains("302.3k"), "{out}");
        assert!(out.contains("compacted"), "{out}");
    }

    /// Without a size it still has to render — the point of the line is
    /// explaining a multi-minute silence, and that holds with or without a number.
    #[test]
    fn a_compaction_without_a_size_still_renders() {
        let mut names = HashMap::new();
        let out = render(&AgentEvent::Compacted { pre_tokens: None }, &mut names).unwrap();
        assert!(out.contains("compacted"), "{out}");
        assert!(!out.contains('0'), "must not invent a number: {out}");
    }

    /// An unknown tool must still say WHAT it acted on. A card reading
    /// `deploy` with an empty detail is the "black box" complaint in miniature.
    #[test]
    fn an_unknown_tool_still_gets_a_detail() {
        assert_eq!(tool_detail("deploy", &serde_json::json!({ "detail": "todo" })), "todo");
        assert_eq!(
            tool_detail("mcp__browser__navigate", &serde_json::json!({ "url": "https://x" })),
            "https://x"
        );
        assert_eq!(tool_detail("whatever", &serde_json::json!({})), "");
        // The explicit table still wins over the fallback.
        assert_eq!(
            tool_detail("Bash", &serde_json::json!({ "command": "ls", "detail": "ignored" })),
            "ls"
        );
    }

    #[test]
    fn a_usage_limit_renders_its_kind() {
        let mut names = HashMap::new();
        let out = render(
            &AgentEvent::RateLimited { kind: "five_hour".into(), resets_at: None },
            &mut names,
        )
        .unwrap();
        assert!(out.contains("five_hour"), "{out}");
        assert!(out.contains("Usage limit"), "{out}");
    }
}

#[cfg(test)]
mod heal_tests {
    use super::heal_open_code;

    /// The 2026-08-15 message: three spliced attempts left five backticks, and
    /// the `{% mafold/run %}` under them rendered as literal text.
    #[test]
    fn an_odd_backtick_is_closed_before_the_cards_below_it() {
        let md = "Now theNow the RN twin — first `TNow the RN twin — first `TextArea` gains the same `grow` contract:\n\
                  \n{% mafold/run summary=\"Ran 1 shell command\" %}\n\
                  {% mafold/tool name=\"Bash\" detail=\"pnpm test\" /%}\n\
                  {% /mafold/run %}\n";
        let out = heal_open_code(md);
        let head = out.split("{% mafold/run").next().unwrap();
        assert_eq!(head.matches('`').count() % 2, 0, "span still open: {out}");
        assert!(out.contains("{% mafold/run summary="), "the cards must survive: {out}");
    }

    /// Balanced prose is returned byte-for-byte — the repair must be invisible
    /// on every healthy reply, which is nearly all of them.
    #[test]
    fn balanced_prose_is_untouched() {
        for md in [
            "plain prose, no code at all\n",
            "call `foo()` then `bar()` and stop\n",
            "```\nfn main() {}\n```\n\ndone\n",
            "a `span` here\n\nand `another` there\n",
        ] {
            assert_eq!(heal_open_code(md), md, "rewrote a healthy message: {md}");
        }
    }

    /// A card BODY is the renderer's own escaped output. A diff line that
    /// happens to carry one backtick is not a defect and must not be edited.
    #[test]
    fn card_bodies_are_left_alone() {
        let md = "{% mafold/tool name=\"Bash\" detail=\"echo\" %}\n\
                  let s = `unterminated in a shell heredoc\n\
                  {% /mafold/tool %}\n";
        assert_eq!(heal_open_code(md), md);
    }

    /// An unclosed fence eats even more than a stray backtick — close it at the
    /// first card tag rather than letting the rest of the reply vanish into it.
    #[test]
    fn an_unclosed_fence_is_closed_before_a_card() {
        let md = "here:\n```rust\nfn main() {}\n\n{% mafold/run summary=\"Ran 1 shell command\" %}\n{% /mafold/run %}\n";
        let out = heal_open_code(md);
        assert_eq!(out.matches("```").count(), 2, "fence not closed: {out}");
        let fence_close = out.find("```").unwrap() + 3;
        assert!(
            out[fence_close..].find("```").unwrap() + fence_close < out.find("{% mafold/run").unwrap(),
            "the fence must close ABOVE the card: {out}"
        );
    }

    /// Parity is per paragraph: an odd count in one paragraph must not be
    /// "balanced" by an odd count in the next.
    #[test]
    fn parity_does_not_leak_across_paragraphs() {
        let out = heal_open_code("one `open\n\ntwo `also open\n");
        assert_eq!(out, "one `open`\n\ntwo `also open`\n", "{out}");
    }
}
