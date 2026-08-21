//! The half of a `computer` connection that actually runs something.
//!
//! The core decides *what* was asked (`mafold_core::computer` parses the
//! request and owns the limits); this decides *how it runs on this box*, and it
//! lives in the cli because the cli is the only surface that should ever spawn
//! a process. A browser tab and an iOS app link the same core and simply never
//! install this — which is why they decline a shell call instead of claiming
//! one they cannot serve.
//!
//! Two lifetimes, deliberately:
//!
//!   * `shell.exec` is **tied to the call**. It is killed at the core's cap and
//!     answers with whatever it printed, because a caller parked on a relayed
//!     request needs an answer more than it needs completeness.
//!   * `shell.spawn` **outlives everything** — this turn, the socket, the
//!     daemon's restart. Same trick as `bash_hook`: a new session (`setsid`),
//!     output appended to a log file, pid on disk. `shell.status` reads that
//!     registry back, so a ten-minute build is four short calls rather than one
//!     long one nothing can wait for.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use serde_json::{json, Value};
use tokio::io::AsyncReadExt;

use mafold_core::computer::{Job, MAX_OUTPUT};
use mafold_core::connections::Executor;

type Result<T> = std::result::Result<T, String>;

/// Hand this to `Runtime::attach_computer` — it is what makes a process a
/// machine the user can call.
pub fn executor() -> Executor {
    Arc::new(|job| Box::pin(run(job)))
}

async fn run(job: Job) -> Result<Value> {
    match job {
        Job::Exec {
            cmd,
            cwd,
            timeout_ms,
        } => exec(&cmd, cwd.as_deref(), timeout_ms).await,
        Job::Spawn { cmd, cwd } => spawn(&cmd, cwd.as_deref()).await,
        Job::Status { task_id, tail } => status(&task_id, tail),
        Job::Kill { task_id } => kill(&task_id),
    }
}

// ── where the shell comes from ────────────────────────────────────────────

/// The user's own login shell, run as a LOGIN shell.
///
/// `-l` costs a few milliseconds of profile sourcing and buys the thing that
/// makes this feature usable at all: `PATH`. A daemon started by launchd or by
/// a service manager inherits a minimal environment, so `sh -c "pnpm build"` on
/// a machine with pnpm installed answers `command not found` — a failure that
/// looks like a Mafold bug and is really a missing profile. Running what the
/// user runs makes "it works in my terminal" true here too.
#[cfg(unix)]
fn shell() -> (String, Vec<String>) {
    let sh = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "/bin/bash".to_string());
    (sh, vec!["-lc".to_string()])
}

#[cfg(not(unix))]
fn shell() -> (String, Vec<String>) {
    (
        std::env::var("COMSPEC").unwrap_or_else(|_| "cmd.exe".to_string()),
        vec!["/C".to_string()],
    )
}

/// Where a command runs when the caller names no directory.
fn home() -> PathBuf {
    std::env::var("HOME")
        .or_else(|_| std::env::var("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

fn workdir(cwd: Option<&str>) -> Result<PathBuf> {
    let dir = cwd.map(PathBuf::from).unwrap_or_else(home);
    if !dir.is_dir() {
        // Named before the command runs. A shell would answer this as its own
        // failure with an exit code the caller has to guess about.
        return Err(format!("no such directory on this machine: {}", dir.display()));
    }
    Ok(dir)
}

// ── shell.exec ────────────────────────────────────────────────────────────

async fn exec(cmd: &str, cwd: Option<&str>, timeout_ms: u64) -> Result<Value> {
    let dir = workdir(cwd)?;
    let (sh, args) = shell();
    let mut command = tokio::process::Command::new(&sh);
    command
        .args(&args)
        .arg(cmd)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Its own process group, so a timeout can kill the whole tree. Without it,
    // `npm run dev &` leaves a server running that nothing will ever stop, and
    // the machine's owner finds out from a busy port.
    #[cfg(unix)]
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let started = std::time::Instant::now();
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start `{sh}`: {e}"))?;
    let pid = child.id();
    let out = child.stdout.take().map(read_capped);
    let err = child.stderr.take().map(read_capped);
    let (out, err) = (
        tokio::spawn(async move {
            match out {
                Some(f) => f.await,
                None => (String::new(), false),
            }
        }),
        tokio::spawn(async move {
            match err {
                Some(f) => f.await,
                None => (String::new(), false),
            }
        }),
    );

    let deadline = std::time::Duration::from_millis(timeout_ms);
    let (code, timed_out) = match tokio::time::timeout(deadline, child.wait()).await {
        Ok(Ok(status)) => (status.code(), false),
        Ok(Err(e)) => return Err(format!("{cmd}: {e}")),
        Err(_) => {
            // Kill the GROUP: the shell may have exited already, leaving the
            // thing we actually care about running under it.
            if let Some(pid) = pid {
                kill_group(pid as i32);
            }
            let _ = child.wait().await;
            (None, true)
        }
    };
    // The readers finish when the pipes close, which the kill above guarantees.
    let (stdout, out_trunc) = out.await.unwrap_or_default();
    let (stderr, err_trunc) = err.await.unwrap_or_default();

    Ok(json!({
        "exit_code": code,
        "stdout": stdout,
        "stderr": stderr,
        "truncated": out_trunc || err_trunc,
        "timed_out": timed_out,
        "duration_ms": started.elapsed().as_millis() as u64,
        "cwd": dir.to_string_lossy(),
    }))
}

/// Read a stream, keeping at most [`MAX_OUTPUT`] bytes but DRAINING the rest.
///
/// Draining is the load-bearing half. Simply stopping at the cap leaves the
/// pipe full, which blocks the child on its next write — so a chatty command
/// would hang forever instead of being truncated, and the caller would see a
/// timeout with no output at all.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> (String, bool) {
    let mut kept: Vec<u8> = Vec::new();
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let room = MAX_OUTPUT.saturating_sub(kept.len());
                if room == 0 {
                    truncated = true;
                    continue;
                }
                let take = room.min(n);
                kept.extend_from_slice(&buf[..take]);
                truncated |= take < n;
            }
        }
    }
    (String::from_utf8_lossy(&kept).into_owned(), truncated)
}

#[cfg(unix)]
fn kill_group(pid: i32) {
    // The group first (that is the point of `setsid`), then the pid itself in
    // case the group is gone but the process isn't.
    unsafe {
        libc::killpg(pid, libc::SIGTERM);
        libc::kill(pid, libc::SIGTERM);
    }
}

#[cfg(not(unix))]
fn kill_group(_pid: i32) {}

// ── shell.spawn / status / kill ───────────────────────────────────────────

/// Where detached tasks live. A sibling of `~/.mafold/bgtasks`, not the same
/// directory: those belong to a conversation's agent turn and are swept when
/// that turn reports them, and a task started over a connection has no turn.
fn tasks_dir() -> Result<PathBuf> {
    let dir = home().join(".mafold").join("computer");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    Ok(dir)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Task ids are readable and sortable on purpose: they end up in a chat, and a
/// human reading `t-1755830000123-…` can at least tell which of two is newer.
///
/// The pid and counter are not decoration. A millisecond is a long time to a
/// caller firing two `shell.spawn`s in a row, and two tasks sharing an id share
/// a log file, a pid file and an exit code — the second one's status silently
/// describes the first. (Caught by two tests spawning in the same millisecond,
/// which is exactly how an agent behaves.)
fn new_task_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "t-{}-{:x}{:x}",
        now_ms(),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(unix)]
async fn spawn(cmd: &str, cwd: Option<&str>) -> Result<Value> {
    let dir = workdir(cwd)?;
    let root = tasks_dir()?;
    sweep_old(&root);
    let id = new_task_id();
    let (script, log, exit) = (
        root.join(format!("{id}.sh")),
        root.join(format!("{id}.log")),
        root.join(format!("{id}.exit")),
    );
    // The exit code is written by the script itself. Nothing else can observe
    // it: the daemon does not stay to reap a process it deliberately detached,
    // and a pid that is simply gone tells you nothing about how it went.
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n{cmd}\nprintf '%s' \"$?\" > {}\n",
            single_quote(&exit.to_string_lossy())
        ),
    )
    .map_err(|e| format!("{}: {e}", script.display()))?;

    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|e| format!("{}: {e}", log.display()))?;
    let err = out.try_clone().map_err(|e| e.to_string())?;
    let mut command = tokio::process::Command::new("/bin/sh");
    command
        .arg(&script)
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err));
    // SAFETY: setsid() is async-signal-safe and the closure calls nothing else.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("could not start it: {e}"))?;
    let pid = child.id().unwrap_or_default();

    // REAP IT. `bash_hook` gets away without this because the hook process
    // exits immediately and init adopts the task; a daemon stays, so an exited
    // child of ours becomes a zombie — and a zombie answers `kill(pid, 0)`,
    // which is exactly how `shell.status` decides something is still running.
    // Without this a killed task reports `running: true` forever.
    tokio::spawn(async move {
        let _ = child.wait().await;
    });

    std::fs::write(root.join(format!("{id}.pid")), pid.to_string()).ok();
    std::fs::write(
        root.join(format!("{id}.meta")),
        json!({ "cmd": cmd, "cwd": dir.to_string_lossy(), "started_at": now_ms() }).to_string(),
    )
    .ok();
    Ok(json!({
        "task_id": id,
        "pid": pid,
        "log": log.to_string_lossy(),
        "cwd": dir.to_string_lossy(),
    }))
}

#[cfg(not(unix))]
async fn spawn(_cmd: &str, _cwd: Option<&str>) -> Result<Value> {
    Err("shell.spawn needs a POSIX machine — this one can only run shell.exec".into())
}

fn status(task_id: &str, tail: usize) -> Result<Value> {
    let root = tasks_dir()?;
    let id = safe_id(task_id)?;
    let meta: Value = std::fs::read_to_string(root.join(format!("{id}.meta")))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null);
    let pid: Option<i32> = std::fs::read_to_string(root.join(format!("{id}.pid")))
        .ok()
        .and_then(|s| s.trim().parse().ok());
    if pid.is_none() && meta.is_null() {
        return Err(format!(
            "no task `{task_id}` on this machine — it may have been swept, or started \
             somewhere else"
        ));
    }
    let exit_code: Option<i32> = std::fs::read_to_string(root.join(format!("{id}.exit")))
        .ok()
        .and_then(|s| s.trim().parse().ok());
    // Alive means "the pid answers signal 0", not "no exit file yet": a task
    // killed from outside writes no exit code at all, and reporting it as
    // running forever is how a caller waits on nothing.
    let running = exit_code.is_none() && pid.is_some_and(alive);
    let log = read_tail(&root.join(format!("{id}.log")), tail);
    Ok(json!({
        "task_id": id,
        "running": running,
        "exit_code": exit_code,
        // A killed task writes no exit code — its script never got to that
        // line. Without this flag "stopped, exit_code: null" is indistinguishable
        // from "the machine rebooted under it", and only one of those is
        // something the caller did.
        "killed": root.join(format!("{id}.killed")).exists(),
        "pid": pid,
        "cmd": meta.get("cmd").cloned().unwrap_or(Value::Null),
        "cwd": meta.get("cwd").cloned().unwrap_or(Value::Null),
        "started_at": meta.get("started_at").cloned().unwrap_or(Value::Null),
        "output": log.0,
        "truncated": log.1,
    }))
}

fn kill(task_id: &str) -> Result<Value> {
    let root = tasks_dir()?;
    let id = safe_id(task_id)?;
    let pid: i32 = std::fs::read_to_string(root.join(format!("{id}.pid")))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .ok_or_else(|| format!("no task `{task_id}` on this machine"))?;
    let was = alive(pid);
    kill_group(pid);
    std::fs::write(root.join(format!("{id}.killed")), now_ms().to_string()).ok();
    Ok(json!({ "task_id": id, "killed": was, "pid": pid }))
}

#[cfg(unix)]
fn alive(pid: i32) -> bool {
    // Signal 0: "does this pid exist and may I signal it" — no signal is sent.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn alive(_pid: i32) -> bool {
    false
}

/// Last `n` lines of a log, and whether anything was left out.
fn read_tail(path: &Path, n: usize) -> (String, bool) {
    let Ok(text) = std::fs::read_to_string(path) else {
        return (String::new(), false);
    };
    let lines: Vec<&str> = text.lines().collect();
    let cut = lines.len().saturating_sub(n);
    let kept = lines[cut..].join("\n");
    // Cap the bytes too: 5000 lines of a webpack build is not a chat message.
    if kept.len() > MAX_OUTPUT {
        let start = kept.len() - MAX_OUTPUT;
        return (kept[start..].to_string(), true);
    }
    (kept, cut > 0)
}

/// A task id becomes a FILENAME, so it gets the same alphabet the api's
/// connection names get. `../../.ssh/id_rsa` is a task id right up until it
/// isn't.
fn safe_id(raw: &str) -> Result<String> {
    let id: String = raw
        .trim()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if id.is_empty() {
        return Err("task_id is empty".into());
    }
    Ok(id)
}

fn single_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// Best-effort GC of task artifacts older than a week — the same rule
/// `bash_hook` uses, for the same reason: nothing else ever deletes these.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exec_returns_output_and_an_exit_code() {
        let out = exec("echo hello && exit 3", Some("/tmp"), 5_000).await.unwrap();
        assert_eq!(out["exit_code"], json!(3));
        assert!(out["stdout"].as_str().unwrap().contains("hello"));
        assert_eq!(out["timed_out"], json!(false));
    }

    #[tokio::test]
    async fn stderr_comes_back_separately() {
        let out = exec("echo oops 1>&2", Some("/tmp"), 5_000).await.unwrap();
        assert!(out["stderr"].as_str().unwrap().contains("oops"));
        assert_eq!(out["stdout"].as_str().unwrap(), "");
    }

    /// A timeout must answer with what the command PRINTED, not with an error.
    /// "no output, timed out" and "here is where it got to, then it hung" are
    /// the same failure and completely different bug reports.
    #[tokio::test]
    async fn a_timeout_keeps_the_partial_output() {
        let out = exec("echo before; sleep 30", Some("/tmp"), 400).await.unwrap();
        assert_eq!(out["timed_out"], json!(true));
        assert_eq!(out["exit_code"], Value::Null);
        assert!(
            out["stdout"].as_str().unwrap().contains("before"),
            "partial output must survive the kill: {out}"
        );
    }

    /// The cap exists so a relayed answer stays a message rather than a dump —
    /// and the command must still FINISH, which is what draining buys.
    #[tokio::test]
    async fn a_flood_is_truncated_and_still_completes() {
        let out = exec(
            "for i in $(seq 1 40000); do echo 0123456789012345678901234567890123456789; done",
            Some("/tmp"),
            20_000,
        )
        .await
        .unwrap();
        assert_eq!(out["timed_out"], json!(false), "draining keeps it unblocked");
        assert_eq!(out["exit_code"], json!(0));
        assert_eq!(out["truncated"], json!(true));
        assert!(out["stdout"].as_str().unwrap().len() <= MAX_OUTPUT);
    }

    #[tokio::test]
    async fn a_missing_directory_is_named_before_anything_runs() {
        let e = exec("echo x", Some("/no/such/place/at/all"), 5_000)
            .await
            .unwrap_err();
        assert!(e.contains("/no/such/place/at/all"), "{e}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_spawned_task_outlives_the_call_and_reports_its_exit_code() {
        let started = spawn("echo working; sleep 1; echo done", Some("/tmp"))
            .await
            .unwrap();
        let id = started["task_id"].as_str().unwrap().to_string();
        let early = status(&id, 50).unwrap();
        assert_eq!(early["running"], json!(true), "{early}");

        // Poll rather than sleep-and-hope: a loaded CI box is slower than any
        // number this test could hardcode.
        let mut done = early;
        for _ in 0..100 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            done = status(&id, 50).unwrap();
            if done["running"] == json!(false) {
                break;
            }
        }
        assert_eq!(done["running"], json!(false), "{done}");
        assert_eq!(done["exit_code"], json!(0), "{done}");
        let output = done["output"].as_str().unwrap();
        assert!(output.contains("working") && output.contains("done"), "{output}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn killing_a_task_stops_it_and_status_stops_saying_running() {
        let started = spawn("sleep 60", Some("/tmp")).await.unwrap();
        let id = started["task_id"].as_str().unwrap().to_string();
        assert_eq!(kill(&id).unwrap()["killed"], json!(true));
        let mut st = status(&id, 10).unwrap();
        for _ in 0..50 {
            if st["running"] == json!(false) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            st = status(&id, 10).unwrap();
        }
        // The regression this test caught: an unreaped child stays a zombie,
        // a zombie answers `kill(pid, 0)`, and the task reported itself
        // running forever — with a `sleep 60` that was very much dead.
        assert_eq!(st["running"], json!(false), "{st}");
        assert_eq!(st["killed"], json!(true), "{st}");
        assert_eq!(st["exit_code"], Value::Null, "a killed script never wrote one");
    }

    #[test]
    fn a_task_id_cannot_walk_out_of_its_directory() {
        // Dots go too, so no id can name a parent directory or a dotfile.
        assert_eq!(safe_id("../../.ssh/id_rsa").unwrap(), "sshid_rsa");
        assert!(safe_id("  ").is_err());
    }

    #[test]
    fn an_unknown_task_says_so_rather_than_reporting_a_blank_one() {
        let e = status("t-does-not-exist", 10).unwrap_err();
        assert!(e.contains("no task"), "{e}");
    }

    // ── the whole chain ──

    /// A path-routed HTTP stub: enough api for the real core to relay against.
    /// Returns its base and every (path, body) it saw.
    async fn stub_api(items: String) -> (String, Arc<std::sync::Mutex<Vec<(String, String)>>>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<std::sync::Mutex<Vec<(String, String)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let (log, items) = (log.clone(), items.clone());
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let (path, body) = loop {
                        match sock.read(&mut chunk).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => buf.extend_from_slice(&chunk[..n]),
                        }
                        let text = String::from_utf8_lossy(&buf).into_owned();
                        let Some(end) = text.find("\r\n\r\n") else { continue };
                        let head = text[..end].to_string();
                        let want: usize = head
                            .lines()
                            .find(|l| l.to_ascii_lowercase().starts_with("content-length:"))
                            .and_then(|l| l.split(':').nth(1))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        let body = text[end + 4..].to_string();
                        if body.len() < want {
                            continue;
                        }
                        break (
                            head.lines()
                                .next()
                                .and_then(|l| l.split(' ').nth(1))
                                .unwrap_or("")
                                .to_string(),
                            body,
                        );
                    };
                    log.lock().unwrap().push((path.clone(), body));
                    let result = match path.as_str() {
                        "/listConnections" => format!("{{\"items\":[{items}]}}"),
                        "/claimConnectionCall" => "{\"claimed\":true}".to_string(),
                        _ => "null".to_string(),
                    };
                    let reply = format!("{{\"ok\":true,\"result\":{result}}}");
                    let out = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                        reply.len()
                    );
                    let _ = sock.write_all(out.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        (format!("http://{addr}"), seen)
    }

    /// End to end, with nothing stubbed but the api: a relayed
    /// `events.connectionCall` reaches the core, the core opens a real sealed
    /// binding, this executor runs a REAL process, and the output goes back on
    /// `answerConnectionCall`.
    ///
    /// The unit tests either side of this both passed while the wiring was
    /// missing — a `Runtime` built without `attach_computer` declines every
    /// shell call in silence, which is invisible to both.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_relayed_call_runs_a_real_process_and_answers_with_its_output() {
        use mafold_core::mafold_types::connections::provider_infos;
        let umk = crate::vault::Key::random();
        let sealed = crate::vault::seal_payload(
            &umk,
            &json!({ "device_id": "device-A", "machine": "test-box" }).to_string(),
        );
        let conn = json!({
            "name": "laptop",
            "provider": "computer",
            "label": "test-box",
            "blob": sealed.blob,
            "wrapped_dek": sealed.wrapped_dek,
            "key_id": "k1",
        });
        let (base, seen) = stub_api(conn.to_string()).await;
        mafold_core::providers::install_unverified_for_tests(1, provider_infos(), now_ms());

        let mut rt = mafold_core::connections::Runtime::new(&base, "s_tok", umk);
        rt.attach_computer("device-A", executor());
        let handled = mafold_core::connections::handle_event(
            &mut rt,
            &json!({
                "method": "events.connectionCall",
                "params": {
                    "call_id": "c-1",
                    "connection": "laptop",
                    "method": "shell.exec",
                    "params": { "cmd": "echo mafold-e2e", "cwd": "/tmp" },
                },
            })
            .to_string(),
        )
        .await;

        assert!(handled);
        let calls = seen.lock().unwrap().clone();
        let answer = calls
            .iter()
            .find(|(p, _)| p == "/answerConnectionCall")
            .unwrap_or_else(|| panic!("never answered; saw {:?}", calls.iter().map(|c| &c.0).collect::<Vec<_>>()));
        assert!(answer.1.contains("mafold-e2e"), "{}", answer.1);
        assert!(!answer.1.contains("\"error\""), "{}", answer.1);
    }
}
