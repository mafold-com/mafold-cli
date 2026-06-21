//! `mafold ask-hook` — Claude Code PreToolUse hook for AskUserQuestion.
//!
//! The native AskUserQuestion auto-declines in headless `claude -p` (no client to
//! collect the answer). We register this command as a PreToolUse hook matching
//! AskUserQuestion (see `harness::claude_code`); claude runs it BEFORE the tool.
//! It blocks until the user answers the `{% ask %}` chat card — the daemon writes
//! their pick to `$MAFOLD_ASK_FILE` — then returns a `deny` whose reason carries
//! the answer. claude feeds that reason back to the model as the tool's result,
//! so the model continues the SAME turn with the user's choice. (Validated: a
//! deny-reason `"The user answered: X"` is consumed by the model as the answer.)

use anyhow::Result;
use std::io::Read;
use std::time::{Duration, Instant};

pub fn run() -> Result<()> {
    // Drain the PreToolUse JSON so claude's pipe doesn't block. We don't need its
    // contents — the question card is rendered from the harness's tool_use stream.
    let mut _input = String::new();
    let _ = std::io::stdin().read_to_string(&mut _input);

    let reason = match std::env::var("MAFOLD_ASK_FILE").ok() {
        Some(path) => match wait_for_answer(&path) {
            Some(ans) if !ans.trim().is_empty() => format!("The user answered: {}", ans.trim()),
            _ => "No answer was received from the user (timed out). Do not call \
                  AskUserQuestion again — ask the user in plain text instead."
                .to_string(),
        },
        None => "Interactive questions aren't available here — ask the user in \
                 plain text instead."
            .to_string(),
    };

    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    println!("{out}");
    Ok(())
}

/// Poll `path` until the daemon writes the user's answer (≤ 10 min), then consume
/// it (remove the file so a second ask in the same turn starts clean). The file
/// is unique per turn, so there's never a stale answer to read.
fn wait_for_answer(path: &str) -> Option<String> {
    let deadline = Instant::now() + Duration::from_secs(600);
    while Instant::now() < deadline {
        if let Ok(s) = std::fs::read_to_string(path) {
            if !s.trim().is_empty() {
                let _ = std::fs::remove_file(path);
                return Some(s);
            }
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    None
}
