//! Multi-daemon supervisor — run one daemon PER BOT (separate process, pid, and
//! log), driven by a local config (`~/.mafold/daemons.json`, the "local cache"
//! of which bots to run). Each daemon reads its harness from the server
//! (cloud-first via `getMe`); the local config supplies the token + workdir the
//! server can't know.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// One configured daemon = one bot presence.
#[derive(Serialize, Deserialize, Clone)]
pub struct DaemonCfg {
    /// The bot username — also the per-daemon pid/log key.
    pub name: String,
    /// The bot's `mb_` token.
    pub token: String,
    pub workdir: String,
    /// Local harness hint; the server (`getMe`) is authoritative at runtime.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
}

#[derive(Serialize, Deserialize, Default)]
struct Config {
    daemons: Vec<DaemonCfg>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}
fn cfg_path() -> PathBuf {
    home().join(".mafold/daemons.json")
}
fn daemons_dir() -> PathBuf {
    home().join(".mafold/daemons")
}
fn safe(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}
fn pid_path(name: &str) -> PathBuf {
    daemons_dir().join(safe(name)).join("pid")
}
fn log_path(name: &str) -> PathBuf {
    daemons_dir().join(safe(name)).join("log")
}

fn load() -> Config {
    fs::read_to_string(cfg_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
fn store(c: &Config) -> Result<()> {
    let p = cfg_path();
    if let Some(dir) = p.parent() { fs::create_dir_all(dir)?; }
    fs::write(p, serde_json::to_string_pretty(c)?)?;
    Ok(())
}

fn read_pid(name: &str) -> Option<u32> {
    fs::read_to_string(pid_path(name)).ok()?.trim().parse().ok()
}
fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

/// Reap any exited child daemons so they don't linger as zombies — a zombie pid
/// still answers `kill(pid, 0)`, which would make the liveness check (and thus
/// respawn) wrongly believe a crashed daemon is still running.
fn reap() {
    loop {
        let mut status = 0;
        let r = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
        if r <= 0 { break; } // 0 = children exist but none exited; -1 = no children
    }
}

/// `mafold add <name> --workdir … [--harness …]` (token from global --token).
pub fn add(name: String, token: String, workdir: String, harness: Option<String>) -> Result<()> {
    let workdir = fs::canonicalize(&workdir).map(|p| p.to_string_lossy().into_owned()).unwrap_or(workdir);
    let mut c = load();
    c.daemons.retain(|d| d.name != name);
    c.daemons.push(DaemonCfg { name: name.clone(), token, workdir, harness: harness.filter(|s| !s.is_empty()) });
    store(&c)?;
    println!("✓ added daemon `{name}` — start it with `mafold up {name}`");
    Ok(())
}

/// `mafold rm <name>` — drop from config (and stop it if running).
pub fn rm(name: &str) -> Result<()> {
    let mut c = load();
    let before = c.daemons.len();
    c.daemons.retain(|d| d.name != name);
    if c.daemons.len() == before {
        anyhow::bail!("no daemon named `{name}`");
    }
    store(&c)?;
    let _ = stop_one(name);
    println!("✓ removed daemon `{name}`");
    Ok(())
}

fn sup_pid_path() -> PathBuf {
    home().join(".mafold/supervisor.pid")
}
fn read_pid_file(p: &PathBuf) -> Option<u32> {
    fs::read_to_string(p).ok()?.trim().parse().ok()
}
fn sup_running() -> Option<u32> {
    read_pid_file(&sup_pid_path()).filter(|p| alive(*p))
}

/// `mafold up` — start the long-lived supervisor (one per machine) that keeps
/// all configured daemons running and OWNS updates (checks hourly, replaces the
/// shared binary safely, then restarts every daemon on the new version).
pub fn up(base: &str) -> Result<()> {
    let c = load();
    if c.daemons.is_empty() {
        println!("No daemons configured. Add one:\n  mafold --token mb_… add <bot> --workdir /path/to/repo");
        return Ok(());
    }
    if let Some(pid) = sup_running() {
        println!("supervisor already running (pid {pid}) — managing {} daemon(s) (it picks up add/rm within ~10s).", c.daemons.len());
        return Ok(());
    }
    let pid = start_supervisor(base)?;
    println!("✓ supervisor running (pid {pid}) — managing {} daemon(s)", c.daemons.len());
    println!("  logs:   ~/.mafold/daemons/<name>/log  (supervisor: ~/.mafold/supervisor.log)");
    println!("  status: mafold status   ·   stop: mafold down");
    Ok(())
}

/// Spawn the detached `mafold supervise` process.
fn start_supervisor(base: &str) -> Result<u32> {
    fs::create_dir_all(daemons_dir())?;
    let exe = std::env::current_exe()?;
    let out = fs::OpenOptions::new().create(true).append(true).open(home().join(".mafold/supervisor.log"))?;
    let err = out.try_clone()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--base").arg(base).arg("supervise")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    unsafe {
        cmd.pre_exec(|| { libc::setsid(); Ok(()) });
    }
    let child = cmd.spawn().context("failed to spawn supervisor")?;
    let pid = child.id();
    writeln!(fs::File::create(sup_pid_path())?, "{pid}")?;
    Ok(pid)
}

/// The long-lived supervisor loop: keep every configured daemon running, and own
/// updates — check hourly, replace the shared binary safely (locked + verified),
/// then stop all daemons and re-exec self so the new supervisor respawns them on
/// the new version.
pub async fn supervise(base: String) {
    let _ = fs::create_dir_all(daemons_dir());
    if let Ok(mut f) = fs::File::create(sup_pid_path()) {
        let _ = writeln!(f, "{}", std::process::id());
    }
    println!("supervisor up (pid {}) · base {base}", std::process::id());
    let http = reqwest::Client::new();
    let mut ticks: u64 = 0;
    loop {
        reap(); // clear zombies so the liveness check below is accurate
        let cfg = load();
        // Keep every configured daemon running (start any that died / aren't up).
        for d in &cfg.daemons {
            if !read_pid(&d.name).map(alive).unwrap_or(false) {
                match start_one(&base, d) {
                    Ok(Some(pid)) => println!("↑ started {} (pid {pid})", d.name),
                    Ok(None) => {}
                    Err(e) => eprintln!("✗ {}: {e}", d.name),
                }
            }
        }
        // Hourly update check (360 × 10s).
        ticks += 1;
        if ticks % 360 == 0 {
            match crate::update::check(&http).await {
                Ok(Some(r)) => {
                    println!("↻ update v{} available — applying + restarting daemons…", r.version);
                    if crate::update::apply(&http, &r.url, &r.version, r.sha256.as_deref()).await.is_ok() {
                        for d in &cfg.daemons { let _ = stop_one(&d.name); }
                        let _ = crate::update::reexec(); // new supervisor respawns daemons (new binary)
                    } else {
                        eprintln!("update failed — keeping the current version");
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!("update check failed: {e}"),
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

fn start_one(base: &str, d: &DaemonCfg) -> Result<Option<u32>> {
    if let Some(pid) = read_pid(&d.name) {
        if alive(pid) { return Ok(None); }
    }
    let dir = daemons_dir().join(safe(&d.name));
    fs::create_dir_all(&dir)?;
    let exe = std::env::current_exe()?;
    let out = fs::OpenOptions::new().create(true).append(true).open(log_path(&d.name))?;
    let err = out.try_clone()?;

    let mut cmd = Command::new(exe);
    cmd.arg("agent")
        // The supervisor owns updates — daemons never self-update.
        .arg("--no-auto-update")
        .env("MAFOLD_BASE", base)
        .env("MAFOLD_BOT_TOKEN", &d.token)
        .env("MAFOLD_WORKDIR", &d.workdir)
        .env("MAFOLD_HARNESS", d.harness.as_deref().unwrap_or("claude-code"))
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    // New session → detached from the controlling terminal.
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let child = cmd.spawn().with_context(|| format!("failed to spawn daemon `{}`", d.name))?;
    let pid = child.id();
    writeln!(fs::File::create(pid_path(&d.name))?, "{pid}")?;
    Ok(Some(pid))
}

/// `mafold down [name]` — stop the supervisor + all daemons (or one daemon).
pub fn down(only: Option<&str>) -> Result<()> {
    // Stop the supervisor FIRST (when stopping everything) so it doesn't just
    // respawn the daemons we're about to kill.
    if only.is_none() {
        if let Some(pid) = read_pid_file(&sup_pid_path()) {
            unsafe { libc::kill(pid as i32, libc::SIGTERM); }
            let _ = fs::remove_file(sup_pid_path());
            println!("✓ stopped supervisor");
        }
    }
    let c = load();
    let mut stopped = 0;
    for d in &c.daemons {
        if only.is_some_and(|n| n != d.name) { continue; }
        if stop_one(&d.name).is_ok() {
            println!("✓ stopped {}", d.name);
            stopped += 1;
        }
    }
    println!("{stopped} daemon(s) stopped");
    Ok(())
}

fn stop_one(name: &str) -> Result<()> {
    match read_pid(name) {
        Some(pid) => {
            unsafe { libc::kill(pid as i32, libc::SIGTERM); }
            let _ = fs::remove_file(pid_path(name));
            Ok(())
        }
        None => anyhow::bail!("not running"),
    }
}

/// `mafold status` — list configured daemons and whether each is running.
pub fn status() {
    match sup_running() {
        Some(p) => println!("supervisor: running (pid {p}) — owns updates"),
        None => println!("supervisor: not running (mafold up)"),
    }
    let c = load();
    if c.daemons.is_empty() {
        println!("No bot daemons configured (mafold add …).");
        return;
    }
    println!("bot daemons ({}):", c.daemons.len());
    for d in &c.daemons {
        let state = match read_pid(&d.name) {
            Some(p) if alive(p) => format!("running (pid {p})"),
            _ => "stopped".into(),
        };
        println!(
            "  • {:<26} {:<20} harness={}  ·  {}",
            d.name,
            state,
            d.harness.as_deref().unwrap_or("claude-code"),
            d.workdir,
        );
    }
}

/// `mafold logs <name>` — last ~40 lines of a daemon's log.
pub fn logs(name: &str) -> Result<()> {
    let p = log_path(name);
    let text = fs::read_to_string(&p).with_context(|| format!("no log for `{name}` (looked at {})", p.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(40);
    for l in &lines[start..] {
        println!("{l}");
    }
    Ok(())
}
