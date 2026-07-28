//! Zero-dependency mock HTTP server for the net/methods/store tests.
//!
//! Binds a std `TcpListener` on `127.0.0.1:0` (port known synchronously, so
//! concurrent tests never collide), converts it to tokio, and serves one canned
//! `(status, body)` per CONNECTION in order (the last repeats). Every response
//! carries `connection: close` — this is LOAD-BEARING: the shared reqwest
//! client pools connections per host, and without close a pooled second
//! request would wait forever on an already-consumed connection, breaking the
//! one-response-per-request pairing the tests rely on.
//!
//! Each request is captured (`path`, `authorization` header, raw body) so
//! tests can assert the exact wire shape the client sent — e.g. that
//! `SendMessageParams` really omits `channel_id` when None.

use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub(crate) struct Captured {
    pub path: String,
    pub auth: Option<String>,
    pub body: String,
}

pub(crate) struct MockApi {
    /// `http://127.0.0.1:<port>` — pass as the rpc `base` (no `/api` suffix;
    /// the method name becomes the path directly).
    pub base: String,
    pub requests: Arc<Mutex<Vec<Captured>>>,
}

impl MockApi {
    /// The nth captured request (panics if it never arrived).
    pub fn request(&self, n: usize) -> Captured {
        self.requests.lock().unwrap().get(n).cloned().unwrap_or_else(|| panic!("no request #{n} captured"))
    }
}

/// Envelope sugar: a 200 `{ok:true,result:<json>}` response.
pub(crate) fn ok(result_json: &str) -> (u16, String) {
    (200, format!("{{\"ok\":true,\"result\":{result_json}}}"))
}

/// Spawn the mock on the AMBIENT tokio runtime (call from `#[tokio::test]`).
/// `responses` are served per-connection in order; the last one repeats.
pub(crate) fn spawn_mock(responses: Vec<(u16, String)>) -> MockApi {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind mock");
    std_listener.set_nonblocking(true).expect("nonblocking");
    let base = format!("http://{}", std_listener.local_addr().unwrap());
    let listener = tokio::net::TcpListener::from_std(std_listener).expect("tokio listener");
    let requests: Arc<Mutex<Vec<Captured>>> = Arc::new(Mutex::new(Vec::new()));

    let reqs = requests.clone();
    tokio::spawn(async move {
        let mut served = 0usize;
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let (status, body) = responses
                .get(served.min(responses.len().saturating_sub(1)))
                .cloned()
                .unwrap_or((200, "{\"ok\":true,\"result\":null}".into()));
            served += 1;
            let reqs = reqs.clone();
            // Serve inline (tests are strictly request→response sequential).
            let _ = serve_one(&mut sock, status, &body, &reqs).await;
        }
    });

    MockApi { base, requests }
}

async fn serve_one(
    sock: &mut tokio::net::TcpStream,
    status: u16,
    body: &str,
    reqs: &Arc<Mutex<Vec<Captured>>>,
) -> std::io::Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    // Read headers (until CRLFCRLF), then Content-Length body bytes.
    let mut buf = Vec::with_capacity(1024);
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(()); // peer went away
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find_crlfcrlf(&buf) {
            break pos + 4;
        }
        if buf.len() > 64 * 1024 {
            return Ok(()); // absurd header block — bail
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let content_length: usize = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse().ok())?
        })
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
    }

    let path = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    let auth = head.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case("authorization").then(|| v.trim().to_string())
    });
    let req_body = String::from_utf8_lossy(&buf[header_end..header_end + content_length]).to_string();
    reqs.lock().unwrap().push(Captured { path, auth, body: req_body });

    let resp = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len(),
    );
    sock.write_all(resp.as_bytes()).await?;
    sock.shutdown().await?;
    Ok(())
}

fn find_crlfcrlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}
