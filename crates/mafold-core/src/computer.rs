//! The `computer` driver's protocol — what a machine of your own offers, and
//! how a request for it is read.
//!
//! Every other connection ends in an HTTP request, so the core can both decide
//! *what* to do and *do* it. This one ends in a process, and a process is not
//! something four of the five surfaces that link this core may run: a browser
//! tab cannot, and an iOS app must not. So the module splits along that line —
//! the **protocol** (method names, argument shapes, result shapes, limits) lives
//! here, shared, and the **execution** is installed by whichever host can
//! honestly perform it ([`Runtime::attach_computer`](crate::connections::Runtime::attach_computer),
//! today only mafold-cli's daemon).
//!
//! That is not a compromise, it is the point: a host with no executor attached
//! silently declines a computer call before claiming it, so the machine that
//! *can* answer gets the claim instead of losing a race to a phone.

use serde_json::{json, Value};

/// The name the registry row uses in `native_api`. Data names the driver; this
/// is the code it names.
pub const DRIVER: &str = "computer";

/// The longest a `shell.exec` may run before it is killed and its partial
/// output returned.
///
/// Tied to the api's 30s park on `callConnection`, deliberately and with room
/// to spare: a caller that is going to be told "no device answered in 30s"
/// learns nothing, whereas a caller that gets 25 seconds of output plus
/// `timed_out: true` learns exactly where the command got stuck. Anything
/// longer is a different verb — `shell.spawn`, which returns immediately and is
/// read back with `shell.status`.
pub const MAX_EXEC_MS: u64 = 25_000;

/// What a caller gets if it names no timeout. Short enough that a hung command
/// is a fast, legible failure rather than a stalled turn.
pub const DEFAULT_EXEC_MS: u64 = 15_000;

/// Per-stream output cap. Both streams ride home as JSON through a relay that
/// parks a caller's request, so an unbounded `cat` of a log file is a way to
/// hurt the server rather than a way to read a log. Whatever is dropped is
/// reported (`truncated`), never silently swallowed.
pub const MAX_OUTPUT: usize = 64 * 1024;

/// One request, already validated. The host's executor sees only this — it
/// never parses JSON, so a second host cannot disagree about what
/// `timeout_ms: 0` means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    /// Run to completion (or to `timeout_ms`) and answer with the output.
    Exec {
        cmd: String,
        cwd: Option<String>,
        timeout_ms: u64,
    },
    /// Start it and let go. Survives this call, the daemon's restart, and the
    /// caller hanging up — the point of the verb.
    Spawn { cmd: String, cwd: Option<String> },
    /// How is it going, and what has it printed?
    Status { task_id: String, tail: usize },
    /// Stop it. The task's own process GROUP, not just the pid it started with:
    /// a shell that forked children and exited would otherwise leave them
    /// running under a task that reports itself dead.
    Kill { task_id: String },
}

/// Lines of output `shell.status` returns when the caller names no `tail`.
pub const DEFAULT_TAIL: usize = 200;

/// Read a relayed `{method, params}` into a [`Job`].
///
/// Rejects rather than defaults wherever a default would be a guess about what
/// the caller wanted to RUN. `cmd` is the whole of it: this driver hands a
/// string to a shell, which is what makes it useful and is also why nothing
/// here tries to look inside it. There is no allowlist, no argv splitting, and
/// no quoting to get wrong — the machine's own user account is the boundary,
/// and the owner chose that boundary knowingly.
pub fn parse(method: &str, params: &Value) -> Result<Job, String> {
    let str_of = |k: &str| {
        params
            .get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    let cmd = || {
        str_of("cmd")
            .or_else(|| str_of("command"))
            .ok_or_else(|| format!("{method} needs `cmd` — the command line to run"))
    };
    let task_id = || {
        str_of("task_id")
            .ok_or_else(|| format!("{method} needs `task_id`, as returned by shell.spawn"))
    };
    match method {
        "shell.exec" => {
            let asked = params
                .get("timeout_ms")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_EXEC_MS);
            Ok(Job::Exec {
                cmd: cmd()?,
                cwd: str_of("cwd"),
                // Clamped, not refused. A caller asking for ten minutes is
                // asking for something reasonable that this verb cannot do; it
                // gets 25 seconds and `timed_out: true`, which names the
                // situation, where an error would just look like a rejection.
                timeout_ms: asked.clamp(1, MAX_EXEC_MS),
            })
        }
        "shell.spawn" => Ok(Job::Spawn {
            cmd: cmd()?,
            cwd: str_of("cwd"),
        }),
        "shell.status" => Ok(Job::Status {
            task_id: task_id()?,
            tail: params
                .get("tail")
                .and_then(Value::as_u64)
                .map(|n| n.clamp(1, 5_000) as usize)
                .unwrap_or(DEFAULT_TAIL),
        }),
        "shell.kill" => Ok(Job::Kill { task_id: task_id()? }),
        other => Err(format!(
            "`{other}` is not something a computer offers — it has {}",
            METHODS
                .iter()
                .map(|m| m.name)
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Is this method one of ours?
///
/// The pre-claim gate uses it to stay cheap: on a device that can spawn
/// processes at all, the only calls whose answer depends on WHICH machine we
/// are are these four. Everything else — an MCP tool, codex's `responses`, the
/// reserved `tools/list` — is answered identically by any device holding the
/// key, so making them pay a `listConnections` round trip before every claim
/// would slow the common path to protect a case it cannot be.
pub fn owns_method(method: &str) -> bool {
    METHODS.iter().any(|m| m.name == method)
}

/// One method, as both the catalog and the error above need it.
pub struct Method {
    pub name: &'static str,
    pub description: &'static str,
    pub schema: fn() -> Value,
    /// MCP's `readOnlyHint`, and honest for once: we are the provider here, so
    /// unlike a third party's self-report this is a fact about code in this
    /// repo. `shell.status` really does only read.
    pub read_only: bool,
}

pub const METHODS: &[Method] = &[
    Method {
        name: "shell.exec",
        description: "Run a shell command on this machine and wait for it. Returns exit code, \
                      stdout and stderr. Killed at 25s — use shell.spawn for anything longer.",
        schema: exec_schema,
        read_only: false,
    },
    Method {
        name: "shell.spawn",
        description: "Start a shell command and return immediately with a task id. The command \
                      keeps running after the call ends; read it back with shell.status.",
        schema: spawn_schema,
        read_only: false,
    },
    Method {
        name: "shell.status",
        description: "Whether a spawned task is still running, its exit code once it isn't, and \
                      the tail of its combined output.",
        schema: status_schema,
        read_only: true,
    },
    Method {
        name: "shell.kill",
        description: "Terminate a spawned task and its child processes.",
        schema: kill_schema,
        read_only: false,
    },
];

fn exec_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "cmd": { "type": "string", "description": "Command line, run through the machine's shell." },
            "cwd": { "type": "string", "description": "Working directory. Defaults to the daemon's home." },
            "timeout_ms": {
                "type": "integer",
                "description": "Give up after this long and return partial output with timed_out: true. Capped at 25000.",
            },
        },
        "required": ["cmd"],
    })
}

fn spawn_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "cmd": { "type": "string" },
            "cwd": { "type": "string" },
        },
        "required": ["cmd"],
    })
}

fn status_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": { "type": "string" },
            "tail": { "type": "integer", "description": "Last N lines of output. Default 200." },
        },
        "required": ["task_id"],
    })
}

fn kill_schema() -> Value {
    json!({
        "type": "object",
        "properties": { "task_id": { "type": "string" } },
        "required": ["task_id"],
    })
}

/// The catalog in the CORE's own vocabulary, for the paths that ask a
/// connection what it can do without going through `tools/list`
/// (`mafold connection methods`, the daemon's MCP aggregator).
///
/// Same four rows, one source. A second hand-written list here is how the cli
/// would end up printing a verb the driver no longer implements.
pub fn method_specs() -> Vec<crate::mcp::MethodSpec> {
    METHODS
        .iter()
        .map(|m| crate::mcp::MethodSpec {
            name: m.name.to_string(),
            title: m.name.to_string(),
            description: m.description.to_string(),
            input_schema: (m.schema)(),
            read_only: m.read_only,
        })
        .collect()
}

/// The catalog, in the shape `tools/list` already speaks.
///
/// A computer answers this for real, unlike the other native driver: `codex`'s
/// `responses` is a model call that no agent should see as a tool, while these
/// four ARE the tools. Same reserved method, same envelope — an agent that can
/// use a Notion connection can use a laptop without learning anything new.
pub fn catalog() -> Value {
    json!({
        "tools": METHODS
            .iter()
            .map(|m| json!({
                "name": m.name,
                "title": m.name,
                "description": m.description,
                "inputSchema": (m.schema)(),
                "readOnly": m.read_only,
            }))
            .collect::<Vec<_>>()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_beyond_the_relay_budget_is_clamped_not_refused() {
        let job = parse("shell.exec", &json!({ "cmd": "sleep 600", "timeout_ms": 600_000 })).unwrap();
        assert_eq!(
            job,
            Job::Exec {
                cmd: "sleep 600".into(),
                cwd: None,
                timeout_ms: MAX_EXEC_MS
            }
        );
    }

    /// `timeout_ms: 0` used to mean "no timeout" in the first cut, which is the
    /// one value that must NOT mean that: a caller who sends it gets a process
    /// nothing will ever kill, on someone else's machine.
    #[test]
    fn zero_is_the_shortest_timeout_not_an_infinite_one() {
        let Job::Exec { timeout_ms, .. } =
            parse("shell.exec", &json!({ "cmd": "x", "timeout_ms": 0 })).unwrap()
        else {
            panic!("exec")
        };
        assert_eq!(timeout_ms, 1);
    }

    #[test]
    fn a_missing_command_is_a_sentence_not_an_empty_run() {
        let e = parse("shell.exec", &json!({ "cwd": "/tmp" })).unwrap_err();
        assert!(e.contains("cmd"), "{e}");
    }

    /// The error a caller sees for a typo must list what does exist — this is
    /// the only place an agent that skipped `tools/list` can learn the names.
    #[test]
    fn an_unknown_method_names_the_real_ones() {
        let e = parse("shell.run", &json!({})).unwrap_err();
        for m in ["shell.exec", "shell.spawn", "shell.status", "shell.kill"] {
            assert!(e.contains(m), "{e} must mention {m}");
        }
    }

    #[test]
    fn the_catalog_is_every_method_and_says_which_only_read() {
        let cat = catalog();
        let tools = cat["tools"].as_array().unwrap();
        assert_eq!(tools.len(), METHODS.len());
        let status = tools.iter().find(|t| t["name"] == "shell.status").unwrap();
        assert_eq!(status["readOnly"], json!(true));
        assert_eq!(tools[0]["inputSchema"]["required"], json!(["cmd"]));
    }
}
