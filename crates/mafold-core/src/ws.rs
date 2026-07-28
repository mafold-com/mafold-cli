//! WS subscription — the sync engine's realtime layer, IN THE CORE.
//! Streams each text frame from the server to a host callback, with an internal
//! heartbeat (keeps presence warm), reconnect on drop, and open/close signals so
//! the host can drive its connection status. wasm = gloo-net; native =
//! tokio-tungstenite.

use futures::StreamExt;

/// Control sentinels the host distinguishes from real (JSON) server frames.
/// Shared by BOTH bindings so iOS (UniFFI) and web (wasm) see identical signals.
pub const OPEN: &str = "\u{1}open";
pub const CLOSE: &str = "\u{1}close";

// ───────────────────────── web (wasm) ─────────────────────────
/// Reconnecting WS: streams text frames to `on_event`, emits OPEN/CLOSE sentinels
/// around each connection, and pings every 25s (the server treats any inbound
/// frame as a heartbeat). Stops when `closed` is set (the JS handle's `close()`).
#[cfg(target_arch = "wasm32")]
pub fn connect(url: &str, on_event: js_sys::Function, closed: std::rc::Rc<std::cell::Cell<bool>>) {
    use futures::{select, FutureExt};
    use gloo_net::websocket::{futures::WebSocket, Message};
    use gloo_timers::future::{IntervalStream, TimeoutFuture};
    use wasm_bindgen::JsValue;

    let url = url.to_string();
    let emit = move |s: &str| {
        let _ = on_event.call1(&JsValue::NULL, &JsValue::from_str(s));
    };
    wasm_bindgen_futures::spawn_local(async move {
        while !closed.get() {
            let ws = match WebSocket::open(&url) {
                Ok(w) => w,
                Err(_) => {
                    TimeoutFuture::new(2000).await;
                    continue;
                }
            };
            let (mut write, mut read) = ws.split();
            emit(OPEN);
            let mut ping = IntervalStream::new(25_000);
            loop {
                select! {
                    frame = read.next().fuse() => match frame {
                        Some(Ok(Message::Text(t))) => emit(&t),
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break,
                    },
                    _ = ping.next().fuse() => {
                        use futures::SinkExt;
                        let _ = write.send(Message::Text("{\"method\":\"ping\"}".into())).await;
                    }
                }
                if closed.get() {
                    return;
                }
            }
            emit(CLOSE);
            if closed.get() {
                return;
            }
            TimeoutFuture::new(2000).await; // backoff, then reconnect
        }
    });
}

// ───────────────────────── native (iOS/Android) ─────────────────────────
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, OnceLock};
    use std::time::Duration;
    use tokio::runtime::Runtime;
    use super::{StreamExt, CLOSE, OPEN};

    static RT: OnceLock<Runtime> = OnceLock::new();
    fn rt() -> &'static Runtime {
        RT.get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio runtime")
        })
    }

    /// Host (Swift/Kotlin) implements this to receive each realtime text frame
    /// plus the OPEN/CLOSE control sentinels emitted around each connection.
    #[uniffi::export(callback_interface)]
    pub trait WsListener: Send + Sync {
        fn on_event(&self, text: String);
    }

    /// Returned by `connect_ws`; `close()` stops the reconnect loop and drops the
    /// socket (call it on logout / account switch / a host-forced reconnect).
    #[derive(uniffi::Object)]
    pub struct WsHandle {
        closed: Arc<AtomicBool>,
    }
    #[uniffi::export]
    impl WsHandle {
        pub fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    /// Open a RECONNECTING realtime WS: streams text frames to `listener`, emits
    /// OPEN/CLOSE sentinels around each connection, and pings every 25s (the
    /// server treats any inbound frame as a heartbeat — keeps presence warm and
    /// stops the proxy dropping the idle socket). Native mirror of the wasm
    /// `connect`; returns immediately, the loop runs on the core's runtime.
    #[uniffi::export]
    pub fn connect_ws(url: String, listener: Box<dyn WsListener>) -> Arc<WsHandle> {
        let closed = Arc::new(AtomicBool::new(false));
        let handle = Arc::new(WsHandle { closed: closed.clone() });
        rt().spawn(async move {
            use futures::SinkExt;
            use tokio_tungstenite::tungstenite::Message;
            while !closed.load(Ordering::SeqCst) {
                let (ws, _) = match tokio_tungstenite::connect_async(&url).await {
                    Ok(pair) => pair,
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(2000)).await;
                        continue;
                    }
                };
                let (mut write, mut read) = ws.split();
                listener.on_event(OPEN.to_string());
                let mut ping = tokio::time::interval(Duration::from_secs(25));
                ping.tick().await; // consume the immediate first tick
                loop {
                    tokio::select! {
                        frame = read.next() => match frame {
                            Some(Ok(Message::Text(t))) => listener.on_event(t.to_string()),
                            Some(Ok(_)) => {}
                            Some(Err(_)) | None => break,
                        },
                        _ = ping.tick() => {
                            let _ = write.send(Message::Text("{\"method\":\"ping\"}".into())).await;
                        }
                    }
                    if closed.load(Ordering::SeqCst) {
                        return;
                    }
                }
                listener.on_event(CLOSE.to_string());
                if closed.load(Ordering::SeqCst) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(2000)).await; // backoff, then reconnect
            }
        });
        handle
    }
}
