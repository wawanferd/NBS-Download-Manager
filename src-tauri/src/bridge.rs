//! Local WebSocket server (127.0.0.1 only) that receives downloads captured
//! by the browser extension, directly or relayed by the native messaging host.

use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::Arc;

use futures_util::{SinkExt, StreamExt};
use nbs_core::NewDownload;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tauri::async_runtime::JoinHandle;
use tauri::{AppHandle, Emitter, Manager};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::Message;

use crate::state::AppState;

#[derive(Default)]
pub struct Bridge {
    port: AtomicU16,
    listening: AtomicBool,
    clients: AtomicUsize,
    error: Mutex<Option<String>>,
    task: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeStatus {
    pub port: u16,
    pub listening: bool,
    pub clients: usize,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserDownload {
    request_id: Option<String>,
    url: String,
    filename: Option<String>,
    referrer: Option<String>,
    cookies: Option<String>,
    user_agent: Option<String>,
    file_size: Option<i64>,
    mime: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Incoming {
    Hello,
    Ping,
    Download(BrowserDownload),
}

/// A captured download waiting for the user to confirm it in the UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingDownload {
    pub url: String,
    pub file_name: Option<String>,
    pub file_size: Option<u64>,
    pub mime: Option<String>,
    pub headers: Vec<(String, String)>,
}

impl Bridge {
    pub fn status(&self) -> BridgeStatus {
        BridgeStatus {
            port: self.port.load(Ordering::Relaxed),
            listening: self.listening.load(Ordering::Relaxed),
            clients: self.clients.load(Ordering::Relaxed),
            error: self.error.lock().clone(),
        }
    }

    /// (Re)starts the server on `port`. Existing connections stay open.
    pub fn start(self: &Arc<Self>, app: AppHandle, port: u16) {
        if let Some(task) = self.task.lock().take() {
            task.abort();
        }
        self.port.store(port, Ordering::Relaxed);
        self.listening.store(false, Ordering::Relaxed);
        let bridge = self.clone();
        let task = tauri::async_runtime::spawn(async move { bridge.serve(app, port).await });
        *self.task.lock() = Some(task);
    }

    fn emit_status(&self, app: &AppHandle) {
        let _ = app.emit("bridge://status", self.status());
    }

    async fn serve(self: Arc<Self>, app: AppHandle, port: u16) {
        let listener = match TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => l,
            Err(e) => {
                *self.error.lock() = Some(format!("cannot listen on 127.0.0.1:{port}: {e}"));
                self.emit_status(&app);
                return;
            }
        };
        self.listening.store(true, Ordering::Relaxed);
        *self.error.lock() = None;
        self.emit_status(&app);
        loop {
            let Ok((stream, _)) = listener.accept().await else { continue };
            let (bridge, app) = (self.clone(), app.clone());
            tauri::async_runtime::spawn(async move { bridge.handle(app, stream).await });
        }
    }

    async fn handle(self: Arc<Self>, app: AppHandle, stream: TcpStream) {
        // Web pages can open WebSockets to localhost too; only accept browser
        // extensions (which send their own origin) and local processes such as
        // the native messaging host (which send none).
        let check_origin = |req: &Request, resp: Response| -> Result<Response, ErrorResponse> {
            match req.headers().get("origin").and_then(|o| o.to_str().ok()) {
                None => Ok(resp),
                Some(origin) if is_extension_origin(origin) => Ok(resp),
                Some(_) => {
                    let mut denied = ErrorResponse::new(Some("origin not allowed".into()));
                    *denied.status_mut() = StatusCode::FORBIDDEN;
                    Err(denied)
                }
            }
        };
        let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, check_origin).await else { return };
        let (mut tx, mut rx) = ws.split();

        self.clients.fetch_add(1, Ordering::Relaxed);
        self.emit_status(&app);
        let welcome = json!({ "type": "welcome", "app": "NBS Download Manager", "version": env!("CARGO_PKG_VERSION") });
        let _ = tx.send(Message::text(welcome.to_string())).await;

        while let Some(Ok(msg)) = rx.next().await {
            let reply = match msg {
                Message::Text(text) => match serde_json::from_str::<Incoming>(text.as_str()) {
                    Ok(Incoming::Hello) | Ok(Incoming::Ping) => Some(json!({ "type": "pong" })),
                    Ok(Incoming::Download(d)) => {
                        let request_id = d.request_id.clone();
                        Some(match accept_download(&app, d) {
                            Ok(()) => json!({ "type": "ack", "requestId": request_id, "accepted": true }),
                            Err(e) => json!({ "type": "ack", "requestId": request_id, "accepted": false, "error": e }),
                        })
                    }
                    Err(e) => Some(json!({ "type": "error", "message": format!("invalid message: {e}") })),
                },
                Message::Close(_) => break,
                _ => None,
            };
            if let Some(reply) = reply {
                if tx.send(Message::text(reply.to_string())).await.is_err() {
                    break;
                }
            }
        }

        self.clients.fetch_sub(1, Ordering::Relaxed);
        self.emit_status(&app);
    }
}

fn is_extension_origin(origin: &str) -> bool {
    ["chrome-extension://", "moz-extension://", "extension://", "safari-web-extension://"]
        .iter()
        .any(|p| origin.starts_with(p))
}

fn accept_download(app: &AppHandle, d: BrowserDownload) -> Result<(), String> {
    let url = d.url.trim().to_string();
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("only http and https downloads can be captured".into());
    }
    let mut headers = Vec::new();
    for (name, value) in [("Referer", d.referrer), ("Cookie", d.cookies), ("User-Agent", d.user_agent)] {
        if let Some(v) = value.filter(|v| !v.is_empty()) {
            headers.push((name.to_string(), v));
        }
    }
    let file_name = d.filename.filter(|f| !f.is_empty());

    let state = app.state::<AppState>();
    if state.settings.lock().auto_start_browser {
        let view = state
            .manager
            .add(NewDownload {
                url,
                file_name,
                headers,
                start: true,
                source: Some("browser".into()),
                ..Default::default()
            })
            .map_err(|e| e.to_string())?;
        let _ = app.emit("bridge://captured", view);
    } else {
        let pending = PendingDownload {
            url,
            file_name,
            file_size: d.file_size.and_then(|s| u64::try_from(s).ok()).filter(|s| *s > 0),
            mime: d.mime,
            headers,
        };
        crate::show_main_window(app);
        let _ = app.emit("bridge://download-request", pending);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_extension_origins_are_allowed() {
        assert!(is_extension_origin("chrome-extension://abcdef"));
        assert!(is_extension_origin("moz-extension://1234-5678"));
        assert!(!is_extension_origin("https://evil.example"));
        assert!(!is_extension_origin("null"));
    }

    #[test]
    fn parses_extension_messages() {
        let msg: Incoming = serde_json::from_str(
            r#"{"type":"download","requestId":"1","url":"https://x.org/a.zip","filename":"a.zip","fileSize":10}"#,
        )
        .unwrap();
        assert!(matches!(msg, Incoming::Download(d) if d.url == "https://x.org/a.zip" && d.file_size == Some(10)));
        assert!(matches!(serde_json::from_str(r#"{"type":"ping"}"#).unwrap(), Incoming::Ping));
        assert!(matches!(
            serde_json::from_str(r#"{"type":"hello","client":"chrome","version":"1.0.0"}"#).unwrap(),
            Incoming::Hello
        ));
    }
}
