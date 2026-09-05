//! `mafold steer-hook` — Claude Code PostToolUse hook that delivers what the
//! user said WHILE the turn was running.
//!
//! The daemon's turns are long. A user who spots the agent going the wrong way
//! has, until now, had two options: `/stop` (kill it, lose the work) or send
//! another message (which starts a SECOND turn racing the first in the same
//! workdir). Neither is what anyone means by "no, the other file".
//!
//! So a mid-turn message becomes a CORRECTION to the turn in flight. The daemon
//! appends it to `$MAFOLD_STEER_FILE`; this hook runs after every tool call and,
//! when there is something waiting, hands it to the model as `additionalContext`
//! — which claude feeds in as part of that tool's result. The effect:
//!
//!   * the reasoning and partial text already on screen stay exactly as they are
//!     (nothing is killed, nothing is re-said),
//!   * tool calls that already finished stay in the turn with their results,
//!   * the tool that was RUNNING when they spoke finishes normally — this is a
//!     PostToolUse hook, so it cannot interrupt one,
//!   * and the correction takes effect at the next tool-result boundary.
//!
//! **Consuming is a race and is settled by rename.** The daemon also drains this
//! file when the turn ends (a correction that arrives after the model's last
//! tool call would otherwise be silently lost, and it must become the next turn
//! instead). `fs::rename` to a unique name is atomic on every platform we ship,
//! so exactly one of the two readers gets any given message.
//!
//! Empty is the overwhelmingly common case, and it costs one failed rename.

use anyhow::Result;
use std::io::Read;

pub fn run() -> Result<()> {
    // Drain the PostToolUse JSON so claude's pipe never blocks. Nothing in it is
    // needed: what to say is in the steer file, and WHEN to say it is "now".
    let mut _input = String::new();
    let _ = std::io::stdin().read_to_string(&mut _input);

    let pending = std::env::var("MAFOLD_STEER_FILE").ok().and_then(|p| take(&p));
    let Some(text) = pending else {
        // No output at all: claude treats an empty hook result as "nothing to
        // add", which is precisely true and adds no tokens to the turn.
        return Ok(());
    };

    let out = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PostToolUse",
            "additionalContext": format!(
                "The user sent this WHILE you were working, so it has not been \
                 answered yet and it is newer than anything above:\n\n{}\n\n\
                 Take it into account from here on. If it changes what you should \
                 be doing, change course now rather than finishing the old plan \
                 first; if it is just information, carry on.",
                text.trim()
            ),
        }
    });
    println!("{out}");
    Ok(())
}

/// Atomically claim whatever is waiting in `path`, or None.
///
/// Rename-then-read, never read-then-delete: the daemon's end-of-turn drain runs
/// concurrently with this, and a read-then-delete would let both deliver the same
/// message — the model steered AND a duplicate follow-up turn asking the same
/// thing. The temp name carries this process's pid so two hooks (parallel tool
/// calls) can't collide either.
pub fn take(path: &str) -> Option<String> {
    let claim = format!("{path}.taken.{}", std::process::id());
    if std::fs::rename(path, &claim).is_err() {
        return None; // nothing waiting, or another reader got there first
    }
    let text = std::fs::read_to_string(&claim).ok();
    let _ = std::fs::remove_file(&claim);
    text.filter(|s| !s.trim().is_empty())
}
