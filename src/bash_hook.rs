//! `mafold bash-hook` — Claude Code PreToolUse hook for Bash background tasks.
//!
//! `claude -p` SIGTERMs its background shells' process groups the moment the
//! process exits (verified 2026-07-19, with and without the daemon in the
//! loop), so a `run_in_background` task can never outlive its turn on its own.
//! This hook intercepts Bash calls with `run_in_background: true` BEFORE they
//! run and moves the work out of claude's kill radius:
//!   1. the command is written to `~/.mafold/bgtasks/<conv>.<ts>.sh`,
//!   2. the hook ITSELF spawns it in a new session (fork + setsid — macOS has
//!      no `setsid` utility) with output to the sibling `.log`; the hook exits
//!      right after, so init adopts the task,
//!   3. the pid lands in the sibling `.pid` — the daemon's completion-wakeup
//!      monitor polls those and resumes the chat when all of them exit,
//!   4. the tool input is rewritten (`updatedInput`) to a foreground `echo`
//!      telling the model the task is detached and reported next turn.
//! Anything that isn't a background Bash — or any internal failure — produces
//! NO output, so claude proceeds with the original call untouched.

use anyhow::Result;
use serde_json::Value;
use std::io::Read;
#[cfg(unix)]
use std::path::Path;

pub fn run() -> Result<()> {
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    if let Some(out) = rewrite(&input) {
        println!("{out}");
    }
    Ok(())
}

fn rewrite(input: &str) -> Option<String> {
    let v: Value = serde_json::from_str(input).ok()?;
    if v["tool_name"].as_str()? != "Bash" {
        return None;
    }
    let ti = &v["tool_input"];
    if ti["run_in_background"].as_bool() != Some(true) {
        return None;
    }
    detach(&v, ti)
}

// No detach story on Windows yet — background tasks keep claude's own
// (turn-scoped) semantics there.
#[cfg(not(unix))]
fn detach(_v: &Value, _ti: &Value) -> Option<String> {
    None
}

#[cfg(unix)]
fn detach(v: &Value, ti: &Value) -> Option<String> {
    let command = ti["command"].as_str()?;
    let home = std::env::var("HOME").ok()?;
    let dir = Path::new(&home).join(".mafold").join("bgtasks");
    std::fs::create_dir_all(&dir).ok()?;
    sweep_old(&dir);

    // Same registry key the daemon scans for (agent::bgtasks_scan): the
    // conversation id claude was launched with, sanitized identically.
    let tag: String = std::env::var("MAFOLD_CONV")
        .unwrap_or_else(|_| "untagged".into())
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect();
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    // join, not with_extension — with_extension would eat `.{ts}` as the
    // extension and collide every task of the conversation onto one filename.
    let script = dir.join(format!("{tag}.{ts}.sh"));
    let log = dir.join(format!("{tag}.{ts}.log"));
    let pidf = dir.join(format!("{tag}.{ts}.pid"));
    std::fs::write(&script, format!("#!/bin/bash\n{command}\n")).ok()?;

    // The tool call's cwd (claude passes it in the hook input); fall back to
    // the hook's own cwd (claude spawns hooks in the session cwd).
    let cwd = v["cwd"]
        .as_str()
        .map(String::from)
        .or_else(|| std::env::current_dir().ok().map(|p| p.to_string_lossy().into_owned()))?;
    let pid = spawn_detached(&script, &log, &cwd)?;
    std::fs::write(&pidf, pid.to_string()).ok()?;

    let msg = format!(
        "[mafold] Background task detached (pid {pid}) — it runs in its own session and \
         SURVIVES this turn and daemon restarts. Its output streams to {} — do NOT wait \
         for it or poll it this turn: when every detached task finishes, the daemon opens \
         a NEW turn for you to read that log and report the results.",
        log.display()
    );
    let mut updated = ti.clone();
    updated["command"] = Value::String(format!("echo {}", shell_single_quote(&msg)));
    updated["run_in_background"] = Value::Bool(false);
    Some(
        serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "allow",
                "permissionDecisionReason": "background task detached by mafold so it survives the turn",
                "updatedInput": updated,
            }
        })
        .to_string(),
    )
}

/// Spawn `bash <script>` in a NEW SESSION with stdout/stderr appended to `log`.
/// The child leaves claude's process group entirely, so claude's exit-time
/// killpg can't reach it; when this hook exits, init adopts it.
#[cfg(unix)]
fn spawn_detached(script: &Path, log: &Path, cwd: &str) -> Option<u32> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let out = std::fs::OpenOptions::new().create(true).append(true).open(log).ok()?;
    let err = out.try_clone().ok()?;
    let mut cmd = Command::new("bash");
    cmd.arg(script)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    // SAFETY: setsid() is async-signal-safe and the closure only calls it.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    cmd.spawn().ok().map(|c| c.id())
}

/// POSIX single-quote `s` for safe embedding in a shell command.
#[cfg(unix)]
fn shell_single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Best-effort GC: registry artifacts older than 7 days (logs/scripts of
/// long-reported tasks) — keeps ~/.mafold/bgtasks from growing forever.
#[cfg(unix)]
fn sweep_old(dir: &Path) {
    const WEEK: u64 = 7 * 24 * 3600;
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let stale = e
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| m.elapsed().ok())
            .is_some_and(|age| age.as_secs() > WEEK);
        if stale {
            let _ = std::fs::remove_file(e.path());
        }
    }
}
