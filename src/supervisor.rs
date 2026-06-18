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
/// A daemon should simply exist — so adding one brings the boot-persistent
/// supervisor up: it runs now AND after every reboot, no extra step.
pub fn add(name: String, token: String, workdir: String, harness: Option<String>, base: &str) -> Result<()> {
    let workdir = fs::canonicalize(&workdir).map(|p| p.to_string_lossy().into_owned()).unwrap_or(workdir);
    let mut c = load();
    c.daemons.retain(|d| d.name != name);
    c.daemons.push(DaemonCfg { name: name.clone(), token, workdir, harness: harness.filter(|s| !s.is_empty()) });
    store(&c)?;
    println!("✓ added daemon `{name}`");
    up(base)
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

// ── boot-persistence (a daemon must survive reboot — autostart is part of the
// semantics, not an opt-in) ── the supervisor runs as a per-user service:
// launchd LaunchAgent on macOS, systemd --user unit on Linux. RunAtLoad/WantedBy
// → starts at login; KeepAlive/Restart=always → relaunches on crash. (Self-update
// re-exec keeps the same PID, so the service manager sees no exit → no fight.)
#[cfg(target_os = "macos")]
const SERVICE_LABEL: &str = "com.mafold.supervisor";

fn kill_supervisor_process() {
    if let Some(pid) = read_pid_file(&sup_pid_path()) {
        unsafe { libc::kill(pid as i32, libc::SIGTERM); }
        let _ = fs::remove_file(sup_pid_path());
    }
}

/// The PATH the supervised service must run with: the installing shell's PATH
/// (which found `claude` etc.) plus the usual user bins. A service manager
/// (launchd/systemd) otherwise hands out a minimal PATH (`/usr/bin:/bin:…`) that
/// lacks them — which is exactly what breaks `claude` under launchd.
fn service_path() -> String {
    let base = std::env::var("PATH").unwrap_or_default();
    let extra = format!("{}/.local/bin:/opt/homebrew/bin", home().display());
    if base.is_empty() { extra } else { format!("{extra}:{base}") }
}

#[cfg(target_os = "macos")]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn launchagent_path() -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{SERVICE_LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn ensure_autostart(base: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log = home().join(".mafold/supervisor.log");
    fs::create_dir_all(home().join(".mafold"))?;
    fs::create_dir_all(home().join("Library/LaunchAgents"))?;
    let plist = launchagent_path();
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\"><dict>\n\
  <key>Label</key><string>{label}</string>\n\
  <key>ProgramArguments</key>\n\
  <array>\n    <string>{exe}</string>\n    <string>--base</string><string>{base}</string>\n    <string>supervise</string>\n  </array>\n\
  <key>EnvironmentVariables</key>\n  <dict>\n    <key>PATH</key><string>{path}</string>\n  </dict>\n\
  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><true/>\n  <key>ProcessType</key><string>Background</string>\n\
  <key>StandardOutPath</key><string>{log}</string>\n  <key>StandardErrorPath</key><string>{log}</string>\n\
</dict></plist>\n",
        label = SERVICE_LABEL, exe = exe.display(), base = base, log = log.display(), path = xml_escape(&service_path()),
    );
    fs::write(&plist, xml)?;
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    // Re-load cleanly: bootout if already present (ignore errors), then bootstrap.
    let _ = Command::new("launchctl").arg("bootout").arg(format!("{domain}/{SERVICE_LABEL}")).output();
    let out = Command::new("launchctl").arg("bootstrap").arg(&domain).arg(&plist).output()
        .context("launchctl not available")?;
    if !out.status.success() {
        anyhow::bail!("launchctl bootstrap failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn remove_autostart() {
    let domain = format!("gui/{}", unsafe { libc::getuid() });
    let _ = Command::new("launchctl").arg("bootout").arg(format!("{domain}/{SERVICE_LABEL}")).output();
    let _ = fs::remove_file(launchagent_path());
}

#[cfg(target_os = "macos")]
fn autostart_loaded() -> bool {
    launchagent_path().exists()
        && Command::new("launchctl")
            .arg("print").arg(format!("gui/{}/{SERVICE_LABEL}", unsafe { libc::getuid() }))
            .output().map(|o| o.status.success()).unwrap_or(false)
}

#[cfg(target_os = "linux")]
fn systemd_unit_path() -> PathBuf {
    home().join(".config/systemd/user/mafold-supervisor.service")
}

#[cfg(target_os = "linux")]
fn ensure_autostart(base: &str) -> Result<()> {
    let exe = std::env::current_exe()?;
    fs::create_dir_all(home().join(".config/systemd/user"))?;
    let unit = format!(
        "[Unit]\nDescription=Mafold bot supervisor\nAfter=network-online.target\n\n\
[Service]\nEnvironment=PATH={path}\nExecStart={exe} --base {base} supervise\nRestart=always\nRestartSec=3\n\n\
[Install]\nWantedBy=default.target\n",
        exe = exe.display(), base = base, path = service_path(),
    );
    fs::write(systemd_unit_path(), unit)?;
    let _ = Command::new("systemctl").args(["--user", "daemon-reload"]).output();
    let out = Command::new("systemctl").args(["--user", "enable", "--now", "mafold-supervisor"]).output()
        .context("systemctl not available")?;
    if !out.status.success() {
        anyhow::bail!("systemctl enable failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn remove_autostart() {
    let _ = Command::new("systemctl").args(["--user", "disable", "--now", "mafold-supervisor"]).output();
    let _ = fs::remove_file(systemd_unit_path());
}

#[cfg(target_os = "linux")]
fn autostart_loaded() -> bool {
    Command::new("systemctl").args(["--user", "is-enabled", "mafold-supervisor"])
        .output().map(|o| o.status.success()).unwrap_or(false)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn ensure_autostart(_base: &str) -> Result<()> { anyhow::bail!("boot-persistence not supported on this OS") }
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn remove_autostart() {}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn autostart_loaded() -> bool { false }

/// `mafold up` — bring the supervisor up as a boot-persistent service (one per
/// machine): it keeps every configured daemon running and OWNS updates. Autostart
/// is the default — once up, it starts on login and relaunches on crash, until
/// `mafold down`. Idempotent: re-running just confirms (the running supervisor
/// picks up add/rm within ~10s).
pub fn up(base: &str) -> Result<()> {
    let c = load();
    if c.daemons.is_empty() {
        println!("No daemons configured. Add one:\n  mafold --token mb_… add <bot> --workdir /path/to/repo");
        return Ok(());
    }
    if autostart_loaded() {
        println!("✓ supervisor enabled (boot-persistent) — managing {} daemon(s); picks up changes within ~10s", c.daemons.len());
        return Ok(());
    }
    kill_supervisor_process(); // clear any stale detached supervisor first
    match ensure_autostart(base) {
        Ok(()) => {
            println!("✓ supervisor enabled — managing {} daemon(s); starts on login + relaunches on crash", c.daemons.len());
            println!("  logs:   ~/.mafold/daemons/<name>/log  (supervisor: ~/.mafold/supervisor.log)");
            println!("  status: mafold status   ·   stop: mafold down");
        }
        Err(e) => {
            // No service manager → at least run it now (just not across reboots).
            eprintln!("note: couldn't enable autostart ({e}); running detached (won't survive reboot)");
            if sup_running().is_none() {
                let pid = start_supervisor(base)?;
                println!("✓ supervisor running (pid {pid}) — managing {} daemon(s)", c.daemons.len());
            } else {
                println!("supervisor already running (detached)");
            }
        }
    }
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
        // Update check every 10 min (60 × 10s) — matches the standalone agent.
        ticks += 1;
        if ticks % 60 == 0 {
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

/// `mafold down [name]` — turn the supervisor off (remove autostart + stop all
/// daemons), or stop just one daemon. `down` is the explicit "off": after it, the
/// supervisor won't come back on reboot until `mafold up` / `add` again.
pub fn down(only: Option<&str>) -> Result<()> {
    // Stop the supervisor FIRST (when stopping everything) so it doesn't just
    // respawn the daemons we're about to kill — remove the service so KeepAlive /
    // Restart doesn't relaunch it, then kill the process.
    if only.is_none() {
        remove_autostart();
        kill_supervisor_process();
        println!("✓ stopped supervisor (autostart removed)");
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
    let auto = if autostart_loaded() { "enabled (boot-persistent)" } else { "off" };
    match sup_running() {
        Some(p) => println!("supervisor: running (pid {p}) · autostart {auto} · owns updates"),
        None => println!("supervisor: not running · autostart {auto} (mafold up)"),
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
