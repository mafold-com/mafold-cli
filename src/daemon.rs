//! Background daemon control — detach from the terminal so the agent keeps
//! running after the shell is closed (setsid + log file + pid file), plus
//! `stop` / `status`.

use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::platform;

fn dir() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("no HOME")?;
    let d = PathBuf::from(home).join(".mafold");
    fs::create_dir_all(&d)?;
    Ok(d)
}
fn pid_path() -> Result<PathBuf> {
    Ok(dir()?.join("agent.pid"))
}
fn log_path() -> Result<PathBuf> {
    Ok(dir()?.join("agent.log"))
}

fn read_pid() -> Option<u32> {
    fs::read_to_string(pid_path().ok()?)
        .ok()?
        .trim()
        .parse()
        .ok()
}
fn alive(pid: u32) -> bool {
    platform::pid_alive(pid)
}

/// Re-exec ourselves in a new session (detached from the controlling terminal),
/// with stdio redirected to a log file. The parent records the pid and exits.
pub fn start_detached(
    base: &str,
    token: &str,
    workdir: Option<&str>,
    harness: &str,
    no_auto_update: bool,
) -> Result<u32> {
    if let Some(pid) = read_pid() {
        if alive(pid) {
            anyhow::bail!("agent already running (pid {pid}) — run `mafold stop` first");
        }
    }
    let exe = std::env::current_exe()?;
    let out = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path()?)?;
    let err = out.try_clone()?;

    let mut cmd = Command::new(exe);
    cmd.arg("agent");
    // The child re-parses its own CLI, so a parent-only --no-auto-update used to
    // vanish here (the flag isn't in the env set below) — the detached agent then
    // self-updated anyway. Forward it on argv (it's a global flag, valid after
    // the subcommand), which also makes it visible in the process list.
    if no_auto_update {
        cmd.arg("--no-auto-update");
    }
    cmd.env("MAFOLD_BASE", base)
        .env("MAFOLD_BOT_TOKEN", token)
        .env("MAFOLD_HARNESS", harness)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    // Only pin the workdir when the user set one — otherwise the child resolves
    // it from the server owner-config (or the current dir) at runtime.
    if let Some(w) = workdir {
        cmd.env("MAFOLD_WORKDIR", w);
    }
    // New session / no console → survives the shell closing.
    platform::configure_detached(&mut cmd);
    let child = cmd.spawn().context("failed to spawn background agent")?;
    let pid = child.id();
    writeln!(fs::File::create(pid_path()?)?, "{pid}")?;
    Ok(pid)
}

pub fn stop() -> Result<()> {
    match read_pid() {
        Some(pid) if alive(pid) => {
            // Plain SIGTERM — the daemon's shutdown handler kills its in-flight
            // claude children precisely; background tasks survive.
            platform::terminate(pid);
            let _ = fs::remove_file(pid_path()?);
            println!("✓ stopped agent (pid {pid})");
        }
        _ => println!("no running agent"),
    }
    Ok(())
}

pub fn status() -> Result<()> {
    match read_pid() {
        Some(pid) if alive(pid) => {
            println!("agent running (pid {pid}) · logs: ~/.mafold/agent.log");
        }
        _ => println!("agent not running"),
    }
    Ok(())
}
