//! Claude accounts on this machine — several logins behind ONE `~/.claude`.
//!
//! Claude Code keys its OAuth credential by a single directory: with
//! `CLAUDE_SECURESTORAGE_CONFIG_DIR=<dir>` in a process's environment it reads
//! and writes the login that belongs to `<dir>` — a macOS Keychain item named
//! `Claude Code-credentials-<sha256(dir)[..8]>`, `<dir>/.credentials.json`
//! elsewhere — and touches nothing else. Skills, settings, plugins,
//! `~/.claude.json`, the session transcripts under `~/.claude/projects/` and
//! the auto-memory that lives beside them all stay `~/.claude`'s. That is
//! exactly the split a bot wants: N subscriptions behind one identity, one
//! memory, one set of resumable sessions. (`CLAUDE_CONFIG_DIR` is the OTHER
//! switch, and the wrong one — it isolates all of that too, so a bot would
//! forget everything the moment it changed accounts.)
//!
//! One account = one directory under `~/.mafold/claude-accounts/<name>/`;
//! `default` is the login `~/.claude` already holds and carries no env at all.
//! The registry (`~/.mafold/claude-accounts.json`) is machine-level: a usage
//! limit belongs to the account, not to the daemon that ran into it, so every
//! daemon on the box reads the same file and writes it atomically (rename;
//! last writer wins — the most a lost write costs is one extra attempt on a
//! window somebody else already found full).
//!
//! Selection is sticky, not round-robin: a turn runs on the account it was
//! asked for (`/account` › the Customize sheet › `default`) until that seat's
//! window is full, then on the first other seat that is not, and it comes back
//! by itself once the window rolls over. Windows are remembered by their reset
//! time (`exhausted`), which is what makes "come back by itself" free.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// The one environment variable this module exists to set.
pub const ENV: &str = "CLAUDE_SECURESTORAGE_CONFIG_DIR";
/// The login `~/.claude` already holds — no env, no directory of its own.
pub const DEFAULT: &str = "default";

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into()))
}

/// `~/.mafold/claude-accounts.json`.
pub fn registry_path() -> PathBuf {
    home().join(".mafold/claude-accounts.json")
}

/// `~/.mafold/claude-accounts/<name>` — a named account's credential directory.
/// Only its path matters to Claude Code (the Keychain item is named after it);
/// on Linux/Windows the credential file lives inside.
pub fn account_dir(name: &str) -> PathBuf {
    home().join(".mafold/claude-accounts").join(name)
}

/// Epoch seconds now.
pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// One login. `dir: None` is the default account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    /// The credential directory (`CLAUDE_SECURESTORAGE_CONFIG_DIR`). None =
    /// `~/.claude`'s own login, which needs no env.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
    /// The email the login reported (`/api/oauth/profile`). Display only —
    /// the credential itself never leaves Claude Code's storage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Epoch seconds the account was added (0 for `default`).
    #[serde(default)]
    pub added_at: i64,
}

impl Account {
    /// The login `~/.claude` holds.
    pub fn default_login() -> Self {
        Account { name: DEFAULT.into(), dir: None, email: None, added_at: 0 }
    }

    pub fn is_default(&self) -> bool {
        self.dir.is_none()
    }

    /// The process environment that makes `claude` use THIS login. Empty for
    /// the default account — the daemon's own env already is that login.
    pub fn env(&self) -> Vec<(String, String)> {
        match &self.dir {
            Some(d) => vec![(ENV.to_string(), d.clone())],
            None => Vec::new(),
        }
    }

    /// The macOS Keychain service Claude Code files this account's credential
    /// under: the bare name for the default login, `-<8 hex>` of the
    /// directory's sha256 for every other one.
    pub fn keychain_service(&self) -> String {
        match &self.dir {
            Some(d) => format!("Claude Code-credentials-{}", hash8(d)),
            None => "Claude Code-credentials".to_string(),
        }
    }

    /// Where the credential file lives when it is a file (Linux/Windows, and
    /// the fallback Claude Code keeps everywhere).
    pub fn credentials_file(&self) -> PathBuf {
        match &self.dir {
            Some(d) => Path::new(d).join(".credentials.json"),
            None => home().join(".claude/.credentials.json"),
        }
    }

    /// The account a process env describes — the inverse of [`Account::env`],
    /// for code that is handed a turn's env rather than an account. A
    /// directory the registry doesn't know (someone set the variable by hand)
    /// still resolves, named after its last path component.
    pub fn from_env(env: &[(String, String)]) -> Account {
        let dir = env
            .iter()
            .find(|(k, _)| k == ENV)
            .map(|(_, v)| v.to_string())
            // A daemon pinned to one login by its OWN environment (the
            // supervisor's per-daemon `env` map) is "default" to its turns —
            // an empty turn env inherits, and what it inherits is that
            // directory — so the credential lookups must follow it too.
            .or_else(|| std::env::var(ENV).ok())
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let Some(dir) = dir else { return Self::default_login() };
        let reg = load();
        reg.accounts
            .iter()
            .find(|a| a.dir.as_deref() == Some(dir.as_str()))
            .cloned()
            .unwrap_or_else(|| Account {
                name: Path::new(&dir)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "?".into()),
                dir: Some(dir),
                email: None,
                added_at: 0,
            })
    }
}

/// First 8 hex chars of `sha256(dir)` — Claude Code's suffix for the Keychain
/// item of a non-default storage directory. Verbatim from the 2.1.260 binary:
/// `sha256(process.env.CLAUDE_SECURESTORAGE_CONFIG_DIR.normalize("NFC"))
/// .digest("hex").substring(0, 8)`, with an unset OR EMPTY variable meaning
/// the bare (default) item. Hashed exactly as handed over in the env — not
/// resolved, not canonicalized — so the daemon must always pass the same
/// string it hashes (the registry stores the absolute path for that reason).
pub fn hash8(dir: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(dir.as_bytes())
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// An account name: lowercase, digits, `-` `_` `.`, starts alphanumeric, ≤32.
/// It becomes a directory name, an env value and a Customize option — one
/// shape that is safe in all three.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 32
        && s.chars().next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

/// A window that was found full, remembered until it rolls over.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exhausted {
    /// Epoch seconds the window resets — the account is skipped until then.
    pub until: i64,
    /// The window, as Claude names it (`five_hour`, `seven_day`, …).
    pub kind: String,
    /// When the wall was hit.
    pub at: i64,
    /// The model this hold applies to, when the window was scoped to one
    /// (see [`Wall::scope`]). None = the whole account. Absent in files
    /// written by an earlier build, which read back as account-wide — the
    /// safe direction: it can cost one avoidable switch, never a wrong claim
    /// that a full window is fine.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl Exhausted {
    /// Does this hold stop a turn running `model`? Same rule as [`Wall::stops`].
    pub fn stops(&self, model: Option<&str>) -> bool {
        let Some(scope) = &self.scope else { return true };
        model.is_some_and(|m| model_matches(m, scope))
    }
}

/// `~/.mafold/claude-accounts.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Registry {
    /// In selection order; `default` is always first.
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// account name → the window that is full right now.
    #[serde(default)]
    pub exhausted: BTreeMap<String, Exhausted>,
}

impl Registry {
    /// `default` first and exactly once; every other row needs a directory
    /// (a row without one is a hand edit gone wrong, not an account).
    fn normalize(&mut self) {
        let mut seen: HashSet<String> = HashSet::new();
        let mut out = Vec::with_capacity(self.accounts.len() + 1);
        out.push(Account::default_login());
        seen.insert(DEFAULT.to_string());
        for a in self.accounts.drain(..) {
            if a.dir.is_none() || !valid_name(&a.name) {
                continue;
            }
            if seen.insert(a.name.clone()) {
                out.push(a);
            }
        }
        self.accounts = out;
    }

    pub fn get(&self, name: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.name == name)
    }

    /// Register `name` (idempotent), creating its credential directory. The
    /// login itself is Claude Code's job (`/login <name>`); this only gives it
    /// a place to keep the result.
    pub fn add(&mut self, name: &str) -> Result<Account, String> {
        let name = name.trim().to_ascii_lowercase();
        if !valid_name(&name) {
            return Err(format!(
                "`{name}` isn't a usable account name — lowercase letters, digits, `-` `_` `.`, up to 32 characters"
            ));
        }
        if let Some(a) = self.get(&name) {
            return Ok(a.clone());
        }
        let dir = account_dir(&name);
        std::fs::create_dir_all(&dir).map_err(|e| format!("couldn't create {}: {e}", dir.display()))?;
        let a = Account {
            name: name.clone(),
            dir: Some(dir.to_string_lossy().into_owned()),
            email: None,
            added_at: now(),
        };
        self.accounts.push(a.clone());
        Ok(a)
    }

    /// Forget `name` (never `default`). The credential directory stays — a
    /// `/logout` clears the login; this only stops offering the seat.
    pub fn remove(&mut self, name: &str) -> bool {
        if name == DEFAULT {
            return false;
        }
        let n = self.accounts.len();
        self.accounts.retain(|a| a.name != name);
        self.exhausted.remove(name);
        self.accounts.len() != n
    }

    pub fn mark_exhausted(&mut self, name: &str, kind: &str, until: i64, scope: Option<String>) {
        self.exhausted
            .insert(name.to_string(), Exhausted { until, kind: kind.to_string(), at: now(), scope });
    }

    pub fn clear_exhausted(&mut self, name: &str) -> bool {
        self.exhausted.remove(name).is_some()
    }

    /// The window that is still full at `now`, if any.
    pub fn exhausted_at(&self, name: &str, now: i64) -> Option<&Exhausted> {
        self.exhausted.get(name).filter(|x| x.until > now)
    }

    /// Drop marks whose window has rolled over. True when anything changed.
    pub fn prune(&mut self, now: i64) -> bool {
        let n = self.exhausted.len();
        self.exhausted.retain(|_, x| x.until > now);
        self.exhausted.len() != n
    }

    /// Candidate order for a turn: the preferred account first (when this
    /// machine has it), then everyone else in registry order.
    pub fn ordered(&self, preferred: Option<&str>) -> Vec<Account> {
        let mut out: Vec<Account> = Vec::with_capacity(self.accounts.len());
        if let Some(p) = preferred.and_then(|p| self.get(p)) {
            out.push(p.clone());
        }
        for a in &self.accounts {
            if !out.iter().any(|x| x.name == a.name) {
                out.push(a.clone());
            }
        }
        out
    }
}

pub fn load() -> Registry {
    load_from(&registry_path())
}

pub fn load_from(path: &Path) -> Registry {
    let mut r: Registry = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    r.normalize();
    r
}

pub fn save(reg: &Registry) -> std::io::Result<()> {
    save_to(&registry_path(), reg)
}

/// Atomic: a per-process scratch file renamed over the real one, so two
/// daemons writing at once can't interleave and a crash can't leave a torn file.
pub fn save_to(path: &Path, reg: &Registry) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_string_pretty(reg)?)?;
    std::fs::rename(&tmp, path)
}

/// "resets in 2h 05m", or "resets soon" once the moment has passed.
pub fn reset_hint(until: i64, now: i64) -> String {
    if until > now {
        format!("resets in {}", crate::commands::fmt_dur(until - now))
    } else {
        "resets soon".to_string()
    }
}

/// A run's error text says the seat's usage limit was hit — `Some(the reset
/// epoch, when the text carried one)`. Claude Code prints
/// `Claude usage limit reached|<epoch>` as its whole reason on stdout, or ends
/// the turn with `You've hit your session limit · resets 6:30pm (…)` (seen
/// 2026-09-06, no epoch); other phrasings carry no timestamp and come back as
/// `Some(None)`.
pub fn usage_limit_reset(text: &str) -> Option<Option<i64>> {
    let lower = text.to_ascii_lowercase();
    let is_limit = lower.contains("usage limit")
        || lower.contains("rate limit")
        || lower.contains("rate_limit")
        || (lower.contains("hit your") && lower.contains("limit"));
    if !is_limit {
        return None;
    }
    let epoch = text
        .split('|')
        .nth(1)
        .map(str::trim)
        .and_then(|s| s.split(|c: char| !c.is_ascii_digit()).next())
        .and_then(|d| d.parse::<i64>().ok())
        .filter(|t| *t > 1_000_000_000);
    Some(epoch)
}

// ───────────────────────── seat state (live) ─────────────────────────

/// What a seat can be right now, as far as a quota-free probe can tell.
#[derive(Debug, Clone, PartialEq)]
pub enum SeatState {
    Ok,
    /// An ACCOUNT-WIDE window is full and nothing (extra usage) is covering
    /// the overflow. A window that is full for one MODEL only is not this —
    /// see [`Wall::scope`].
    Exhausted { kind: String, until: i64 },
    /// No readable credential, or upstream said 401 — `/login <name>` fixes it.
    NoCredential,
    /// Upstream said 403 — re-logging-in does not fix it.
    Rejected(u16),
    /// Couldn't tell (network, an unexpected status): let the turn try.
    Unknown,
}

/// A usage window found full.
#[derive(Debug, Clone, PartialEq)]
pub struct Wall {
    /// Claude's own name for the window (`session`, `weekly_all`,
    /// `weekly_scoped`, `seven_day_opus`, …).
    pub kind: String,
    /// Epoch seconds it rolls over.
    pub until: i64,
    /// The MODEL this wall applies to, lowercased, when the window is scoped
    /// to one (`weekly_scoped` carries `scope.model.display_name`; a kind
    /// like `seven_day_opus` names it in the kind). None = the whole account
    /// is out, whatever it is asked to run.
    ///
    /// This distinction is load-bearing, not cosmetic: on 2026-09-06 this
    /// machine's account sat at `weekly_scoped` 100% (Fable) with `session`
    /// at 29% and `weekly_all` at 54%. Treating that as an exhausted ACCOUNT
    /// would hand every turn — Sonnet, Opus, Haiku, all fine — to another
    /// login for five days.
    pub scope: Option<String>,
}

impl Wall {
    /// Does this wall stop a turn running `model`? An account-wide wall stops
    /// everything. A scoped one stops only its own model — and when the turn
    /// has no model pinned (the harness's own default, which we cannot know
    /// from here) it does NOT stop it: guessing wrong would hand away a
    /// healthy seat, while letting the turn try costs at most one refusal,
    /// which the runtime failover already handles.
    pub fn stops(&self, model: Option<&str>) -> bool {
        let Some(scope) = &self.scope else { return true };
        model.is_some_and(|m| model_matches(m, scope))
    }
}

/// Is `model` (a `/model` value or Customize选项 — "fable", "opus",
/// "claude-fable-5-1", …) the model a window is scoped to ("fable")? Compared
/// on the scope's name appearing in the model id, both lowercased, because
/// the two come from different vocabularies: the scope is a display name and
/// the model is whatever the user or the sheet wrote.
fn model_matches(model: &str, scope: &str) -> bool {
    let (m, s) = (model.to_ascii_lowercase(), scope.to_ascii_lowercase());
    !s.is_empty() && (m.contains(&s) || s.contains(&m))
}

/// The model a window's own name scopes it to, for the kinds that carry it
/// there instead of in `scope` (`seven_day_opus` → `opus`). None for the
/// account-wide kinds and for `weekly_scoped`, whose model rides in `scope`.
fn model_in_kind(kind: &str) -> Option<String> {
    const KNOWN: [&str; 5] = ["opus", "sonnet", "haiku", "fable", "omelette"];
    let k = kind.to_ascii_lowercase();
    KNOWN.iter().find(|m| k.ends_with(&format!("_{m}"))).map(|m| (*m).to_string())
}

#[derive(Debug, Clone)]
pub struct SeatSnapshot {
    pub state: SeatState,
    /// The fullest window — (Claude's kind, percent, resets_at) — for listings.
    pub worst: Option<(String, f64, Option<i64>)>,
    /// Every window found full, account-wide ones first (so the first one
    /// that stops a turn is also the most useful reason to report).
    pub walls: Vec<Wall>,
}

impl SeatSnapshot {
    fn bare(state: SeatState) -> Self {
        SeatSnapshot { state, worst: None, walls: Vec::new() }
    }

    /// The wall that stops a turn running `model`, if any.
    pub fn blocks(&self, model: Option<&str>) -> Option<&Wall> {
        self.walls.iter().find(|w| w.stops(model))
    }

    /// One phrase for a listing: "ok · session 90%", "exhausted (weekly_all)
    /// — resets in 3h", "ok · Fable out (resets in 5d) · session 29%".
    pub fn describe(&self, now: i64) -> String {
        match &self.state {
            SeatState::Ok => {
                let mut out = String::new();
                // A model that is out is the first thing to say even though
                // the seat is "ok" — it is the difference between "this login
                // can take my turn" and "…but not the model I asked for".
                for w in self.walls.iter().filter(|w| w.scope.is_some()) {
                    let m = w.scope.as_deref().unwrap_or("scoped");
                    out.push_str(&format!("{m} out ({}) · ", reset_hint(w.until, now)));
                }
                match &self.worst {
                    Some((kind, pct, _)) => format!("ok · {out}{} {}%", kind.replace('_', " "), pct.round() as i64),
                    None if out.is_empty() => "ok".into(),
                    None => format!("ok · {}", out.trim_end_matches(" · ")),
                }
            }
            SeatState::Exhausted { kind, until } => format!("exhausted ({kind}) — {}", reset_hint(*until, now)),
            SeatState::NoCredential => "not logged in".into(),
            SeatState::Rejected(s) => format!("rejected upstream ({s})"),
            SeatState::Unknown => "unknown (couldn't reach the usage endpoint)".into(),
        }
    }
}

/// Read Claude's `/api/oauth/usage` payload as a seat verdict.
///
/// A window at 100% is only a wall when extra usage is not paying for the
/// overflow — with it enabled and its own spend limit not reached, requests
/// still go through. And a full window that is scoped to ONE MODEL leaves the
/// seat usable for every other one, so it is recorded as a scoped [`Wall`]
/// rather than as an exhausted account.
///
/// Payload shape verified live 2026-09-06: `limits[]` of
/// `{kind, group, percent, severity, resets_at (ISO), scope, is_active}`
/// where `scope` is `null` or `{"model":{"display_name":"Fable"}}`, plus
/// `extra_usage.{is_enabled, spend_limit_reached}`.
pub fn snapshot_from_usage(v: &Value, now: i64) -> SeatSnapshot {
    let extra = &v["extra_usage"];
    let overage_covers = extra["is_enabled"].as_bool() == Some(true)
        && extra["spend_limit_reached"].as_bool() != Some(true);
    let mut worst: Option<(String, f64, Option<i64>)> = None;
    let mut walls: Vec<Wall> = Vec::new();
    for l in v["limits"].as_array().into_iter().flatten() {
        let Some(pct) = l["percent"].as_f64() else { continue };
        let kind = l["kind"].as_str().unwrap_or("usage").to_string();
        let resets = l["resets_at"].as_str().and_then(crate::commands::iso_epoch_secs);
        if worst.as_ref().is_none_or(|w| pct > w.1) {
            worst = Some((kind.clone(), pct, resets));
        }
        if pct >= 100.0 && !overage_covers {
            let scope = l["scope"]["model"]["display_name"]
                .as_str()
                .map(|s| s.to_ascii_lowercase())
                .or_else(|| model_in_kind(&kind));
            // A reset time already past (clock skew, a cached payload) must
            // not pin the seat forever — hold it a bounded hour instead.
            let until = resets.filter(|t| *t > now).unwrap_or(now + 3600);
            walls.push(Wall { kind, until, scope });
        }
    }
    // Account-wide first: it is both the stronger verdict and the better
    // explanation when several windows are full at once.
    walls.sort_by_key(|w| w.scope.is_some());
    let state = match walls.first() {
        Some(w) if w.scope.is_none() => SeatState::Exhausted { kind: w.kind.clone(), until: w.until },
        _ => SeatState::Ok,
    };
    SeatSnapshot { state, worst, walls }
}

/// Probe results live a minute: a turn start must not cost a round trip every
/// time, and a window does not empty between two messages anyway. Keyed by
/// account name; process-local (each daemon keeps its own).
const FRESH: std::time::Duration = std::time::Duration::from_secs(60);

fn cache() -> &'static Mutex<HashMap<String, (Instant, SeatSnapshot)>> {
    static C: OnceLock<Mutex<HashMap<String, (Instant, SeatSnapshot)>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Forget what we knew about a seat — after a login, a logout, or a wall.
pub fn forget_seat(name: &str) {
    cache().lock().unwrap().remove(name);
}

/// Put a probe result into the cache as if it had just been measured — the
/// seam the chooser's tests drive it through, so they exercise the real
/// `check_seat` → `first_usable` path instead of a parallel fake of it.
#[cfg(test)]
fn seed_seat(name: &str, snap: SeatSnapshot) {
    cache().lock().unwrap().insert(name.to_string(), (Instant::now(), snap));
}

/// Is this seat usable right now? Cached (see [`FRESH`]); the probe is the
/// same quota-free usage call Claude Code itself makes.
pub async fn check_seat(acct: &Account) -> SeatSnapshot {
    if let Some((at, snap)) = cache().lock().unwrap().get(&acct.name).cloned() {
        if at.elapsed() < FRESH {
            return snap;
        }
    }
    let snap = probe_seat(acct).await;
    cache().lock().unwrap().insert(acct.name.clone(), (Instant::now(), snap.clone()));
    snap
}

async fn probe_seat(acct: &Account) -> SeatSnapshot {
    use crate::commands::UtilizationProbe as P;
    match crate::commands::probe_utilization(&acct.env()).await {
        P::Ok(v) => snapshot_from_usage(&v, now()),
        P::NoCredential | P::Http(401) => SeatSnapshot::bare(SeatState::NoCredential),
        P::Http(403) => SeatSnapshot::bare(SeatState::Rejected(403)),
        P::Http(_) | P::Unreachable => SeatSnapshot::bare(SeatState::Unknown),
    }
}

/// Every account with its live state, probed side by side (for `/account`).
pub async fn list_states() -> Vec<(Account, SeatSnapshot, Option<Exhausted>)> {
    let reg = load();
    let now = now();
    let probes = futures_util::future::join_all(reg.accounts.iter().map(check_seat)).await;
    reg.accounts
        .iter()
        .zip(probes)
        .map(|(a, s)| (a.clone(), s, reg.exhausted_at(&a.name, now).cloned()))
        .collect()
}

// ───────────────────────── choosing a seat ─────────────────────────

/// The seat a turn runs on, and why it is not the one asked for (when it isn't).
#[derive(Debug, Clone)]
pub struct Choice {
    pub account: Account,
    /// The account the turn was asked to run on (`/account`, the sheet, or `default`).
    pub preferred: String,
    /// (name, why) for every seat passed over on the way to `account`.
    pub skipped: Vec<(String, String)>,
}

impl Choice {
    pub fn switched(&self) -> bool {
        self.account.name != self.preferred
    }

    /// One line for the reply when the turn is not on the account it was
    /// asked for — the user should never have to guess whose window is full.
    pub fn note(&self) -> Option<String> {
        if !self.switched() {
            return None;
        }
        let why = self
            .skipped
            .iter()
            .map(|(n, w)| format!("`{n}` {w}"))
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!("↻ Using account `{}` — {why}", self.account.name))
    }
}

/// Walk `order` and take the first seat that can run a turn of `model`.
///
/// Every seat is probed (cached a minute), a remembered wall included: the
/// probe is the truth about the window, the mark is only what the last turn
/// saw — so a mark the probe contradicts is cleared rather than obeyed (a
/// transient refusal must not lock a seat out for the length of its hold).
/// Only when the probe can't tell does the mark decide. True in the second
/// slot = the registry changed and wants saving.
async fn first_usable(
    reg: &mut Registry,
    order: &[Account],
    now: i64,
    model: Option<&str>,
    skipped: &mut Vec<(String, String)>,
) -> (Option<Account>, bool) {
    let mut dirty = false;
    for acct in order {
        let marked = reg.exhausted_at(&acct.name, now).cloned();
        let snap = check_seat(acct).await;
        match &snap.state {
            SeatState::NoCredential => {
                skipped.push((acct.name.clone(), "isn't logged in — `/login <name>` fixes that".into()));
                continue;
            }
            SeatState::Rejected(s) => {
                skipped.push((acct.name.clone(), format!("was rejected upstream ({s})")));
                continue;
            }
            // The probe reached the account: whatever it says now supersedes
            // what the last turn remembered.
            SeatState::Ok | SeatState::Exhausted { .. } => {
                // What gets REMEMBERED is what the probe saw about the seat —
                // not what blocks this particular turn. Storing the
                // model-specific answer would make one Sonnet turn erase a
                // Fable hold, and a Fable turn write it back, on repeat.
                let seen = snap.walls.first();
                let want = seen.map(|w| (w.kind.clone(), w.until, w.scope.clone()));
                let have = marked.as_ref().map(|x| (x.kind.clone(), x.until, x.scope.clone()));
                if want != have {
                    match seen {
                        Some(w) => reg.mark_exhausted(&acct.name, &w.kind, w.until, w.scope.clone()),
                        None => {
                            reg.clear_exhausted(&acct.name);
                        }
                    }
                    dirty = true;
                }
                // …and what DECIDES is the wall that stops THIS turn.
                match snap.blocks(model).cloned() {
                    None => return (Some(acct.clone()), dirty),
                    Some(w) => {
                        let what = match &w.scope {
                            Some(m) => format!("is out of {m}"),
                            None => format!("is exhausted ({})", w.kind),
                        };
                        skipped.push((acct.name.clone(), format!("{what} — {}", reset_hint(w.until, now))));
                    }
                }
            }
            // No answer from upstream: the remembered wall is all we have.
            SeatState::Unknown => match marked.filter(|x| x.stops(model)) {
                Some(x) => skipped.push((
                    acct.name.clone(),
                    format!("is exhausted ({}) — {}", x.kind, reset_hint(x.until, now)),
                )),
                None => return (Some(acct.clone()), dirty),
            },
        }
    }
    (None, dirty)
}

/// Pick the seat for a turn: the preferred account unless it can't run this
/// turn (a window found full by a fresh probe, or remembered and
/// unverifiable), else the first other seat that can. `model` is the model
/// the turn will ask for — a window scoped to a DIFFERENT model is not a wall
/// for it. When every seat is out, the preferred one runs anyway and the turn
/// itself reports the wall — a silent refusal is worse than a clear one.
pub async fn choose(preferred: Option<&str>, model: Option<&str>) -> Choice {
    let mut reg = load();
    let now = now();
    let mut dirty = reg.prune(now);
    let preferred_name = preferred.map(str::to_string).unwrap_or_else(|| DEFAULT.into());
    let mut skipped: Vec<(String, String)> = Vec::new();
    if preferred.is_some() && reg.get(&preferred_name).is_none() {
        skipped.push((preferred_name.clone(), "isn't on this machine — `/login <name>` adds it".into()));
    }
    let order = reg.ordered(preferred);
    // One login and nowhere else to go: nothing to choose, so nothing to
    // probe — a single-account daemon pays no round trip per turn.
    let pick = if reg.accounts.len() <= 1 {
        order.first().cloned()
    } else {
        let (p, d) = first_usable(&mut reg, &order, now, model, &mut skipped).await;
        dirty |= d;
        p
    };
    if dirty {
        let _ = save(&reg);
    }
    let account = pick.unwrap_or_else(|| order.first().cloned().unwrap_or_else(Account::default_login));
    Choice { account, preferred: preferred_name, skipped }
}

/// The seat behind a running turn just hit its wall: remember it until the
/// window resets and hand back the next seat that can take the turn over —
/// None when there is none, in which case the turn reports the wall. The
/// second slot says why each other seat was passed over.
///
/// The refusal names its window but not its model, so the scope is read out
/// of the window's own name (`seven_day_opus` → `opus`): a per-model wall
/// must not hold the whole account for a week.
pub async fn failover(
    current: &str,
    kind: &str,
    resets_at: Option<i64>,
    model: Option<&str>,
) -> (Option<Account>, Vec<(String, String)>) {
    let now = now();
    let until = resets_at.filter(|t| *t > now).unwrap_or(now + 3600);
    let mut reg = load();
    reg.mark_exhausted(current, kind, until, model_in_kind(kind));
    forget_seat(current);
    let order: Vec<Account> = reg.ordered(None).into_iter().filter(|a| a.name != current).collect();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let (pick, _) = first_usable(&mut reg, &order, now, model, &mut skipped).await;
    let _ = save(&reg);
    (pick, skipped)
}

/// Remember the email a login reported (`claude auth status --json`), for
/// listings. Display only; no-op for a name the registry doesn't hold.
pub fn set_email(name: &str, email: Option<String>) {
    let mut reg = load();
    if let Some(a) = reg.accounts.iter_mut().find(|a| a.name == name) {
        if a.email != email {
            a.email = email;
            let _ = save(&reg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry's seats, in selection order.
    fn names(r: &Registry) -> Vec<String> {
        r.accounts.iter().map(|a| a.name.clone()).collect()
    }

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("mafold-accounts-{}-{name}.json", std::process::id()))
    }

    /// The suffix formula Claude Code names its Keychain item by, pinned to a
    /// pair OBSERVED on a real machine rather than to our own arithmetic:
    /// `/tmp/cc-acct-probe` was handed to Claude Code 2.1.x as
    /// `CLAUDE_SECURESTORAGE_CONFIG_DIR` on 2026-09-02, and the item it wrote
    /// into the login Keychain was named `Claude Code-credentials-94718db4`.
    /// (Recovered the other way round — by searching for the directory whose
    /// digest produced that suffix — which is what makes it evidence and not
    /// a restatement of the code below.)
    ///
    /// Get this wrong and nothing fails loudly: the daemon simply reads the
    /// WRONG account's credential, or none at all, and reports a healthy seat
    /// as logged out.
    #[test]
    fn keychain_suffix_is_the_first_eight_hex_of_sha256() {
        assert_eq!(hash8("/tmp/cc-acct-probe"), "94718db4");
        let a = Account { name: "probe".into(), dir: Some("/tmp/cc-acct-probe".into()), email: None, added_at: 0 };
        assert_eq!(a.keychain_service(), "Claude Code-credentials-94718db4");
        // No directory ⇒ the BARE item: Claude Code appends the suffix only
        // when the variable is set and non-empty.
        assert_eq!(Account::default_login().keychain_service(), "Claude Code-credentials");
    }

    /// The whole premise: a seat moves the CREDENTIAL and nothing else. The
    /// credential file/Keychain item is per-account, while the transcripts,
    /// the auto-memory beside them, skills and settings all stay in
    /// `~/.claude` — which is why `--resume` carries a conversation across an
    /// account switch. (Verified live on 2026-09-06: `claude auth status
    /// --json` under a non-default seat reports `loggedIn:false` for that
    /// seat while still reporting `projectsDirectory: ~/.claude/projects`.)
    #[test]
    fn a_seat_moves_the_credential_and_nothing_else() {
        let a = Account { name: "work".into(), dir: Some("/tmp/seat".into()), email: None, added_at: 0 };
        assert_eq!(a.credentials_file(), Path::new("/tmp/seat/.credentials.json"));
        assert_eq!(a.env(), vec![(ENV.to_string(), "/tmp/seat".to_string())]);
        // The only variable a seat sets. `CLAUDE_CONFIG_DIR` would isolate
        // sessions, memory and skills too — the one thing this must never do.
        assert_eq!(a.env().len(), 1);
        assert!(!a.env().iter().any(|(k, _)| k == "CLAUDE_CONFIG_DIR"));
        assert!(Account::default_login().credentials_file().ends_with(".claude/.credentials.json"));
    }

    /// The default account carries no env: Claude Code reads an unset (or
    /// empty) variable as its own `~/.claude` login, and only a non-empty
    /// directory names a different one — so "default" is spelled by saying
    /// nothing, never by setting the variable to something.
    #[test]
    fn default_account_carries_no_env() {
        assert!(Account::default_login().env().is_empty());
        let a = Account { name: "x".into(), dir: Some("/tmp/x".into()), email: None, added_at: 0 };
        assert_eq!(a.env(), vec![(ENV.to_string(), "/tmp/x".to_string())]);
        assert_eq!(Account::from_env(&a.env()).dir.as_deref(), Some("/tmp/x"));
        assert!(Account::from_env(&[]).is_default());
        assert!(Account::from_env(&[(ENV.into(), "  ".into())]).is_default());
    }

    #[test]
    fn names_are_directory_env_and_option_safe() {
        for ok in ["default", "acct2", "work-max", "a.b_c", "x"] {
            assert!(valid_name(ok), "{ok}");
        }
        for bad in ["", "Acct", "-x", ".hidden", "a b", "a/b", "über", &"x".repeat(33)] {
            assert!(!valid_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_missing_registry_is_just_the_default_login() {
        let r = load_from(&tmp("missing-nope"));
        assert_eq!(names(&r), vec!["default"]);
        assert!(r.exhausted.is_empty());
    }

    #[test]
    fn registry_roundtrips_and_keeps_default_first() {
        let p = tmp("roundtrip");
        let mut r = Registry::default();
        r.normalize();
        r.accounts.push(Account { name: "b".into(), dir: Some("/tmp/b".into()), email: Some("b@x".into()), added_at: 5 });
        r.accounts.push(Account { name: "a".into(), dir: Some("/tmp/a".into()), email: None, added_at: 6 });
        r.mark_exhausted("b", "five_hour", 4_000_000_000, None);
        save_to(&p, &r).unwrap();
        let back = load_from(&p);
        assert_eq!(names(&back), vec!["default", "b", "a"], "registry order is selection order");
        assert_eq!(back.exhausted["b"].kind, "five_hour");
        assert_eq!(back.get("b").unwrap().email.as_deref(), Some("b@x"));
        let _ = std::fs::remove_file(&p);
    }

    /// Hand edits and older files: a second `default`, a row without a dir, a
    /// duplicate — none of them may become a seat a turn tries to run on.
    #[test]
    fn normalize_drops_malformed_rows() {
        let mut r = Registry {
            accounts: vec![
                Account { name: "default".into(), dir: Some("/nope".into()), email: None, added_at: 0 },
                Account { name: "nodir".into(), dir: None, email: None, added_at: 0 },
                Account { name: "a".into(), dir: Some("/tmp/a".into()), email: None, added_at: 0 },
                Account { name: "a".into(), dir: Some("/tmp/a2".into()), email: None, added_at: 0 },
                Account { name: "Bad Name".into(), dir: Some("/tmp/bad".into()), email: None, added_at: 0 },
            ],
            exhausted: BTreeMap::new(),
        };
        r.normalize();
        assert_eq!(names(&r), vec!["default", "a"]);
        assert!(r.get("default").unwrap().is_default(), "default is never a directory");
        assert_eq!(r.get("a").unwrap().dir.as_deref(), Some("/tmp/a"), "first wins");
    }

    #[test]
    fn ordered_puts_the_preferred_seat_first_then_registry_order() {
        let mut r = Registry::default();
        r.normalize();
        for n in ["a", "b", "c"] {
            r.accounts.push(Account { name: n.into(), dir: Some(format!("/tmp/{n}")), email: None, added_at: 0 });
        }
        let names = |v: Vec<Account>| v.into_iter().map(|a| a.name).collect::<Vec<_>>();
        assert_eq!(names(r.ordered(Some("b"))), vec!["b", "default", "a", "c"]);
        assert_eq!(names(r.ordered(None)), vec!["default", "a", "b", "c"]);
        assert_eq!(names(r.ordered(Some("zzz"))), vec!["default", "a", "b", "c"], "unknown preference is ignored");
    }

    #[test]
    fn exhaustion_expires_at_its_reset_time() {
        let mut r = Registry::default();
        r.normalize();
        r.mark_exhausted("default", "seven_day", 1_000, None);
        assert!(r.exhausted_at("default", 999).is_some());
        assert!(r.exhausted_at("default", 1_000).is_none(), "the reset moment itself is usable");
        assert!(r.prune(1_000));
        assert!(r.exhausted.is_empty());
        assert!(!r.remove("default"), "the default login can't be forgotten");
    }

    #[test]
    fn usage_limit_text_yields_its_reset_epoch() {
        assert_eq!(usage_limit_reset("Claude usage limit reached|1785900000"), Some(Some(1785900000)));
        assert_eq!(
            usage_limit_reset("claude exited unsuccessfully: Claude usage limit reached|1785900000"),
            Some(Some(1785900000))
        );
        assert_eq!(usage_limit_reset("You've hit your usage limit for this window"), Some(None));
        // The 2026-09-06 field shape: no "usage", no epoch — still the wall.
        assert_eq!(usage_limit_reset("You've hit your session limit · resets 6:30pm (Asia/Shanghai)"), Some(None));
        assert_eq!(usage_limit_reset("API Error: 429 rate_limit_error"), Some(None));
        assert_eq!(usage_limit_reset("No conversation found with session ID"), None);
        assert_eq!(usage_limit_reset("working directory does not exist"), None);
    }

    /// The live payload shape (captured 2026-09-06): `limits[]` with percent /
    /// resets_at ISO / kind, plus `extra_usage`. 100% with extra usage off is
    /// a wall; 100% with extra usage paying is not.
    #[test]
    fn a_full_window_is_a_wall_unless_extra_usage_covers_it() {
        let now = 1_788_000_000;
        let payload = |pct: f64, extra: bool| {
            serde_json::json!({
                "extra_usage": { "is_enabled": extra, "spend_limit_reached": false },
                "limits": [
                    { "kind": "session", "percent": pct, "resets_at": "2026-09-06T10:29:59.853207+00:00" },
                    { "kind": "weekly_all", "percent": 47, "resets_at": "2026-09-11T12:59:59.853228+00:00" }
                ]
            })
        };
        let s = snapshot_from_usage(&payload(90.0, false), now);
        assert_eq!(s.state, SeatState::Ok);
        assert_eq!(s.worst.as_ref().map(|w| (w.0.as_str(), w.1)), Some(("session", 90.0)));
        assert_eq!(s.blocks(Some("opus")), None);

        let s = snapshot_from_usage(&payload(100.0, false), now);
        let SeatState::Exhausted { kind, until } = s.state.clone() else { panic!("{:?}", s.state) };
        assert_eq!(kind, "session");
        // 2026-09-06T10:29:59Z — the window's own reset time, not a guess.
        assert_eq!(until, crate::commands::iso_epoch_secs("2026-09-06T10:29:59.853207+00:00").unwrap());
        // Account-wide ⇒ it stops every model, and a turn with no model set.
        assert!(s.blocks(Some("opus")).is_some());
        assert!(s.blocks(None).is_some());

        assert_eq!(snapshot_from_usage(&payload(100.0, true), now).state, SeatState::Ok);
    }

    /// The state this machine was ACTUALLY in on 2026-09-06, verbatim off
    /// `/api/oauth/usage`: `weekly_scoped` full for Fable while `session`
    /// (29%) and `weekly_all` (54%) are fine. Calling that an exhausted
    /// ACCOUNT would hand every Sonnet/Opus/Haiku turn to another login for
    /// five days — so a scoped window walls only its own model.
    #[test]
    fn a_full_window_scoped_to_one_model_does_not_exhaust_the_account() {
        let now = 1_788_600_000;
        let v = serde_json::json!({
            "extra_usage": { "is_enabled": false, "spend_limit_reached": false },
            "limits": [
                { "kind": "session", "group": "session", "percent": 29, "severity": "normal",
                  "resets_at": "2026-09-06T15:29:59.640541+00:00", "scope": null, "is_active": false },
                { "kind": "weekly_all", "group": "weekly", "percent": 54, "severity": "normal",
                  "resets_at": "2026-09-11T12:59:59.640566+00:00", "scope": null, "is_active": false },
                { "kind": "weekly_scoped", "group": "weekly", "percent": 100, "severity": "critical",
                  "resets_at": "2026-09-11T12:59:59.640811+00:00",
                  "scope": { "model": { "id": null, "display_name": "Fable" } }, "is_active": true }
            ]
        });
        let s = snapshot_from_usage(&v, now);
        assert_eq!(s.state, SeatState::Ok, "the ACCOUNT is not out — one model is");
        assert_eq!(s.walls.len(), 1);
        assert_eq!(s.walls[0].scope.as_deref(), Some("fable"));

        assert!(s.blocks(Some("fable")).is_some(), "the model that is out");
        assert!(s.blocks(Some("claude-fable-5-1")).is_some(), "…by full slug too");
        assert_eq!(s.blocks(Some("sonnet")), None, "every other model still runs here");
        assert_eq!(s.blocks(Some("opus")), None);
        // No model pinned = the harness's own default, which we can't know.
        // Letting it try costs one refusal (the runtime failover catches it);
        // guessing would give away a seat that is fine.
        assert_eq!(s.blocks(None), None);
        assert!(s.describe(now).starts_with("ok · fable out"), "{}", s.describe(now));
    }

    /// Both kinds full at once: the account-wide one is the verdict AND the
    /// reason, because it is the one that explains every refusal.
    #[test]
    fn an_account_wide_wall_outranks_a_scoped_one() {
        let now = 1_788_600_000;
        let v = serde_json::json!({ "limits": [
            { "kind": "weekly_scoped", "percent": 100, "resets_at": "2026-09-11T12:59:59Z",
              "scope": { "model": { "display_name": "Fable" } } },
            { "kind": "session", "percent": 100, "resets_at": "2026-09-06T15:29:59Z", "scope": null }
        ]});
        let s = snapshot_from_usage(&v, now);
        assert!(matches!(&s.state, SeatState::Exhausted { kind, .. } if kind == "session"));
        assert_eq!(s.blocks(Some("sonnet")).map(|w| w.kind.as_str()), Some("session"));
    }

    /// A window that names its model in its KIND (`seven_day_opus` — the
    /// shape the refusal event reports, where there is no `scope` object).
    #[test]
    fn a_model_scoped_kind_is_read_out_of_its_name() {
        assert_eq!(model_in_kind("seven_day_opus").as_deref(), Some("opus"));
        assert_eq!(model_in_kind("seven_day_sonnet").as_deref(), Some("sonnet"));
        assert_eq!(model_in_kind("seven_day"), None);
        assert_eq!(model_in_kind("five_hour"), None);
        assert_eq!(model_in_kind("weekly_all"), None);
        // …and a hold carrying it stops only that model.
        let x = Exhausted { until: 9, kind: "seven_day_opus".into(), at: 0, scope: Some("opus".into()) };
        assert!(x.stops(Some("opus")));
        assert!(!x.stops(Some("sonnet")));
        assert!(!x.stops(None));
        // A file written before scopes existed reads back as account-wide.
        let old: Exhausted = serde_json::from_str(r#"{"until":9,"kind":"seven_day","at":0}"#).unwrap();
        assert!(old.stops(Some("sonnet")) && old.stops(None));
    }

    /// A reset time already in the past (a stale cache, clock skew) must not
    /// pin the seat as exhausted forever — it gets a bounded hold instead.
    #[test]
    fn a_past_reset_time_becomes_a_bounded_hold() {
        let now = 4_000_000_000i64;
        let v = serde_json::json!({ "limits": [ { "kind": "session", "percent": 100, "resets_at": "2026-09-06T10:29:59Z" } ] });
        let SeatState::Exhausted { until, .. } = snapshot_from_usage(&v, now).state else { panic!() };
        assert_eq!(until, now + 3600);
    }

    /// The probe cache is process-global — one per daemon, which is the whole
    /// point of it — so the tests that seed `default` have to take turns or
    /// they read each other's answers.
    async fn cache_turn() -> tokio::sync::MutexGuard<'static, ()> {
        static L: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
        L.get_or_init(|| tokio::sync::Mutex::new(())).lock().await
    }

    /// A registry of `default` + the named seats, with a probe result already
    /// cached for each — the shape `first_usable` walks.
    fn seeded(seats: &[(&str, SeatSnapshot)]) -> Registry {
        let mut r = Registry::default();
        r.normalize();
        for (n, snap) in seats {
            if *n != DEFAULT {
                r.accounts.push(Account { name: (*n).into(), dir: Some(format!("/tmp/{n}")), email: None, added_at: 0 });
            }
            seed_seat(n, snap.clone());
        }
        r
    }

    fn full(kind: &str, until: i64, scope: Option<&str>) -> SeatSnapshot {
        let w = Wall { kind: kind.into(), until, scope: scope.map(str::to_string) };
        let state = match scope {
            None => SeatState::Exhausted { kind: kind.into(), until },
            Some(_) => SeatState::Ok,
        };
        SeatSnapshot { state, worst: None, walls: vec![w] }
    }

    fn healthy() -> SeatSnapshot {
        SeatSnapshot { state: SeatState::Ok, worst: None, walls: vec![] }
    }

    /// The whole point of the feature: the login a turn was aimed at is out,
    /// so the turn goes to the next one — and the registry remembers the wall
    /// so the next turn doesn't have to rediscover it.
    #[tokio::test]
    async fn a_full_seat_hands_the_turn_to_the_next_login() {
        let _turn = cache_turn().await;
        let now = 1_788_600_000;
        let mut reg = seeded(&[
            ("default", full("session", now + 3600, None)),
            ("acct-b", healthy()),
        ]);
        let order = reg.ordered(None);
        let mut skipped = vec![];
        let (pick, dirty) = first_usable(&mut reg, &order, now, Some("opus"), &mut skipped).await;
        assert_eq!(pick.map(|a| a.name).as_deref(), Some("acct-b"));
        assert!(dirty, "the wall it found is worth remembering");
        assert_eq!(reg.exhausted["default"].kind, "session");
        assert_eq!(skipped.len(), 1);
        assert!(skipped[0].1.contains("exhausted (session)"), "{:?}", skipped);
    }

    /// A window scoped to ONE model doesn't move a turn running a different
    /// one — the seat stays where it is, and the hold is still recorded so a
    /// turn on the walled model can act on it.
    #[tokio::test]
    async fn a_model_scoped_wall_only_moves_that_models_turns() {
        let _turn = cache_turn().await;
        let now = 1_788_600_000;
        let mut reg = seeded(&[
            ("default", full("weekly_scoped", now + 5 * 86400, Some("fable"))),
            ("acct-c", healthy()),
        ]);
        let order = reg.ordered(None);

        let mut skipped = vec![];
        let (pick, _) = first_usable(&mut reg, &order, now, Some("sonnet"), &mut skipped).await;
        assert_eq!(pick.map(|a| a.name).as_deref(), Some("default"), "sonnet is fine here");
        assert!(skipped.is_empty());
        assert_eq!(reg.exhausted["default"].scope.as_deref(), Some("fable"), "the hold is still recorded");

        let mut skipped = vec![];
        let (pick, _) = first_usable(&mut reg, &order, now, Some("fable"), &mut skipped).await;
        assert_eq!(pick.map(|a| a.name).as_deref(), Some("acct-c"), "fable has to move");
        assert!(skipped[0].1.contains("is out of fable"), "{:?}", skipped);
    }

    /// Every seat is out: somebody still has to run the turn, so the
    /// preferred one does and the turn itself reports the wall. Answering
    /// "no" by staying silent is the one outcome that helps nobody.
    #[tokio::test]
    async fn when_every_seat_is_out_the_preferred_one_still_runs() {
        let _turn = cache_turn().await;
        let now = 1_788_600_000;
        let mut reg = seeded(&[
            ("default", full("session", now + 60, None)),
            ("acct-d", full("weekly_all", now + 60, None)),
        ]);
        let order = reg.ordered(None);
        let mut skipped = vec![];
        let (pick, _) = first_usable(&mut reg, &order, now, Some("opus"), &mut skipped).await;
        assert!(pick.is_none());
        assert_eq!(skipped.len(), 2, "and it can say why about each: {skipped:?}");
    }

    /// A remembered hold that the probe no longer sees is CLEARED, not
    /// obeyed: a transient refusal must not lock a login out for the length
    /// of the window it claimed.
    #[tokio::test]
    async fn a_hold_the_probe_contradicts_is_dropped() {
        let _turn = cache_turn().await;
        let now = 1_788_600_000;
        let mut reg = seeded(&[("default", healthy()), ("acct-e", healthy())]);
        reg.mark_exhausted("default", "session", now + 3600, None);
        let order = reg.ordered(None);
        let mut skipped = vec![];
        let (pick, dirty) = first_usable(&mut reg, &order, now, None, &mut skipped).await;
        assert_eq!(pick.map(|a| a.name).as_deref(), Some("default"));
        assert!(dirty && !reg.exhausted.contains_key("default"), "stale hold must be forgotten");
    }

    #[test]
    fn a_choice_explains_why_it_moved() {
        let c = Choice {
            account: Account { name: "b".into(), dir: Some("/tmp/b".into()), email: None, added_at: 0 },
            preferred: "default".into(),
            skipped: vec![("default".into(), "is exhausted (five_hour) — resets in 1h".into())],
        };
        assert!(c.switched());
        let n = c.note().unwrap();
        assert!(n.contains("`b`") && n.contains("`default` is exhausted"), "{n}");
        let same = Choice { preferred: "b".into(), ..c };
        assert_eq!(same.note(), None);
    }
}
