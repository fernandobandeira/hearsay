//! Live updates: one broadcast channel, many Server-Sent Events streams.
//!
//! A port of `app/events.py`, with tokio's broadcast channel doing what the
//! per-subscriber bounded queues did there. The three facts that shaped the
//! python design hold here too:
//!
//! * **A dead client must not stall the renderer.** `broadcast::Sender::send`
//!   never blocks and never fails on a slow receiver; a receiver that falls
//!   behind gets `RecvError::Lagged` and is told how many it missed. These
//!   events all mean "something changed, go look", so a drop is invisible — the
//!   client refetches anyway.
//! * **No replay.** `Last-Event-ID` is accepted and ignored, on purpose.
//! * **The renderer is not async.** `send` is callable from the render thread
//!   with no runtime handle at all, which is why the bus is a broadcast channel
//!   and not a set of `mpsc` senders owned by the loop.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::Value;
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub struct Event {
    pub id: u64,
    pub name: &'static str,
    pub data: Value,
}

/// One SSE event. `serde_json` cannot emit a raw newline inside a string, so
/// `data:` is always exactly one line and no escaping dance is needed.
pub fn sse(name: &str, data: &Value, eid: Option<u64>) -> String {
    let head = eid.map(|i| format!("id: {i}\n")).unwrap_or_default();
    format!("{head}event: {name}\ndata: {data}\n\n")
}

/// A comment line: ignored by `EventSource`, but still bytes on the wire, which
/// is the whole point — it proves the connection and resets idle timers.
pub fn comment(text: &str) -> String {
    format!(": {text}\n\n")
}

pub fn retry(ms: u64) -> String {
    format!("retry: {ms}\n\n")
}

pub struct Bus {
    tx: broadcast::Sender<Event>,
    seq: AtomicU64,
    throttle: Mutex<HashMap<String, Instant>>,
    render_min: Duration,
}

impl Bus {
    pub fn new(capacity: usize, render_min_s: f64) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self {
            tx,
            seq: AtomicU64::new(0),
            throttle: Mutex::new(HashMap::new()),
            render_min: Duration::from_secs_f64(render_min_s.max(0.0)),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    pub fn subscribers(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Queue an event for every open stream. Safe from any thread, and free when
    /// nobody is listening.
    pub fn emit(&self, name: &'static str, data: Value) -> bool {
        if self.tx.receiver_count() == 0 {
            return false;
        }
        let id = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        self.tx.send(Event { id, name, data }).is_ok()
    }

    /// Render progress, coalesced. `progress` fires per chunk — several a second
    /// on a fast box — and a UI cannot use more than about one repaint a second.
    /// The edges (a chapter change, a chapter finishing, an m4a landing) are rare
    /// and are exactly what a watching UI is waiting for, so they never drop.
    pub fn emit_render(&self, kind: &str, mut data: Value) -> bool {
        if kind == "progress" && !self.ready("progress") {
            return false;
        }
        if let Some(o) = data.as_object_mut() {
            o.insert("kind".into(), Value::String(kind.into()));
        }
        self.emit("render", data)
    }

    fn ready(&self, key: &str) -> bool {
        let Ok(mut g) = self.throttle.lock() else {
            return true;
        };
        let now = Instant::now();
        match g.get(key) {
            Some(last) if now.duration_since(*last) < self.render_min => false,
            _ => {
                g.insert(key.to_string(), now);
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_wire_format_is_the_python_one() {
        assert_eq!(
            sse("position", &json!({"book": "a.epub"}), Some(7)),
            "id: 7\nevent: position\ndata: {\"book\":\"a.epub\"}\n\n"
        );
        assert_eq!(comment("ping"), ": ping\n\n");
        assert_eq!(retry(3000), "retry: 3000\n\n");
    }

    #[tokio::test]
    async fn progress_is_coalesced_but_edges_are_not() {
        let bus = Bus::new(16, 60.0);
        let _rx = bus.subscribe();
        assert!(bus.emit_render("progress", json!({"chapter": 0})));
        assert!(!bus.emit_render("progress", json!({"chapter": 0})));
        assert!(bus.emit_render("complete", json!({"chapter": 0})));
        assert!(bus.emit_render("packed", json!({"chapter": 0})));
    }

    #[tokio::test]
    async fn emitting_with_nobody_listening_costs_nothing() {
        let bus = Bus::new(16, 0.0);
        assert!(!bus.emit("note", json!({})));
    }
}
