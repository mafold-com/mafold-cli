//! Multi-daemon supervisor — run one daemon PER BOT (separate process, pid, and
//! log), driven by a local config (`~/.mafold/daemons.json`, the "local cache"
//! of which bots to run). Each daemon reads its harness from the server
//! (cloud-first via `getMe`); the local config supplies the token + workdir the
//! server can't know.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::platform;

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

/// Per-daemon in-flight-turn marker: the daemon writes its active-turn count here
/// (via `TurnGuard`), and the supervisor reads it to DRAIN — wait a live turn out
/// before a cliUpdate restart instead of killing the reply mid-flight.
pub(crate) fn busy_path(name: &str) -> PathBuf {
    daemons_dir().join(safe(name)).join("busy")
}

fn busy_count(name: &str) -> usize {
    fs::read_to_string(busy_path(name)).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
}

/// Wait (up to `max`) for every ALIVE daemon to finish its in-flight turns before
/// a restart, so the daemon watching its own release doesn't kill a turn. A dead
/// daemon's stale marker is ignored; the cap stops a forever-busy daemon from
/// blocking updates indefinitely.
async fn drain_daemons(daemons: &[DaemonCfg], max: std::time::Duration) {
    let start = std::time::Instant::now();
    loop {
        let busy: Vec<String> = daemons
            .iter()
            .filter_map(|d| {
                let live = read_pid(&d.name).map(alive).unwrap_or(false);
                let n = busy_count(&d.name);
                (live && n > 0).then(|| format!("{}={n}", d.name))
            })
            .collect();
        if busy.is_empty() {
            return;
        }
        if start.elapsed() >= max {
            println!("↻ drain capped at {max:?} — restarting anyway (still busy: {})", busy.join(", "));
            return;
        }
        println!("↻ waiting for in-flight turns before restart: {}", busy.join(", "));
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
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
    platform::pid_alive(pid)
}

/// Reap any exited child daemons so they don't linger as zombies — a zombie pid
/// still answers `kill(pid, 0)`, which would make the liveness check (and thus
/// respawn) wrongly believe a crashed daemon is still running. (No-op where the
/// OS has no zombies, e.g. Windows.)
fn reap() {
    platform::reap_children();
}

/// `mafold add <name> --workdir … [--harness …]` (token from global --token).
/// A daemon should simply exist — so adding one brings the boot-persistent
/// supervisor up: it runs now AND after every reboot, no extra step.
pub fn add(name: String, token: String, workdir: String, harness: Option<String>, base: &str, no_auto_update: bool) -> Result<()> {
    let workdir = fs::canonicalize(&workdir).map(|p| p.to_string_lossy().into_owned()).unwrap_or(workdir);
    let mut c = load();
    c.daemons.retain(|d| d.name != name);
    c.daemons.push(DaemonCfg { name: name.clone(), token, workdir, harness: harness.filter(|s| !s.is_empty()) });
    store(&c)?;
    println!("✓ added daemon `{name}`");
    up(base, no_auto_update)
}

/// Tombstone a daemon writes (via `deprovision_and_exit`) when its bot was
/// deleted server-side: "remove me from the config, don't respawn me".
fn deprovision_path(name: &str) -> PathBuf {
    daemons_dir().join(safe(name)).join("deprovision")
}

/// Token of a configured daemon, if any. A child uses this to verify that an
/// (inherited) MAFOLD_DAEMON_NAME really refers to ITSELF before tombstoning —
/// a subprocess of a daemon inherits the parent's env, and a tombstone under
/// the parent's name would deprovision the wrong bot.
pub fn daemon_token(name: &str) -> Option<String> {
    load().daemons.iter().find(|d| d.name == name).map(|d| d.token.clone())
}

/// Called by a daemon child right before it exits because its bot no longer
/// exists. The supervisor owns daemons.json, so the child only leaves this
/// marker; the next tick removes the daemon from the config.
pub fn request_deprovision(name: &str, reason: &str) {
    let p = deprovision_path(name);
    if let Some(dir) = p.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(p, reason);
}

/// Drop every tombstoned daemon from the config (and make sure it's dead).
/// Runs at the top of each supervise tick, BEFORE the respawn pass — so a
/// deprovisioned daemon can't be brought back by the liveness check.
fn sweep_deprovisioned() {
    let mut c = load();
    let mut dropped = Vec::new();
    c.daemons.retain(|d| {
        let p = deprovision_path(&d.name);
        if !p.exists() {
            return true;
        }
        let reason = fs::read_to_string(&p).unwrap_or_default();
        let _ = fs::remove_file(&p);
        dropped.push((d.name.clone(), reason));
        false
    });
    if dropped.is_empty() {
        return;
    }
    if let Err(e) = store(&c) {
        eprintln!("✗ deprovision sweep couldn't rewrite config: {e}");
        return;
    }
    for (name, reason) in dropped {
        let _ = stop_one(&name);
        println!("✂ deprovisioned `{name}` — {}", if reason.is_empty() { "bot deleted server-side" } else { &reason });
    }
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
        platform::terminate(pid);
        let _ = fs::remove_file(sup_pid_path());
    }
}

/// The PATH the supervised service must run with: the installing shell's PATH
/// (which found `claude` etc.) plus the usual user bins. A service manager
/// (launchd/systemd) otherwise hands out a minimal PATH (`/usr/bin:/bin:…`) that
/// lacks them — which is exactly what breaks `claude` under launchd.
#[cfg_attr(not(any(target_os = "macos", target_os = "linux")), allow(dead_code))]
fn service_path() -> String {
    let base = std::env::var("PATH").unwrap_or_default();
    let extra = format!("{}/.local/bin:/opt/homebrew/bin", home().display());
    if base.is_empty() { extra } else { format!("{extra}:{base}") }
}

#[cfg(any(target_os = "macos", windows))]
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(target_os = "macos")]
fn launchagent_path() -> PathBuf {
    home().join("Library/LaunchAgents").join(format!("{SERVICE_LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn ensure_autostart(base: &str, no_auto_update: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    let log = home().join(".mafold/supervisor.log");
    fs::create_dir_all(home().join(".mafold"))?;
    fs::create_dir_all(home().join("Library/LaunchAgents"))?;
    let plist = launchagent_path();
    // Carry `--no-auto-update` into the boot-persistent service so a locally-patched
    // install isn't clobbered by the supervisor after the next login (matches the
    // Windows task path + the detached-fallback in start_supervisor).
    let update_arg = if no_auto_update { "\n    <string>--no-auto-update</string>" } else { "" };
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<plist version=\"1.0\"><dict>\n\
  <key>Label</key><string>{label}</string>\n\
  <key>ProgramArguments</key>\n\
  <array>\n    <string>{exe}</string>\n    <string>--base</string><string>{base}</string>\n    <string>supervise</string>{update_arg}\n  </array>\n\
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
fn ensure_autostart(base: &str, no_auto_update: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    fs::create_dir_all(home().join(".config/systemd/user"))?;
    // Carry `--no-auto-update` into the boot-persistent unit (see the macOS/Windows
    // paths) so a locally-patched install survives a reboot un-clobbered.
    let update_arg = if no_auto_update { " --no-auto-update" } else { "" };
    let unit = format!(
        "[Unit]\nDescription=Mafold bot supervisor\nAfter=network-online.target\n\n\
[Service]\nEnvironment=PATH={path}\nExecStart={exe} --base {base} supervise{update_arg}\nRestart=always\nRestartSec=3\n\n\
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

// Windows: a per-user Task Scheduler logon task. Registered from an XML file
// (not `/TR`) because /TR mangles quoted paths AND can't express what we need:
// no execution time limit (the default kills the supervisor after 72h!),
// battery guards off, and a hidden task entry. There is no true launchd
// KeepAlive / systemd Restart=always on Windows — RestartOnFailure only covers
// the task FAILING TO START, not the process crashing later (verified on Win11)
// — so a watchdog TimeTrigger re-fires the task every 30 minutes instead:
// alive → IgnoreNew makes it a no-op; dead → relaunched within half an hour.
#[cfg(windows)]
const TASK_NAME: &str = "MafoldSupervisor";

#[cfg(windows)]
fn task_xml_path() -> PathBuf {
    home().join(".mafold/supervisor-task.xml")
}

/// Task Scheduler requires its XML in UTF-16; write it with a BOM so schtasks
/// accepts the file regardless of the system code page.
#[cfg(windows)]
fn write_utf16le(path: &PathBuf, text: &str) -> std::io::Result<()> {
    let mut bytes = vec![0xFF_u8, 0xFE_u8]; // UTF-16LE BOM
    for u in text.encode_utf16() {
        bytes.extend_from_slice(&u.to_le_bytes());
    }
    fs::write(path, bytes)
}

/// Decode a console tool's captured stdout. schtasks prints in the CONSOLE
/// code page (CP936/GBK on Chinese Windows — verified on this machine), so a
/// plain UTF-8 read garbles any non-ASCII in the task XML (e.g. an exe path
/// under `C:\Users\中文名\…`) into U+FFFD and every `contains` check on it
/// silently fails. Try strict UTF-8 first (covers ASCII and CP65001), then
/// convert from the real console/OEM code page.
#[cfg(windows)]
fn decode_console_bytes(bytes: &[u8]) -> String {
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    use windows_sys::Win32::Globalization::{GetOEMCP, MultiByteToWideChar};
    use windows_sys::Win32::System::Console::GetConsoleOutputCP;
    unsafe {
        let mut cp = GetConsoleOutputCP();
        if cp == 0 {
            cp = GetOEMCP();
        }
        let n = MultiByteToWideChar(cp, 0, bytes.as_ptr(), bytes.len() as i32, std::ptr::null_mut(), 0);
        if n <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        let mut wide = vec![0u16; n as usize];
        let n = MultiByteToWideChar(cp, 0, bytes.as_ptr(), bytes.len() as i32, wide.as_mut_ptr(), n);
        if n <= 0 {
            return String::from_utf8_lossy(bytes).into_owned();
        }
        String::from_utf16_lossy(&wide[..n as usize])
    }
}

/// The registered task's XML, decoded — `None` when the task doesn't exist (or
/// schtasks is unavailable). Shared by the "is it loaded / is it current"
/// checks so they judge the same bytes.
#[cfg(windows)]
fn query_task_xml() -> Option<String> {
    let mut cmd = Command::new("schtasks");
    cmd.args(["/Query", "/TN", TASK_NAME, "/XML"]);
    platform::no_window_std(&mut cmd);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    Some(decode_console_bytes(&out.stdout))
}

/// The exact command line the logon task must run for THIS binary + settings.
/// `--hidden` makes the supervisor hide the console window Windows hands a
/// console binary at logon; the flag stays supported forever so a task written
/// by an older install keeps starting newer binaries.
#[cfg(windows)]
fn task_arguments(base: &str, no_auto_update: bool) -> String {
    let mut args = format!("--base {base} supervise --hidden");
    if no_auto_update {
        args.push_str(" --no-auto-update");
    }
    args
}

#[cfg(windows)]
fn ensure_autostart(base: &str, no_auto_update: bool) -> Result<()> {
    let exe = std::env::current_exe()?;
    fs::create_dir_all(home().join(".mafold"))?;
    // Scope the logon trigger + run-as principal to the current user (the task
    // is per-user, like a launchd gui-domain agent / systemd --user unit).
    let username = std::env::var("USERNAME").context("USERNAME not set — can't scope the logon task")?;
    let user = match std::env::var("USERDOMAIN") {
        Ok(d) if !d.is_empty() => format!("{d}\\{username}"),
        _ => username,
    };
    let xml = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\n\
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\n\
  <RegistrationInfo>\n\
    <Description>Mafold bot supervisor — keeps your Mafold agent daemons running and restarts them at sign-in.</Description>\n\
    <URI>\\{task}</URI>\n\
  </RegistrationInfo>\n\
  <Triggers>\n\
    <LogonTrigger>\n\
      <Enabled>true</Enabled>\n\
      <UserId>{user}</UserId>\n\
    </LogonTrigger>\n\
    <TimeTrigger>\n\
      <StartBoundary>2020-01-01T00:00:00</StartBoundary>\n\
      <Repetition>\n\
        <Interval>PT30M</Interval>\n\
        <StopAtDurationEnd>false</StopAtDurationEnd>\n\
      </Repetition>\n\
      <Enabled>true</Enabled>\n\
    </TimeTrigger>\n\
  </Triggers>\n\
  <Principals>\n\
    <Principal id=\"Author\">\n\
      <UserId>{user}</UserId>\n\
      <LogonType>InteractiveToken</LogonType>\n\
      <RunLevel>LeastPrivilege</RunLevel>\n\
    </Principal>\n\
  </Principals>\n\
  <Settings>\n\
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\n\
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\n\
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\n\
    <AllowHardTerminate>true</AllowHardTerminate>\n\
    <StartWhenAvailable>true</StartWhenAvailable>\n\
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>\n\
    <IdleSettings>\n\
      <StopOnIdleEnd>false</StopOnIdleEnd>\n\
      <RestartOnIdle>false</RestartOnIdle>\n\
    </IdleSettings>\n\
    <AllowStartOnDemand>true</AllowStartOnDemand>\n\
    <Enabled>true</Enabled>\n\
    <Hidden>true</Hidden>\n\
    <RunOnlyIfIdle>false</RunOnlyIfIdle>\n\
    <WakeToRun>false</WakeToRun>\n\
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\n\
    <Priority>7</Priority>\n\
    <RestartOnFailure>\n\
      <Interval>PT1M</Interval>\n\
      <Count>999</Count>\n\
    </RestartOnFailure>\n\
  </Settings>\n\
  <Actions Context=\"Author\">\n\
    <Exec>\n\
      <Command>{exe}</Command>\n\
      <Arguments>{args}</Arguments>\n\
    </Exec>\n\
  </Actions>\n\
</Task>\n",
        task = TASK_NAME,
        user = xml_escape(&user),
        exe = xml_escape(&exe.display().to_string()),
        args = xml_escape(&task_arguments(base, no_auto_update)),
    );
    let xml_path = task_xml_path();
    write_utf16le(&xml_path, &xml)?;
    // /F re-registers in place — same idempotence as launchctl bootout→bootstrap.
    let mut create = Command::new("schtasks");
    create.args(["/Create", "/TN", TASK_NAME, "/XML"]).arg(&xml_path).arg("/F");
    platform::no_window_std(&mut create);
    let out = create.output().context("schtasks not available")?;
    if !out.status.success() {
        anyhow::bail!(
            "schtasks /Create failed: {}",
            decode_console_bytes(&out.stderr).trim()
        );
    }
    // A logon task only fires at the NEXT sign-in; `up` promises a supervisor
    // running NOW (launchctl bootstrap / systemctl enable --now semantics), so
    // kick the task immediately. If that fails, fall back to a plain detached
    // spawn — boot-persistence is already in place either way.
    if !run_task() && sup_running().is_none() {
        let _ = start_supervisor(base, no_auto_update);
    }
    Ok(())
}

/// Start the registered task NOW (so the supervisor runs under the task, where
/// the watchdog trigger and restart settings govern it). True on success.
#[cfg(windows)]
fn run_task() -> bool {
    let mut cmd = Command::new("schtasks");
    cmd.args(["/Run", "/TN", TASK_NAME]);
    platform::no_window_std(&mut cmd);
    cmd.output().map(|o| o.status.success()).unwrap_or(false)
}

#[cfg(windows)]
fn remove_autostart() {
    for args in [&["/End", "/TN", TASK_NAME][..], &["/Delete", "/TN", TASK_NAME, "/F"][..]] {
        let mut cmd = Command::new("schtasks");
        cmd.args(args);
        platform::no_window_std(&mut cmd);
        let _ = cmd.output();
    }
    let _ = fs::remove_file(task_xml_path());
}

/// Loaded = the task exists AND still points at THIS binary. The pointing check
/// matters: a task registered by an older install keeps its old command line
/// forever, and a binary/task drift is exactly how boot-persistence silently
/// broke once (task said `--hidden`, the reinstalled binary didn't know the
/// flag → exit code 2 at every logon). Any mismatch → not loaded → `up`
/// re-registers the task against the current binary.
#[cfg(windows)]
fn autostart_loaded() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let Some(xml) = query_task_xml() else { return false };
    xml.contains(&xml_escape(&exe.display().to_string())) && xml.contains("supervise")
}

/// Current = loaded AND the task's arguments are exactly what `up` would
/// register right now. `up`'s idempotence fast-path must use THIS, not
/// `autostart_loaded`: the task command line encodes `--base`/`--no-auto-update`,
/// so gating on "loaded" alone would silently ignore a re-run with changed
/// flags (e.g. `mafold up --no-auto-update` on a machine whose task was
/// registered without it — the user's intent would never reach the logon task).
#[cfg(windows)]
fn autostart_current(base: &str, no_auto_update: bool) -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let Some(xml) = query_task_xml() else { return false };
    xml.contains(&xml_escape(&exe.display().to_string()))
        && xml.contains(&format!("<Arguments>{}</Arguments>", xml_escape(&task_arguments(base, no_auto_update))))
}

/// The registered plist/unit now carries `--no-auto-update`, so "current" must
/// compare that flag too — otherwise `up --no-auto-update` after a plain `up` (or
/// vice-versa) would see a loaded service and skip re-registration, leaving the
/// wrong update policy persisted across the next reboot.
#[cfg(not(windows))]
fn autostart_current(_base: &str, no_auto_update: bool) -> bool {
    if !autostart_loaded() { return false; }
    #[cfg(target_os = "macos")]
    let registered = fs::read_to_string(launchagent_path()).map(|s| s.contains("--no-auto-update")).unwrap_or(false);
    #[cfg(target_os = "linux")]
    let registered = fs::read_to_string(systemd_unit_path()).map(|s| s.contains("--no-auto-update")).unwrap_or(false);
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    let registered = no_auto_update; // unsupported OS: unreachable (autostart_loaded() == false)
    registered == no_auto_update
}

#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn ensure_autostart(_base: &str, _no_auto_update: bool) -> Result<()> { anyhow::bail!("boot-persistence not supported on this OS") }
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn remove_autostart() {}
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
fn autostart_loaded() -> bool { false }

/// `mafold up` — bring the supervisor up as a boot-persistent service (one per
/// machine): it keeps every configured daemon running and OWNS updates. Autostart
/// is the default — once up, it starts on login and relaunches on crash, until
/// `mafold down`. Idempotent: re-running just confirms (the running supervisor
/// picks up add/rm within ~10s).
pub fn up(base: &str, no_auto_update: bool) -> Result<()> {
    let c = load();
    if c.daemons.is_empty() {
        println!("No daemons configured. Add one:\n  mafold --token mb_… add <bot> --workdir /path/to/repo");
        return Ok(());
    }
    if autostart_current(base, no_auto_update) {
        // Registered and pointing at this binary + these flags. On Windows
        // "registered" doesn't imply "alive" (no KeepAlive analogue — a crashed
        // supervisor stays down until the next logon/watchdog tick), and `up` is
        // exactly the command a user runs to fix an offline bot: revive it here
        // instead of just claiming success.
        #[cfg(windows)]
        if sup_running().is_none() {
            if !run_task() {
                let _ = start_supervisor(base, no_auto_update);
            }
            println!("↑ revived the supervisor (task was registered but no process was running)");
        }
        println!("✓ supervisor enabled (boot-persistent) — managing {} daemon(s); picks up changes within ~10s", c.daemons.len());
        return Ok(());
    }
    kill_supervisor_process(); // clear any stale detached supervisor first
    match ensure_autostart(base, no_auto_update) {
        Ok(()) => {
            println!("✓ supervisor enabled — managing {} daemon(s); starts on login + relaunches on crash", c.daemons.len());
            println!("  logs:   ~/.mafold/daemons/<name>/log  (supervisor: ~/.mafold/supervisor.log)");
            println!("  status: mafold status   ·   stop: mafold down");
        }
        Err(e) => {
            // No service manager → at least run it now (just not across reboots).
            eprintln!("note: couldn't enable autostart ({e}); running detached (won't survive reboot)");
            if sup_running().is_none() {
                let pid = start_supervisor(base, no_auto_update)?;
                println!("✓ supervisor running (pid {pid}) — managing {} daemon(s)", c.daemons.len());
            } else {
                println!("supervisor already running (detached)");
            }
        }
    }
    Ok(())
}

/// Spawn the detached `mafold supervise` process.
fn start_supervisor(base: &str, no_auto_update: bool) -> Result<u32> {
    fs::create_dir_all(daemons_dir())?;
    let exe = std::env::current_exe()?;
    let out = fs::OpenOptions::new().create(true).append(true).open(home().join(".mafold/supervisor.log"))?;
    let err = out.try_clone()?;
    let mut cmd = Command::new(exe);
    cmd.arg("--base").arg(base).arg("supervise");
    // Carry the caller's update setting into the detached process — this is the
    // same intent the service/task command line encodes on the managed paths.
    if no_auto_update {
        cmd.arg("--no-auto-update");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    platform::configure_detached(&mut cmd);
    let child = cmd.spawn().context("failed to spawn supervisor")?;
    let pid = child.id();
    writeln!(fs::File::create(sup_pid_path())?, "{pid}")?;
    Ok(pid)
}

/// The long-lived supervisor loop: keep every configured daemon running, and own
/// updates — check hourly, replace the shared binary safely (locked + verified),
/// then stop all daemons and re-exec self so the new supervisor respawns them on
/// the new version.
pub async fn supervise(base: String, auto_update: bool) {
    let _ = fs::create_dir_all(daemons_dir());
    if let Ok(mut f) = fs::File::create(sup_pid_path()) {
        let _ = writeln!(f, "{}", std::process::id());
    }
    println!("supervisor up (pid {}) · base {base}", std::process::id());
    let http = reqwest::Client::new();
    let mut ticks: u64 = 0;
    loop {
        reap(); // clear zombies so the liveness check below is accurate
        // A daemon whose bot was deleted server-side tombstones itself and
        // exits — drop it from the config BEFORE the respawn pass below.
        sweep_deprovisioned();
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
        // Update check every 10 min (60 × 10s), OR immediately when an agent child
        // nudged us (it relayed an events.cliUpdate) — so a new release lands in
        // seconds via the webhook path, not only on the 10-min poll. Gated by
        // `--no-auto-update` (a locally-patched install must not be clobbered by
        // an official release; the nudge is still consumed so the file can't pile
        // up). A version that just failed to apply (e.g. the release download is
        // unreachable from this network) is under cooldown — retried later, not
        // on every tick.
        ticks += 1;
        let nudged = crate::update::take_nudge();
        if auto_update && (nudged || ticks % 60 == 0) {
            if nudged { println!("↻ update nudge from a daemon — checking now"); }
            match crate::update::check(&http).await {
                Ok(Some(r)) if crate::update::recently_failed(&r.version) => {}
                Ok(Some(r)) => {
                    println!("↻ update v{} available — applying + restarting daemons…", r.version);
                    match crate::update::apply(&http, &r.url, &r.version, r.sha256.as_deref()).await {
                        Ok(()) => {
                            // Graceful drain: download is done; now let in-flight turns
                            // finish before the kill+reexec, so a daemon watching its
                            // own release doesn't cut off a reply. Capped so a stuck
                            // turn can't block updates forever.
                            drain_daemons(&cfg.daemons, std::time::Duration::from_secs(300)).await;
                            for d in &cfg.daemons { let _ = stop_one(&d.name); }
                            let _ = crate::update::reexec(); // new supervisor respawns daemons (new binary)
                        }
                        Err(e) => {
                            crate::update::mark_failed(&r.version);
                            eprintln!("update to v{} failed ({e:#}) — keeping the current version, retrying in ~1h", r.version);
                        }
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!("update check failed: {e}"),
            }
        }
        // Control plane (after `mafold login`): claim any auto-provisioned bots
        // (→ add + start their daemons, no `mafold add` paste) and keep this
        // machine's harness report fresh (~every 30s) so New-Bot sees it online.
        if let Some(sess) = crate::session::load() {
            if let Err(e) = poll_provisions(&http, &base, &sess, !auto_update).await {
                eprintln!("provision poll failed: {e}");
            }
            if ticks % 3 == 0 {
                let _ = report_local_harnesses(&http, &base, &sess).await;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
}

/// Claim auto-provision requests for this device and wire each bot into the local
/// config (the next loop tick starts its daemon). The bot's mb_ token arrives over
/// the owner's authenticated session — no copy-paste. `no_auto_update` carries the
/// supervisor's own update setting into the (re)registered autostart entry.
async fn poll_provisions(http: &reqwest::Client, base: &str, sess: &crate::session::Session, no_auto_update: bool) -> Result<()> {
    let resp: serde_json::Value = http
        .post(format!("{base}/api/claimProvisions"))
        .bearer_auth(&sess.token)
        .json(&serde_json::json!({ "device_id": sess.device_id }))
        .send()
        .await?
        .json()
        .await?;
    let items = resp.pointer("/result/items").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    for it in items {
        let name = it["bot_username"].as_str().unwrap_or_default().to_string();
        let token = it["token"].as_str().unwrap_or_default().to_string();
        if name.is_empty() || token.is_empty() {
            continue;
        }
        let harness = it["harness"].as_str().map(str::to_string);
        let workdir = provision_workdir(&name);
        match add(name.clone(), token, workdir, harness, base, no_auto_update) {
            Ok(()) => println!("⬇ provisioned @{name} — its daemon starts on the next tick"),
            Err(e) => eprintln!("provision @{name} failed: {e}"),
        }
    }
    Ok(())
}

/// Re-send this machine's available harnesses (keeps the device "online").
async fn report_local_harnesses(http: &reqwest::Client, base: &str, sess: &crate::session::Session) -> Result<()> {
    let harnesses: Vec<serde_json::Value> = crate::harness::probe()
        .into_iter()
        .map(|(id, available)| serde_json::json!({ "id": id, "available": available }))
        .collect();
    http.post(format!("{base}/api/reportHarnesses"))
        .bearer_auth(&sess.token)
        .json(&serde_json::json!({
            "device_id": sess.device_id,
            "device_name": sess.device_name,
            "harnesses": harnesses,
        }))
        .send()
        .await?;
    Ok(())
}

/// Default workdir for an auto-provisioned bot: `~/.mafold/work/<label>` (created).
fn provision_workdir(bot_username: &str) -> String {
    let label = bot_username.rsplit(':').next().unwrap_or(bot_username);
    let dir = home().join(".mafold/work").join(label);
    let _ = fs::create_dir_all(&dir);
    dir.to_string_lossy().into_owned()
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
        // So the daemon writes its busy marker at the path the supervisor's drain
        // check reads (keyed by THIS exact daemon name).
        .env("MAFOLD_DAEMON_NAME", &d.name)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    // New session / no console → detached from the controlling terminal.
    platform::configure_detached(&mut cmd);
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
            // Group kill: take the daemon's harness children (claude etc.)
            // with it — a plain pid kill orphans an in-flight claude to ppid 1,
            // where it burns tokens streaming into a dead pipe (and its draft
            // stays "generating" forever).
            platform::terminate_group(pid);
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
