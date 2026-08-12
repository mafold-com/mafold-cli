//! Long-lived Codex App Server transport.
//!
//! This module owns process and JSON-RPC plumbing only. The Codex harness builds
//! thread/turn routing, event normalization, and approval UI on top. Keeping the
//! adapter separate means transport code never contains Mafold presentation logic.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::sync::{broadcast, oneshot, Mutex as AsyncMutex};

const EVENT_BUFFER: usize = 1_024;
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_INITIALIZE_TIMEOUT: Duration = Duration::from_secs(15);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// Process and protocol settings for one App Server connection.
#[derive(Debug, Clone)]
pub struct CodexAppServerOptions {
    /// Executable to launch. Public primarily so tests and packaged installs can
    /// point at an explicit Codex binary.
    pub program: OsString,
    /// Complete arguments passed after `program`.
    pub args: Vec<OsString>,
    /// Optional process cwd. Thread/turn calls should still pass their own cwd.
    pub current_dir: Option<PathBuf>,
    /// Timeout for ordinary client requests.
    pub request_timeout: Duration,
    /// Timeout for the mandatory initialize handshake.
    pub initialize_timeout: Duration,
    /// Opt in to Codex's explicitly experimental App Server API surface.
    pub experimental_api: bool,
}

impl Default for CodexAppServerOptions {
    fn default() -> Self {
        Self {
            program: OsString::from("codex"),
            args: vec![
                OsString::from("app-server"),
                OsString::from("--listen"),
                OsString::from("stdio://"),
            ],
            current_dir: None,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            initialize_timeout: DEFAULT_INITIALIZE_TIMEOUT,
            experimental_api: false,
        }
    }
}

/// A server-originated message or process lifecycle event.
///
/// Responses to requests made through [`CodexAppServer::request`] are resolved
/// internally. Notifications and server requests are broadcast here so the
/// future Harness adapter can map them into `AgentEvent`s and approval cards.
#[derive(Debug, Clone, PartialEq)]
pub enum CodexAppServerEvent {
    Notification {
        method: String,
        params: Value,
    },
    ServerRequest {
        id: Value,
        method: String,
        params: Value,
    },
    Stderr(String),
    ProtocolError(String),
    ProcessExited {
        code: Option<i32>,
        requested: bool,
        error: Option<String>,
    },
}

/// JSON-RPC error returned by App Server, or a transport-level failure used to
/// release pending requests when its process exits.
#[derive(Debug, Clone, PartialEq)]
pub struct CodexAppServerRpcError {
    pub code: Option<i64>,
    pub message: String,
    pub data: Option<Value>,
}

impl CodexAppServerRpcError {
    fn from_wire(value: &Value) -> Self {
        Self {
            code: value["code"].as_i64(),
            message: value["message"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| value.to_string()),
            data: value.get("data").cloned(),
        }
    }

    fn transport(message: impl Into<String>) -> Self {
        Self {
            code: None,
            message: message.into(),
            data: None,
        }
    }
}

impl fmt::Display for CodexAppServerRpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(f, "App Server error {code}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for CodexAppServerRpcError {}

type RpcReply = std::result::Result<Value, CodexAppServerRpcError>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<RpcReply>>>>;

/// One initialized, long-lived `codex app-server` stdio connection.
///
/// Wrap this in an `Arc` when several daemon tasks need to issue requests. A
/// single writer lock preserves JSONL message boundaries; response ids resolve
/// independent requests without serializing their completion.
pub struct CodexAppServer {
    writer: Arc<AsyncMutex<Option<ChildStdin>>>,
    pending: Pending,
    events: broadcast::Sender<CodexAppServerEvent>,
    next_id: AtomicU64,
    shutdown_tx: Mutex<Option<oneshot::Sender<()>>>,
    done_rx: AsyncMutex<Option<oneshot::Receiver<()>>>,
    exited: Arc<AtomicBool>,
    request_timeout: Duration,
    initialize_result: Value,
    pid: Option<u32>,
}

impl CodexAppServer {
    /// Spawn App Server and complete the required `initialize` / `initialized`
    /// handshake before returning. Stable APIs are used unless the caller opts
    /// in through [`CodexAppServerOptions::experimental_api`].
    pub async fn spawn(options: CodexAppServerOptions) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(&options.program);
        cmd.args(&options.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(dir) = &options.current_dir {
            cmd.current_dir(dir);
        }
        crate::platform::no_window(&mut cmd);

        let mut child = cmd
            .spawn()
            .with_context(|| format!("couldn't run Codex App Server with {:?}", options.program))?;
        let pid = child.id();
        let stdin = child
            .stdin
            .take()
            .context("Codex App Server has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("Codex App Server has no stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("Codex App Server has no stderr")?;

        let writer = Arc::new(AsyncMutex::new(Some(stdin)));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _) = broadcast::channel(EVENT_BUFFER);
        let exited = Arc::new(AtomicBool::new(false));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (done_tx, done_rx) = oneshot::channel();

        spawn_stdout_reader(stdout, pending.clone(), events.clone());
        spawn_stderr_reader(stderr, events.clone());

        let waiter_pending = pending.clone();
        let waiter_events = events.clone();
        let waiter_exited = exited.clone();
        tokio::spawn(async move {
            // Keep the process registered for daemon shutdown for its complete
            // lifetime, not merely for the initialization request.
            let _child_guard = super::ChildGuard::new(pid);
            let (status, requested) = tokio::select! {
                status = child.wait() => (status, false),
                _ = shutdown_rx => {
                    let _ = child.start_kill();
                    (child.wait().await, true)
                }
            };

            let (code, error) = match status {
                Ok(status) => (status.code(), None),
                Err(error) => (None, Some(error.to_string())),
            };
            waiter_exited.store(true, Ordering::Release);
            let reason = error
                .clone()
                .unwrap_or_else(|| format!("Codex App Server exited (code {code:?})"));
            fail_pending(&waiter_pending, reason);
            let _ = waiter_events.send(CodexAppServerEvent::ProcessExited {
                code,
                requested,
                error,
            });
            let _ = done_tx.send(());
        });

        let mut server = Self {
            writer,
            pending,
            events,
            next_id: AtomicU64::new(0),
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            done_rx: AsyncMutex::new(Some(done_rx)),
            exited,
            request_timeout: options.request_timeout,
            initialize_result: Value::Null,
            pid,
        };

        let mut params = json!({
            "clientInfo": {
                "name": "mafold",
                "title": "Mafold",
                "version": env!("CARGO_PKG_VERSION"),
            }
        });
        if options.experimental_api {
            params["capabilities"] = json!({ "experimentalApi": true });
        }

        let initialize_result = match server
            .request_with_timeout("initialize", params, options.initialize_timeout)
            .await
        {
            Ok(result) => result,
            Err(error) => {
                let _ = server.shutdown().await;
                return Err(error).context("Codex App Server initialize failed");
            }
        };
        if let Err(error) = server.notify("initialized", json!({})).await {
            let _ = server.shutdown().await;
            return Err(error).context("Codex App Server initialized notification failed");
        }
        server.initialize_result = initialize_result;
        Ok(server)
    }

    /// Subscribe to notifications, approval requests, stderr, and lifecycle
    /// events. Subscribe before starting a turn so no server request is missed.
    pub fn subscribe(&self) -> broadcast::Receiver<CodexAppServerEvent> {
        self.events.subscribe()
    }

    pub fn initialize_result(&self) -> &Value {
        &self.initialize_result
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    pub fn is_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// Send a client request and await its correlated response.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.request_with_timeout(method, params, self.request_timeout)
            .await
    }

    async fn request_with_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value> {
        if self.is_exited() {
            bail!("Codex App Server is not running");
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let message = json!({ "method": method, "id": id, "params": params });
        if let Err(error) = self.write_message(&message).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(error);
        }

        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(result))) => Ok(result),
            Ok(Ok(Err(error))) => Err(anyhow!(error)),
            Ok(Err(_)) => bail!("Codex App Server response channel closed for {method}"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&id);
                bail!("Codex App Server request timed out: {method}")
            }
        }
    }

    /// Send a client notification (a JSON-RPC message without an id).
    pub async fn notify(&self, method: &str, params: Value) -> Result<()> {
        self.write_message(&json!({ "method": method, "params": params }))
            .await
    }

    /// Resolve a server-originated request such as an approval prompt.
    pub async fn respond(&self, id: Value, result: Value) -> Result<()> {
        validate_rpc_id(&id)?;
        self.write_message(&json!({ "id": id, "result": result }))
            .await
    }

    /// Reject a server-originated request using a JSON-RPC error response.
    pub async fn respond_error(
        &self,
        id: Value,
        code: i64,
        message: &str,
        data: Option<Value>,
    ) -> Result<()> {
        validate_rpc_id(&id)?;
        let mut error = json!({ "code": code, "message": message });
        if let Some(data) = data {
            error["data"] = data;
        }
        self.write_message(&json!({ "id": id, "error": error }))
            .await
    }

    /// Stop the child process and wait briefly for its waiter task to reap it.
    /// Calling this more than once is harmless.
    pub async fn shutdown(&self) -> Result<()> {
        self.writer.lock().await.take();
        if let Some(tx) = self.shutdown_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
        if let Some(done) = self.done_rx.lock().await.take() {
            tokio::time::timeout(SHUTDOWN_TIMEOUT, done)
                .await
                .context("timed out waiting for Codex App Server to stop")?
                .context("Codex App Server waiter dropped")?;
        }
        Ok(())
    }

    async fn write_message(&self, message: &Value) -> Result<()> {
        if self.is_exited() {
            bail!("Codex App Server is not running");
        }
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        let mut writer = self.writer.lock().await;
        let stdin = writer
            .as_mut()
            .context("Codex App Server stdin is closed")?;
        stdin
            .write_all(&bytes)
            .await
            .context("write to Codex App Server")?;
        stdin
            .flush()
            .await
            .context("flush Codex App Server stdin")?;
        Ok(())
    }
}

impl Drop for CodexAppServer {
    fn drop(&mut self) {
        // Drop cannot await the waiter, but signalling guarantees the task that
        // owns the child kills and reaps it. Normal daemon teardown should call
        // `shutdown()` when it needs confirmation.
        if let Some(tx) = self.shutdown_tx.lock().unwrap().take() {
            let _ = tx.send(());
        }
    }
}

fn spawn_stdout_reader(
    stdout: tokio::process::ChildStdout,
    pending: Pending,
    events: broadcast::Sender<CodexAppServerEvent>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) if !line.trim().is_empty() => {
                    route_incoming(&line, &pending, &events)
                }
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(error) => {
                    let _ = events.send(CodexAppServerEvent::ProtocolError(format!(
                        "failed reading App Server stdout: {error}"
                    )));
                    break;
                }
            }
        }
    });
}

fn spawn_stderr_reader(
    stderr: tokio::process::ChildStderr,
    events: broadcast::Sender<CodexAppServerEvent>,
) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if !line.trim().is_empty() {
                let _ = events.send(CodexAppServerEvent::Stderr(line));
            }
        }
    });
}

fn route_incoming(line: &str, pending: &Pending, events: &broadcast::Sender<CodexAppServerEvent>) {
    let message: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(error) => {
            let _ = events.send(CodexAppServerEvent::ProtocolError(format!(
                "invalid App Server JSON: {error}"
            )));
            return;
        }
    };

    if let Some(method) = message["method"].as_str() {
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        if let Some(id) = message.get("id") {
            let _ = events.send(CodexAppServerEvent::ServerRequest {
                id: id.clone(),
                method: method.to_owned(),
                params,
            });
        } else {
            let _ = events.send(CodexAppServerEvent::Notification {
                method: method.to_owned(),
                params,
            });
        }
        return;
    }

    let Some(id) = message["id"].as_u64() else {
        let _ = events.send(CodexAppServerEvent::ProtocolError(
            "App Server message has neither method nor numeric response id".into(),
        ));
        return;
    };
    let Some(tx) = pending.lock().unwrap().remove(&id) else {
        let _ = events.send(CodexAppServerEvent::ProtocolError(format!(
            "App Server response has no pending request: {id}"
        )));
        return;
    };

    let reply = if let Some(error) = message.get("error") {
        Err(CodexAppServerRpcError::from_wire(error))
    } else if let Some(result) = message.get("result") {
        Ok(result.clone())
    } else {
        Err(CodexAppServerRpcError::transport(
            "App Server response has neither result nor error",
        ))
    };
    let _ = tx.send(reply);
}

fn fail_pending(pending: &Pending, reason: String) {
    let entries = std::mem::take(&mut *pending.lock().unwrap());
    for (_, tx) in entries {
        let _ = tx.send(Err(CodexAppServerRpcError::transport(reason.clone())));
    }
}

fn validate_rpc_id(id: &Value) -> Result<()> {
    if id.is_number() || id.is_string() {
        Ok(())
    } else {
        bail!("JSON-RPC id must be a number or string")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel() -> (
        Pending,
        broadcast::Sender<CodexAppServerEvent>,
        broadcast::Receiver<CodexAppServerEvent>,
    ) {
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, receiver) = broadcast::channel(16);
        (pending, events, receiver)
    }

    #[tokio::test]
    async fn response_resolves_the_matching_request() {
        let (pending, events, _) = channel();
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(7, tx);

        route_incoming(r#"{"id":7,"result":{"ok":true}}"#, &pending, &events);

        assert_eq!(rx.await.unwrap().unwrap(), json!({ "ok": true }));
        assert!(pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn rpc_error_resolves_as_error() {
        let (pending, events, _) = channel();
        let (tx, rx) = oneshot::channel();
        pending.lock().unwrap().insert(3, tx);

        route_incoming(
            r#"{"id":3,"error":{"code":-32000,"message":"nope","data":{"retry":false}}}"#,
            &pending,
            &events,
        );

        let error = rx.await.unwrap().unwrap_err();
        assert_eq!(error.code, Some(-32000));
        assert_eq!(error.message, "nope");
        assert_eq!(error.data, Some(json!({ "retry": false })));
    }

    #[test]
    fn notification_and_server_request_are_broadcast() {
        let (pending, events, mut receiver) = channel();
        route_incoming(
            r#"{"method":"turn/started","params":{"turn":{"id":"t1"}}}"#,
            &pending,
            &events,
        );
        route_incoming(
            r#"{"method":"item/fileChange/requestApproval","id":"approval-1","params":{"turnId":"t1"}}"#,
            &pending,
            &events,
        );

        assert_eq!(
            receiver.try_recv().unwrap(),
            CodexAppServerEvent::Notification {
                method: "turn/started".into(),
                params: json!({ "turn": { "id": "t1" } }),
            }
        );
        assert_eq!(
            receiver.try_recv().unwrap(),
            CodexAppServerEvent::ServerRequest {
                id: json!("approval-1"),
                method: "item/fileChange/requestApproval".into(),
                params: json!({ "turnId": "t1" }),
            }
        );
    }

    #[test]
    fn malformed_and_unmatched_messages_are_visible() {
        let (pending, events, mut receiver) = channel();
        route_incoming("not json", &pending, &events);
        route_incoming(r#"{"id":99,"result":{}}"#, &pending, &events);

        assert!(matches!(
            receiver.try_recv().unwrap(),
            CodexAppServerEvent::ProtocolError(message) if message.contains("invalid App Server JSON")
        ));
        assert!(matches!(
            receiver.try_recv().unwrap(),
            CodexAppServerEvent::ProtocolError(message) if message.contains("no pending request: 99")
        ));
    }

    #[test]
    fn response_ids_accept_numbers_and_strings_only() {
        assert!(validate_rpc_id(&json!(1)).is_ok());
        assert!(validate_rpc_id(&json!("approval-1")).is_ok());
        assert!(validate_rpc_id(&Value::Null).is_err());
        assert!(validate_rpc_id(&json!({})).is_err());
    }

    #[tokio::test]
    async fn process_failure_releases_every_pending_request() {
        let (pending, _, _) = channel();
        let (tx1, rx1) = oneshot::channel();
        let (tx2, rx2) = oneshot::channel();
        pending.lock().unwrap().insert(1, tx1);
        pending.lock().unwrap().insert(2, tx2);

        fail_pending(&pending, "server stopped".into());

        assert_eq!(rx1.await.unwrap().unwrap_err().message, "server stopped");
        assert_eq!(rx2.await.unwrap().unwrap_err().message, "server stopped");
        assert!(pending.lock().unwrap().is_empty());
    }

    /// Manual compatibility smoke test against the installed Codex version.
    /// Kept ignored so normal CI does not require a Codex installation.
    #[tokio::test]
    #[ignore = "requires an installed Codex CLI with App Server support"]
    async fn live_codex_app_server_handshake() {
        if !super::super::on_path("codex") {
            return;
        }
        let server = CodexAppServer::spawn(CodexAppServerOptions::default())
            .await
            .unwrap();
        assert!(server.initialize_result().is_object());
        assert!(server.initialize_result()["userAgent"].is_string());
        assert!(server.pid().is_some());
        let models = server
            .request("model/list", json!({ "limit": 1, "includeHidden": false }))
            .await
            .unwrap();
        assert!(models["data"].is_array());
        server.shutdown().await.unwrap();
        assert!(server.is_exited());
    }
}
