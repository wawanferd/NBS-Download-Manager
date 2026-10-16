//! Native Messaging host for the NBS browser extension.
//!
//! The browser starts this program and talks to it over stdin/stdout using
//! length-prefixed JSON. Every message is relayed to the app's local
//! WebSocket server and every reply is relayed back. If the app is not
//! running it is launched first.

use std::path::PathBuf;
use std::process::Stdio;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt, Stdout};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

const APP_IDENTIFIER: &str = "com.nbs.downloadmanager";
const DEFAULT_PORT: u16 = 18765;
const MAX_MESSAGE: usize = 4 * 1024 * 1024;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Reads the WebSocket port from the app's settings file.
fn bridge_port() -> u16 {
    dirs::config_dir()
        .map(|d| d.join(APP_IDENTIFIER).join("settings.json"))
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|data| serde_json::from_slice::<Value>(&data).ok())
        .and_then(|v| v.get("bridgePort")?.as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(DEFAULT_PORT)
}

fn app_binary() -> Option<PathBuf> {
    let name = if cfg!(windows) { "nbs-download-manager.exe" } else { "nbs-download-manager" };
    let path = std::env::current_exe().ok()?.parent()?.join(name);
    path.exists().then_some(path)
}

async fn connect(port: u16) -> Option<Socket> {
    let url = format!("ws://127.0.0.1:{port}/native");
    tokio_tungstenite::connect_async(url).await.ok().map(|(ws, _)| ws)
}

/// Connects to the app, starting it if needed.
async fn connect_or_launch(port: u16) -> Option<Socket> {
    if let Some(ws) = connect(port).await {
        return Some(ws);
    }
    let app = app_binary()?;
    std::process::Command::new(app)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(400)).await;
        if let Some(ws) = connect(port).await {
            return Some(ws);
        }
    }
    None
}

async fn read_message<R: AsyncReadExt + Unpin>(input: &mut R) -> Option<Vec<u8>> {
    let mut len = [0u8; 4];
    input.read_exact(&mut len).await.ok()?;
    let len = u32::from_ne_bytes(len) as usize;
    if len > MAX_MESSAGE {
        return None;
    }
    let mut buf = vec![0u8; len];
    input.read_exact(&mut buf).await.ok()?;
    Some(buf)
}

async fn write_message(out: &mut Stdout, payload: &[u8]) -> std::io::Result<()> {
    out.write_all(&(payload.len() as u32).to_ne_bytes()).await?;
    out.write_all(payload).await?;
    out.flush().await
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();

    let Some(ws) = connect_or_launch(bridge_port()).await else {
        let msg = json!({ "type": "error", "message": "NBS Download Manager is not running and could not be started" });
        let _ = write_message(&mut stdout, msg.to_string().as_bytes()).await;
        // Answer pending requests so the extension can fall back to the browser.
        while let Some(raw) = read_message(&mut stdin).await {
            let request_id = serde_json::from_slice::<Value>(&raw).ok().and_then(|v| v.get("requestId").cloned());
            let nack = json!({ "type": "ack", "requestId": request_id, "accepted": false, "error": "app unavailable" });
            if write_message(&mut stdout, nack.to_string().as_bytes()).await.is_err() {
                break;
            }
        }
        return;
    };
    let (mut ws_tx, mut ws_rx) = ws.split();

    let upstream = async {
        while let Some(raw) = read_message(&mut stdin).await {
            let Ok(text) = String::from_utf8(raw) else { continue };
            if ws_tx.send(Message::text(text)).await.is_err() {
                break;
            }
        }
        let _ = ws_tx.close().await;
    };
    let downstream = async {
        while let Some(Ok(msg)) = ws_rx.next().await {
            match msg {
                Message::Text(text) => {
                    if write_message(&mut stdout, text.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    };
    // Exit as soon as either side goes away: the browser closing the port or
    // the app quitting.
    tokio::select! {
        _ = upstream => {}
        _ = downstream => {}
    }
}
