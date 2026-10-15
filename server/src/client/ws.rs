//! WebSocket connection task with reconnect. Delivers raw y-sync payloads to the engine
//! and accepts payloads to send through a per-connection channel.

use std::time::Duration;

use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

pub enum WsEvent {
    Connected(mpsc::UnboundedSender<Vec<u8>>),
    Message(Vec<u8>),
    Disconnected,
    /// The server refused us for good (bad token, revoked ticket); retrying is pointless.
    Fatal(String),
}

pub fn spawn(url: String, events: mpsc::UnboundedSender<WsEvent>) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        loop {
            match tokio_tungstenite::connect_async(&url).await {
                Ok((stream, _)) => {
                    backoff = Duration::from_secs(1);
                    let (mut sink, mut source) = stream.split();
                    let (tx, mut rx) = mpsc::unbounded_channel::<Vec<u8>>();
                    if events.send(WsEvent::Connected(tx)).is_err() {
                        return;
                    }
                    loop {
                        tokio::select! {
                            out = rx.recv() => match out {
                                Some(bytes) => {
                                    if sink.send(Message::Binary(bytes.into())).await.is_err() { break; }
                                }
                                None => break,
                            },
                            inc = source.next() => match inc {
                                Some(Ok(Message::Binary(b))) => {
                                    if events.send(WsEvent::Message(b.to_vec())).is_err() { return; }
                                }
                                Some(Ok(Message::Ping(p))) => { let _ = sink.send(Message::Pong(p)).await; }
                                Some(Ok(Message::Close(_))) | None => break,
                                Some(Ok(_)) => {}
                                Some(Err(e)) => { tracing::debug!("ws error: {e}"); break; }
                            }
                        }
                    }
                    if events.send(WsEvent::Disconnected).is_err() {
                        return;
                    }
                    tracing::warn!("disconnected, reconnecting in {backoff:?}");
                }
                Err(tokio_tungstenite::tungstenite::Error::Http(resp)) if matches!(resp.status().as_u16(), 401 | 403) => {
                    let _ = events.send(WsEvent::Fatal(format!("server refused the connection (HTTP {})", resp.status())));
                    return;
                }
                Err(e) => {
                    let shown = url.split('?').next().unwrap_or(&url);
                    tracing::warn!("connect to {shown} failed: {e}; retrying in {backoff:?}");
                }
            }
            tokio::time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    });
}
