//! Local IPC server (newline-delimited JSON) with event broadcast.
//!
//! Transport: a stream socket, either AF_UNIX (default at runtime) or AF_INET (tests).
//! Protocol: one JSON message per line (UTF-8).
//!
//! Request  (client -> server): {"id": <int|null>, "method": <str>, "params": {...}}
//! Response (server -> client): {"id": <int>, "ok": true, "result": {...}}
//!                              {"id": <int>, "ok": false, "error": {"message": ...}}
//! Event    (server -> client): {"event": <str>, "data": {...}}
//!
//! This mirrors the client's `ipc.py` wire format exactly, so the Python client works
//! unchanged against this server.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::Result;
use serde_json::{json, Map, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::mpsc;

/// Default path of the daemon socket (XDG_RUNTIME_DIR, else /tmp).
pub fn default_socket_path() -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/tmp".to_string());
    PathBuf::from(base).join("silex-daemon.sock")
}

#[derive(Debug, Clone)]
pub enum IpcAddr {
    Unix(PathBuf),
    Inet(String, u16),
}

type BoxFuture = Pin<Box<dyn Future<Output = Result<Value>> + Send>>;
/// Request handler: `(method, params, peer) -> result`. Errors become error responses.
pub type Handler = Arc<dyn Fn(String, Value, Arc<Peer>) -> BoxFuture + Send + Sync>;

fn encode(msg: &Value) -> Vec<u8> {
    let mut b = serde_json::to_vec(msg).unwrap_or_default();
    b.push(b'\n');
    b
}

/// A connected client on the server side.
pub struct Peer {
    tx: mpsc::UnboundedSender<Vec<u8>>,
    subscribed: AtomicBool,
}

impl Peer {
    pub fn subscribe(&self) {
        self.subscribed.store(true, Ordering::Relaxed);
    }

    pub fn is_subscribed(&self) -> bool {
        self.subscribed.load(Ordering::Relaxed)
    }

    pub fn send_event(&self, event: &str, data: Value) {
        let _ = self.tx.send(encode(&json!({"event": event, "data": data})));
    }

    fn send_msg(&self, v: &Value) {
        let _ = self.tx.send(encode(v));
    }
}

pub struct IpcServer {
    addr: IpcAddr,
    handler: Handler,
    peers: Mutex<HashMap<u64, Arc<Peer>>>,
    next_id: AtomicU64,
    bound: Mutex<Option<SocketAddr>>,
}

impl IpcServer {
    pub fn new(addr: IpcAddr, handler: Handler) -> Self {
        Self {
            addr,
            handler,
            peers: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            bound: Mutex::new(None),
        }
    }

    /// For AF_INET tests: the actually bound port (after `serve` has bound).
    #[cfg(test)]
    pub fn bound_port(&self) -> Option<u16> {
        self.bound.lock().unwrap().map(|a| a.port())
    }

    /// Send an event to all (optionally only subscribed) connections.
    pub fn broadcast(&self, event: &str, data: Value, only_subscribed: bool) {
        let peers: Vec<Arc<Peer>> = self.peers.lock().unwrap().values().cloned().collect();
        for p in peers {
            if only_subscribed && !p.is_subscribed() {
                continue;
            }
            p.send_event(event, data.clone());
        }
    }

    pub async fn serve(self: Arc<Self>) -> Result<()> {
        match self.addr.clone() {
            IpcAddr::Unix(path) => {
                let _ = std::fs::remove_file(&path);
                let listener = UnixListener::bind(&path)?;
                loop {
                    let (stream, _addr) = listener.accept().await?;
                    let me = self.clone();
                    tokio::spawn(async move { me.serve_conn(stream).await });
                }
            }
            IpcAddr::Inet(host, port) => {
                let listener = TcpListener::bind((host.as_str(), port)).await?;
                *self.bound.lock().unwrap() = listener.local_addr().ok();
                loop {
                    let (stream, _addr) = listener.accept().await?;
                    let me = self.clone();
                    tokio::spawn(async move { me.serve_conn(stream).await });
                }
            }
        }
    }

    async fn serve_conn<S>(self: Arc<Self>, stream: S)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + 'static,
    {
        let (mut rd, mut wr) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let peer = Arc::new(Peer { tx, subscribed: AtomicBool::new(false) });
        self.peers.lock().unwrap().insert(id, peer.clone());

        let writer = tokio::spawn(async move {
            while let Some(bytes) = rx.recv().await {
                if wr.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        });

        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 65536];
        loop {
            let n = match rd.read(&mut tmp).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            buf.extend_from_slice(&tmp[..n]);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = buf.drain(..=pos).collect();
                let trimmed = &line[..line.len() - 1];
                if trimmed.iter().all(|b| b.is_ascii_whitespace()) {
                    continue;
                }
                if let Ok(msg) = serde_json::from_slice::<Value>(trimmed) {
                    let me = self.clone();
                    let peer = peer.clone();
                    // Dispatch concurrently so a slow request (e.g. connect) does not
                    // block other requests or events on the same connection.
                    tokio::spawn(async move { me.dispatch(msg, peer).await });
                }
            }
        }

        self.peers.lock().unwrap().remove(&id);
        writer.abort();
    }

    async fn dispatch(self: Arc<Self>, msg: Value, peer: Arc<Peer>) {
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let params = msg.get("params").cloned().unwrap_or_else(|| Value::Object(Map::new()));
        let result = (self.handler)(method, params, peer.clone()).await;
        if id.is_null() {
            return; // notification: no response expected
        }
        match result {
            Ok(r) => peer.send_msg(&json!({"id": id, "ok": true, "result": r})),
            Err(e) => peer.send_msg(&json!({"id": id, "ok": false, "error": {"message": e.to_string()}})),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpStream;

    fn echo_handler() -> Handler {
        Arc::new(|method: String, params: Value, peer: Arc<Peer>| {
            Box::pin(async move {
                match method.as_str() {
                    "echo" => Ok(params),
                    "boom" => Err(anyhow::anyhow!("broken")),
                    "sub" => {
                        peer.subscribe();
                        Ok(json!({"ok": true}))
                    }
                    _ => Err(anyhow::anyhow!("unknown")),
                }
            })
        })
    }

    async fn start_server() -> (Arc<IpcServer>, u16) {
        let server = Arc::new(IpcServer::new(IpcAddr::Inet("127.0.0.1".into(), 0), echo_handler()));
        let s2 = server.clone();
        tokio::spawn(async move { let _ = s2.serve().await; });
        let port = loop {
            if let Some(p) = server.bound_port() {
                break p;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        (server, port)
    }

    #[tokio::test]
    async fn request_response_error_and_broadcast() {
        let (server, port) = start_server().await;
        let stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let (rd, mut wr) = stream.into_split();
        let mut lines = BufReader::new(rd).lines();

        // echo
        wr.write_all(b"{\"id\":1,\"method\":\"echo\",\"params\":{\"a\":1}}\n").await.unwrap();
        let resp: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(resp["ok"], json!(true));
        assert_eq!(resp["result"], json!({"a": 1}));

        // error
        wr.write_all(b"{\"id\":2,\"method\":\"boom\"}\n").await.unwrap();
        let resp: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(resp["ok"], json!(false));
        assert_eq!(resp["error"]["message"], json!("broken"));

        // subscribe + broadcast
        wr.write_all(b"{\"id\":3,\"method\":\"sub\"}\n").await.unwrap();
        let _ = lines.next_line().await.unwrap().unwrap();
        server.broadcast("tick", json!({"n": 1}), true);
        let ev: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(ev["event"], json!("tick"));
        assert_eq!(ev["data"], json!({"n": 1}));
    }
}
