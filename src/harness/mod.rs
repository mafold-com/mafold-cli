//! Harness abstraction — the pluggable coding-agent backend a daemon drives.
//!
//! Implemented harnesses: **Claude Code**, **Codex**, and **Kimi Code**; planned:
//! `opencode`, `openclaw`. Each harness knows how to invoke its CLI headlessly and
//! normalize that CLI's output into [`AgentEvent`]s. The renderer (`crate::render`) turns
//! those events into chat text + cards, so card rendering is identical across
//! harnesses — a new harness only has to emit the common event stream.
//!
//! A `Daemon` (one bot presence) is `(token + workdir + harness + model)`; the
//! supervisor runs many daemons, one process per bot.

pub mod claude_code;
pub mod codex;
mod codex_stats;
// Codex uses this transport for native control data, the live thread/turn path,
// and reply-scoped server-request approvals.
#[allow(dead_code)]
pub mod codex_app_server;
pub mod kimi_code;

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::{mpsc::UnboundedSender, Notify};

use crate::client::Client;

/// PIDs of harness child processes (claude runs) currently IN FLIGHT. The
/// daemon's shutdown handler kills exactly these — never the process group:
/// legitimate background tasks the agent left running (run_in_background
/// shells) share the daemon's pgroup and MUST survive a daemon restart
/// (0.9.46's group kill wrongly took them down — the 2026-07-19 regression).
pub fn live_children() -> &'static Mutex<HashSet<u32>> {
    static LIVE: OnceLock<Mutex<HashSet<u32>>> = OnceLock::new();
    LIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

/// RAII registration of a harness child pid in [`live_children`] — deregisters
/// on drop, so every exit path (clean, error, panic) cleans up.
pub struct ChildGuard(Option<u32>);
impl ChildGuard {
    pub fn new(pid: Option<u32>) -> Self {
        if let Some(p) = pid {
            live_children().lock().unwrap().insert(p);
        }
        Self(pid)
    }
}
impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(p) = self.0 {
            live_children().lock().unwrap().remove(&p);
        }
    }
}

/// The normalized event a harness turn speaks. Defined in `mafold-transcript`
/// and re-exported here, because the vocabulary is not the daemon's: the api's
/// own brains emit the same events and get the same cards. Harness-specific
/// output formats (Claude Code stream-json, etc.) are parsed down to it.
pub use mafold_transcript::AgentEvent;

/// One turn to run against a harness.
pub struct Turn {
    pub prompt: String,
    /// The conversation id this turn runs in — exported to the agent's process
    /// env as `MAFOLD_CONV` so `mafold room …` (the room skill) targets it.
    pub conv: String,
    /// The fully scoped SURFACE this turn runs on: bot, harness, conversation,
    /// forum channel and cwd. Exported as `MAFOLD_SURFACE` and used by the
    /// bash-hook as the `~/.mafold/bgtasks` registry key, so a detached task is
    /// only picked up by the daemon/session/tree that created it.
    pub surface: String,
    /// The in-flight reply's message id — the draft this turn is streaming
    /// into. Exported as `MAFOLD_DRAFT` so `mafold attach <file>` can hang
    /// media on THIS reply, which is what lets an agent send a picture in the
    /// same bubble as the words about it. The uniform door: every harness gets
    /// it, whether or not it can also produce images natively.
    pub draft: String,
    pub workdir: String,
    /// The harness's prior session id for this conversation, to resume context.
    pub session: Option<String>,
    /// Per-chat model override (`/model`), or None for the harness default.
    pub model: Option<String>,
    /// Reasoning-effort level (`low`/`medium`/`high`/`xhigh`/`max`), or None for
    /// the harness default. Maps to Claude Code's `--effort`.
    pub effort: Option<String>,
    /// Extended-thinking budget in tokens (`/think`), or None for the harness
    /// default (off). Maps to Claude Code's `MAX_THINKING_TOKENS`.
    pub thinking: Option<u32>,
    /// Fires when `/stop` is invoked — the harness must kill its child and stop.
    pub cancel: Arc<Notify>,
    /// Extra system-prompt context (the daemon's mafold preamble: identity, the
    /// current conversation, embeddable cards). Appended to the agent's own
    /// system prompt; None = a pure, mafold-unaware agent.
    pub system: Option<String>,
    /// Per-turn file the AskUserQuestion PreToolUse hook waits on: the daemon
    /// writes the user's chat-card answer here, the hook reads it and feeds it
    /// back to the model. None = interactive ask unsupported for this harness.
    pub ask_file: Option<String>,
    /// Per-turn file carrying what the user said WHILE this turn was running.
    /// The daemon appends; a PostToolUse hook drains it and hands the text to
    /// the model as the next tool result's context, so a long run is corrected
    /// at the next tool boundary instead of killed and restarted. None = this
    /// harness can't be steered mid-turn (the message queues for the next one).
    pub steer_file: Option<String>,
}

/// Outcome of a turn.
#[derive(Default)]
pub struct TurnOutcome {
    /// Any content (text or a tool event) was produced.
    pub produced: bool,
    /// The turn was interrupted by `/stop`.
    pub stopped: bool,
    /// The (possibly new) session id to persist for this conversation.
    pub session: Option<String>,
    /// Set when the agent ended on an API / execution error (an `is_error`
    /// result, or a fatal error event mid-stream) rather than completing — the
    /// specific reason to surface to the user. The turn stops cleanly and the
    /// session is still persisted, so a retry resumes with context.
    pub error: Option<String>,
}

/// Where a slash command lands when not a daemon control command.
/// (`Handled` is for harnesses whose command posts its own messages; Claude
/// Code only returns `Reply`/`Forward` today.)
#[allow(dead_code)]
pub enum CommandOutcome {
    /// Handled locally — send this markdown reply.
    Reply(String),
    /// Handled locally; the harness already posted its own messages.
    Handled,
    /// Not a harness command — forward the raw text to the harness as a prompt.
    Forward,
}

/// One model advertised by a harness's live model catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelChoice {
    /// Picker identity returned by the harness.
    pub id: String,
    /// Actual model slug passed to a turn.
    pub model: String,
    pub display_name: String,
    pub description: String,
    pub is_default: bool,
    pub default_effort: Option<String>,
    pub efforts: Vec<String>,
}

/// One rate-limit window on the subscription seat behind a harness, normalized
/// across harnesses (Claude Code's `limits[]`, Codex's `primary`/`secondary`).
#[derive(Debug, Clone, Serialize)]
pub struct SeatLimit {
    /// Harness-native window id — `"session"` / `"weekly_all"` /
    /// `"weekly_scoped"` for Claude Code, `"<limitId>.primary"` for Codex.
    /// Opaque to the server: it groups and dedupes, it does not interpret.
    pub kind: String,
    /// Display label ("Session", "Week (all models)").
    pub label: String,
    /// 0–100. The occupancy that actually matters when lending a seat.
    pub percent: f64,
    /// Epoch seconds the window rolls over, when the harness reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<i64>,
}

/// Liveness of the subscription seat behind a harness — "is this seat usable
/// right now", as distinct from [`Harness::available`] ("is the CLI installed").
///
/// A seat can be installed and dead: credential expired, account no longer
/// eligible, window exhausted. Lending, quota and supply decisions all need the
/// second question answered, and only the machine holding the credential can
/// answer it — so it rides the existing `reportHarnesses` heartbeat rather than
/// a channel of its own.
///
/// Everything here is BEST EFFORT and self-reported by the host machine. It is
/// operational telemetry for the seat's own owner, never an authorization input
/// (§15: a flag is not a security boundary) and never billing evidence.
#[derive(Debug, Clone, Serialize)]
pub struct SeatHealth {
    /// One of:
    /// - `ok` — credential valid, upstream answered.
    /// - `exhausted` — answered, but a window is full (100%).
    /// - `unauthenticated` — no readable credential, or upstream said 401.
    ///   The owner has to log in again.
    /// - `rejected` — upstream said 403: the account is no longer allowed to
    ///   use this seat (eligibility / suspension). Distinct from `unauthenticated`
    ///   because re-logging-in does NOT fix it.
    /// - `unreachable` — network failure, timeout, or an unexpected status.
    /// - `unknown` — this harness has no way to probe its seat.
    pub state: String,
    /// Upstream HTTP status when the probe got one — the raw signal behind
    /// `state`, kept so a new upstream behaviour is diagnosable without a CLI
    /// release.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Plan/tier label the seat reports ("Max (20x)", "Pro").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tier: Option<String>,
    /// Rate-limit windows, most-occupied first.
    pub limits: Vec<SeatLimit>,
    /// Epoch seconds this probe ran.
    pub checked_at: i64,
}

impl SeatHealth {
    /// Epoch seconds now (probe stamp).
    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }

    /// A seat with nothing to report — the default for harnesses that cannot
    /// probe. Deliberately NOT `ok`: "we didn't look" and "we looked and it's
    /// fine" must never collapse into the same value, or every consumer
    /// silently treats unprobeable seats as healthy.
    pub fn unknown() -> Self {
        Self {
            state: "unknown".into(),
            status: None,
            tier: None,
            limits: Vec::new(),
            checked_at: Self::now(),
        }
    }

    /// Terminal-ish states carry no windows; build one from a status code.
    pub fn from_status(status: u16) -> Self {
        let state = match status {
            401 => "unauthenticated",
            403 => "rejected",
            _ => "unreachable",
        };
        Self {
            state: state.into(),
            status: Some(status),
            tier: None,
            limits: Vec::new(),
            checked_at: Self::now(),
        }
    }

    /// A live probe: `ok`, or `exhausted` when any window is full.
    pub fn from_limits(tier: Option<String>, mut limits: Vec<SeatLimit>) -> Self {
        limits.sort_by(|a, b| b.percent.total_cmp(&a.percent));
        let exhausted = limits.iter().any(|l| l.percent >= 100.0);
        Self {
            state: if exhausted { "exhausted" } else { "ok" }.into(),
            status: Some(200),
            tier,
            limits,
            checked_at: Self::now(),
        }
    }

    /// No credential on disk / unreadable — same remedy as a 401, but there was
    /// no request to get a status from.
    pub fn unauthenticated() -> Self {
        Self {
            state: "unauthenticated".into(),
            status: None,
            tier: None,
            limits: Vec::new(),
            checked_at: Self::now(),
        }
    }

    /// The probe never got an answer (DNS, TLS, timeout).
    pub fn unreachable() -> Self {
        Self {
            state: "unreachable".into(),
            status: None,
            tier: None,
            limits: Vec::new(),
            checked_at: Self::now(),
        }
    }
}

/// A pluggable coding-agent backend.
#[async_trait]
pub trait Harness: Send + Sync {
    /// Stable id — `"claude-code"`, `"opencode"`, `"codex"`, `"openclaw"`.
    fn id(&self) -> &'static str;

    /// Is the harness CLI installed / runnable on this machine?
    fn available(&self) -> bool;

    /// Can a turn of this harness be CORRECTED while it runs — i.e. does it read
    /// [`Turn::steer_file`] and deliver what it finds to the model mid-flight?
    ///
    /// Default false, and the daemon behaves the same either way: a mid-turn
    /// message is never dropped and never races the running turn. All this
    /// decides is WHEN it arrives — at the next tool-result boundary, or as the
    /// follow-up turn — which is the one thing the user is told, so nobody is
    /// promised a correction that is really a queue.
    fn can_steer(&self) -> bool {
        false
    }

    /// Run one turn, pushing normalized events into `sink` as they arrive.
    async fn run(
        &self,
        turn: Turn,
        sink: UnboundedSender<AgentEvent>,
    ) -> anyhow::Result<TurnOutcome>;

    /// Discover the harness's slash-commands / skills for the bot's `/` menu
    /// (a JSON array of `{command, description, arg_hint?}`).
    fn discover(&self, workdir: &str) -> Value;

    /// Try to handle a slash command locally (config dumps, /login, /stats…),
    /// or return `Forward` to run it as a prompt.
    /// `session` is this chat's live harness session id, when it has one — the
    /// only reliable way to report on it, since sibling chats can share a
    /// workdir and their transcripts race for newest-mtime.
    async fn command(
        &self,
        client: &Client,
        chat_id: &str,
        name: &str,
        arg: &str,
        workdir: &str,
        session: Option<&str>,
    ) -> CommandOutcome;

    /// One-line status (e.g. auth account) appended to `/status`. Empty = none.
    async fn status_line(&self) -> String {
        String::new()
    }

    /// Live model catalog for `/model`. Empty means the harness has no dynamic
    /// catalog and the daemon should keep its existing free-form model UX.
    async fn model_choices(&self) -> anyhow::Result<Vec<ModelChoice>> {
        Ok(Vec::new())
    }

    /// The harness CLI's own version string (e.g. Claude Code `2.1.198`, Codex
    /// `0.5.0`), shown on the `/status` Harness line. Empty = unknown / the CLI
    /// isn't installed. Each harness probes ITS binary — a codex bot must never
    /// report the `claude` version (and vice-versa).
    async fn cli_version(&self) -> String {
        String::new()
    }

    /// Probe the subscription seat behind this harness (see [`SeatHealth`]).
    ///
    /// Default is `unknown` — a harness that cannot ask its upstream "am I still
    /// allowed, and how full is my window" must say so rather than imply health.
    /// Implementations MUST be cheap and quota-free: this runs on every
    /// `reportHarnesses` heartbeat, so a probe that starts a session or burns
    /// tokens would measure the thing by consuming it.
    ///
    /// Implemented for Claude Code (a plain HTTP call to the same utilization
    /// endpoint Claude Code itself polls). **Codex is deliberately still
    /// `unknown`**: its equivalent data (`account/rateLimits/read`) is only
    /// reachable through the App Server, and `select()` hands out a fresh
    /// `Codex` per call, so its connection cache is always cold here — probing
    /// it would spawn a codex process on every heartbeat. Wiring that up needs
    /// process-lifetime harness instances, which is a separate change; until
    /// then `unknown` is the honest answer.
    async fn seat_health(&self) -> SeatHealth {
        SeatHealth::unknown()
    }
}

/// Every harness id the CLI knows about (installed or not) — for menus / docs.
/// (Used by the supervisor to list/validate harnesses — Phase 2.)
#[allow(dead_code)]
pub const KNOWN: &[&str] = &["claude-code", "opencode", "codex", "openclaw", "kimi-code"];

/// The default when a bot doesn't specify one.
#[allow(dead_code)]
pub const DEFAULT: &str = "claude-code";

/// Resolve a harness by id (case/alias tolerant). Unknown ids fall back to the
/// default so a misconfigured bot still runs Claude Code.
pub fn select(id: &str) -> Arc<dyn Harness> {
    match id.trim().to_lowercase().as_str() {
        "claude-code" | "claude" | "claudecode" | "" => Arc::new(claude_code::ClaudeCode),
        "codex" | "codex-cli" => Arc::new(codex::Codex),
        "kimi-code" | "kimi" | "kimi-cli" | "kimicode" => Arc::new(kimi_code::KimiCode),
        // opencode / openclaw plug in here as they're implemented.
        _ => Arc::new(claude_code::ClaudeCode),
    }
}

// (harness id, the CLI binary whose presence signals it's installed)
const BINS: &[(&str, &str)] = &[
    ("claude-code", "claude"),
    ("opencode", "opencode"),
    ("codex", "codex"),
    ("openclaw", "openclaw"),
    ("kimi-code", "kimi"),
];

/// Probe which known harnesses are installed on THIS machine (their CLI is on
/// PATH) — the control plane's capability report for New-Bot recommendation.
/// Returns `(id, available)`. Extend `BINS` as more harness impls land.
pub fn probe() -> Vec<(&'static str, bool)> {
    BINS.iter().map(|(id, bin)| (*id, on_path(bin))).collect()
}

/// `probe_with_versions` cache: version probes spawn subprocesses, so results
/// live ~10 min — the supervisor reports every 30s and must stay cheap.
#[allow(clippy::type_complexity)]
static VERSION_CACHE: std::sync::Mutex<
    Option<(
        std::time::Instant,
        std::collections::HashMap<&'static str, Option<String>>,
    )>,
> = std::sync::Mutex::new(None);

/// Like [`probe`], plus each available harness CLI's version (`<bin> --version`,
/// first numeric-ish token). Cached ~10 min; call [`invalidate_versions`] after
/// an install so the next report is fresh.
pub fn probe_with_versions() -> Vec<(&'static str, bool, Option<String>)> {
    let avail = probe();
    let mut guard = VERSION_CACHE.lock().unwrap();
    let fresh = guard
        .as_ref()
        .is_some_and(|(at, _)| at.elapsed() < std::time::Duration::from_secs(600));
    if !fresh {
        let mut m = std::collections::HashMap::new();
        for ((id, bin), (_, ok)) in BINS.iter().zip(&avail) {
            m.insert(*id, if *ok { bin_version(bin) } else { None });
        }
        *guard = Some((std::time::Instant::now(), m));
    }
    let versions = &guard.as_ref().unwrap().1;
    avail
        .into_iter()
        .map(|(id, ok)| {
            (
                id,
                ok,
                if ok {
                    versions.get(id).cloned().flatten()
                } else {
                    None
                },
            )
        })
        .collect()
}

/// The `reportHarnesses` rows for this machine — availability, CLI version and
/// seat health — built in ONE place so `mafold login` / `mafold report` and the
/// supervisor heartbeat can never drift apart (§0). Both call sites hand this
/// straight to the `harnesses` field.
///
/// Seat health is probed only for harnesses actually installed (asking a missing
/// CLI about its subscription is noise), and sequentially: only Claude Code does
/// I/O today, its probe is timeout-bounded at 5s, and a heartbeat has no latency
/// budget worth a join set for.
pub async fn report_rows() -> Vec<Value> {
    let probed = tokio::task::spawn_blocking(probe_with_versions)
        .await
        .unwrap_or_default();
    let mut rows = Vec::with_capacity(probed.len());
    for (id, available, version) in probed {
        // `null` (not `unknown`) when the harness isn't installed: there is no
        // seat to have an opinion about.
        //
        // `select()` deliberately falls back to Claude Code for ids it doesn't
        // implement, so asking it directly would attribute CLAUDE's seat to
        // `opencode`/`openclaw` — verified live: both rows came back "Max
        // (20x)" with Claude's windows. Ask the harness who it actually is
        // instead of keeping a second list of "ids that are implemented" (§9).
        let health = match select(id) {
            h if available && h.id() == id => Some(h.seat_health().await),
            _ => None,
        };
        rows.push(serde_json::json!({
            "id": id,
            "available": available,
            "version": version,
            "health": health,
        }));
    }
    rows
}

/// Drop the version cache (next `probe_with_versions` re-probes) — call after
/// installing a runtime so its availability + version report immediately.
pub fn invalidate_versions() {
    *VERSION_CACHE.lock().unwrap() = None;
}

/// `<bin> --version` → a short version string ("2.1.198"): first token that
/// starts with a digit, else the trimmed first line. None on any failure.
fn bin_version(bin: &str) -> Option<String> {
    let out = std::process::Command::new(program(bin))
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let line = s.lines().next()?.trim();
    let v = line
        .split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or(line);
    (!v.is_empty()).then(|| v.chars().take(32).collect())
}

/// Is `bin` resolvable on `$PATH`? Windows-aware: also tries each `PATHEXT`
/// extension (`codex.exe` / `claude.cmd` / …) — checking only the bare name made
/// `probe()` report every harness unavailable on Windows, so this machine never
/// showed up as a capable host.
pub(crate) fn on_path(bin: &str) -> bool {
    resolve(bin).is_some()
}

/// The program string to hand [`std::process::Command`] for `bin`: the resolved
/// `$PATH` entry when there is one, else the bare name (so the OS reports the
/// failure exactly as it used to).
///
/// EVERY spawn of a harness CLI goes through this, and `on_path` answers from the
/// same resolver — availability can no longer promise a spawn that then fails.
/// Windows is why: `CreateProcessW` only ever appends `.exe`, while a normal npm
/// install of these CLIs lays down a PAIR — `claude` (a POSIX sh script, which
/// Windows cannot start) plus `claude.cmd`. `on_path` saw the sh script and said
/// yes; `Command::new("claude")` looked for `claude.exe`, found nothing, and the
/// daemon answered the chat with "couldn't run `claude` … is it on PATH?" — the
/// one question it had already answered itself. Handing over the resolved
/// `…\claude.cmd` fixes both halves: std recognizes `.cmd`/`.bat` and routes them
/// through `cmd.exe` with hardened quoting (CVE-2024-24576).
pub(crate) fn program(bin: &str) -> std::ffi::OsString {
    resolve(bin).map_or_else(|| bin.into(), Into::into)
}

/// First `$PATH` entry for `bin` that we could actually execute.
pub(crate) fn resolve(bin: &str) -> Option<PathBuf> {
    // An explicit path (`/opt/bin/claude`, `C:\tools\claude.cmd`) is not a $PATH
    // lookup — take it as given.
    if bin.contains('/') || bin.contains('\\') {
        let p = PathBuf::from(bin);
        return launchable(&p).then_some(p);
    }
    let names = exe_candidates(bin);
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .flat_map(|dir| names.iter().map(move |n| dir.join(n)))
        .find(|p| launchable(p))
}

/// Could `Command` start this file? Unix wants the execute bit (matching what
/// `execvp` itself would accept while searching `$PATH`); on Windows the
/// candidate list is already restricted to launchable extensions.
#[cfg(not(windows))]
fn launchable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}
#[cfg(windows)]
fn launchable(p: &Path) -> bool {
    p.is_file()
}

/// Extensions `std::process::Command` can launch on Windows: the two it starts
/// directly and the two it wraps in `cmd.exe`. A `.ps1`/`.vbs`/`.js` shim needs
/// an interpreter, so it is NOT a candidate — counting it as "installed" is how
/// detection and spawn drifted apart in the first place.
#[cfg(windows)]
const LAUNCHABLE_EXTS: &[&str] = &[".com", ".exe", ".bat", ".cmd"];

/// Filenames to look for on `$PATH` for a program. On Unix that's just the bare
/// name; on Windows the executable is `codex.exe` / `claude.cmd` / …, so we try
/// the bare name with each `PATHEXT` extension appended.
#[cfg(not(windows))]
fn exe_candidates(bin: &str) -> Vec<String> {
    vec![bin.to_string()]
}
#[cfg(windows)]
fn exe_candidates(bin: &str) -> Vec<String> {
    // Deliberately NOT the bare name — see `program`. `PATHEXT` sets the order
    // (the shell's own precedence); the defaults are appended so a trimmed
    // `PATHEXT` can't hide an installed `claude.cmd`.
    let pathext = std::env::var("PATHEXT").unwrap_or_default();
    let mut names: Vec<String> = Vec::new();
    for ext in pathext.split(';').chain(LAUNCHABLE_EXTS.iter().copied()) {
        let ext = ext.trim().to_ascii_lowercase();
        if LAUNCHABLE_EXTS.contains(&ext.as_str()) {
            let name = format!("{bin}{ext}");
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// Why `bin` wouldn't start — one wording for every harness. Separates the two
/// failures the old message ran together: nothing installed at all, vs. a shim we
/// found and still couldn't launch.
///
/// The second half has to carry the CAUSE. `couldn't start \`claude\`
/// (C:\Users\…\claude.exe)` on its own is unfalsifiable — it names a file that
/// demonstrably exists (we just resolved it) and then says nothing about which
/// syscall refused. The `io::Error` holding that answer stays attached as the
/// anyhow source, and the codes a spawn of an already-resolved file can fail
/// with get spelled out here, because "os error 193" is not something the person
/// reading the chat can act on.
pub(crate) fn spawn_err(bin: &str, workdir: &str, e: std::io::Error) -> anyhow::Error {
    let head = match resolve(bin) {
        Some(p) => format!("couldn't start `{bin}` ({}) in {workdir}", p.display()),
        None => format!(
            "`{bin}` is not on PATH — is it installed? (looked for: {})",
            exe_candidates(bin).join(", ")
        ),
    };
    match spawn_cause(&e) {
        Some(why) => anyhow::Error::new(e).context(format!("{head} — {why}")),
        None => anyhow::Error::new(e).context(head),
    }
}

/// Plain language for the handful of OS errors that can reject a program we
/// already found on `$PATH`. Raw codes are per-OS, so each set is gated; the
/// kinds std normalizes are matched first.
fn spawn_cause(e: &std::io::Error) -> Option<&'static str> {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        return Some("access denied — antivirus or file permissions are blocking it");
    }
    #[cfg(windows)]
    {
        // CreateProcessW's own codes (winerror.h).
        return match e.raw_os_error() {
            Some(2) => Some("it disappeared between the PATH lookup and the spawn"),
            Some(193) => Some(
                "not a valid Windows program — a truncated download or the wrong architecture; \
                 reinstall Claude Code",
            ),
            Some(206) => Some(
                "the command line is past the Windows 32,767-character limit — the prompt plus \
                 system prompt is too long for argv",
            ),
            Some(267) => Some("the working directory isn't usable"),
            Some(1455) => Some("Windows is out of commit charge (paging file too small)"),
            _ => None,
        };
    }
    #[allow(unreachable_code)]
    {
        // ENOEXEC / E2BIG — the two Unix equivalents worth naming.
        match e.raw_os_error() {
            Some(8) => Some("not an executable format — a script with no `#!` line?"),
            Some(7) => Some("the argument list is too long for exec"),
            _ => None,
        }
    }
}

#[cfg(test)]
mod path_resolution_tests {
    use super::*;

    /// The invariant the Windows daemon broke: whatever `on_path` calls installed,
    /// `program` must hand back something spawnable — never the bare name that
    /// `CreateProcessW` then fails to find.
    #[test]
    fn availability_implies_a_resolved_program() {
        for (_, bin) in BINS {
            if on_path(bin) {
                assert_ne!(
                    program(bin),
                    std::ffi::OsString::from(*bin),
                    "{bin} reported available but resolved to the bare name"
                );
                assert!(resolve(bin).is_some_and(|p| p.is_absolute() || p.exists()));
            }
        }
    }

    /// A spawn failure has to say WHY, and it has to survive the trip to chat.
    /// The daemon renders with `{:#}`, so the `io::Error` we keep as the source —
    /// the only part that names the syscall's refusal — must show up there;
    /// plain `{}` shows just the context line, which is the dead end two Windows
    /// rounds were spent staring at.
    #[test]
    fn spawn_error_carries_the_os_cause() {
        // EACCES / ERROR_ACCESS_DENIED — both normalize to PermissionDenied.
        let raw = if cfg!(windows) { 5 } else { 13 };
        let e = spawn_err("claude", "/tmp/wd", std::io::Error::from_raw_os_error(raw));
        let full = format!("{e:#}");
        assert!(full.contains("access denied"), "no plain-language cause: {full}");
        assert!(full.contains("os error"), "OS code dropped: {full}");
        assert!(!format!("{e}").contains("os error"), "test is asserting nothing");
    }

    /// A non-executable file named like the CLI is not an install — `execvp`
    /// skips it while searching `$PATH` and so must we. (Tests `launchable`
    /// directly rather than mutating the process `PATH`, which would race the
    /// other tests in this binary.)
    #[test]
    fn non_executable_is_not_an_install() {
        let dir = std::env::temp_dir().join(format!("mafold-resolve-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let decoy = dir.join(if cfg!(windows) { "mafoldtest.exe" } else { "mafoldtest" });
        std::fs::write(&decoy, "not a program").unwrap();
        #[cfg(not(windows))]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(!launchable(&decoy));
            std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        // Windows has no execute bit — a launchable extension IS the install there.
        assert!(launchable(&decoy));
        assert!(!launchable(&dir), "a directory is not a program");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An explicit path bypasses the `$PATH` walk instead of being searched for
    /// as a filename (`resolve` is also the door for a configured absolute CLI).
    #[test]
    fn explicit_path_is_taken_as_given() {
        assert!(resolve("/definitely/not/here/claude").is_none());
    }

    /// On Windows the extensionless npm shim (a POSIX sh script) is not a
    /// candidate, and every candidate is something `Command` can launch.
    #[cfg(windows)]
    #[test]
    fn windows_candidates_are_launchable_only() {
        let names = exe_candidates("claude");
        assert!(!names.contains(&"claude".to_string()));
        assert!(names.contains(&"claude.cmd".to_string()));
        assert!(names.iter().all(|n| LAUNCHABLE_EXTS
            .iter()
            .any(|e| n.ends_with(e))));
    }
}
