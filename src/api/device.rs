//! Who is asking.
//!
//! Until this existed, nothing in the server or the reader could name a device.
//! That sounds like a missing nicety and was actually the root of the bug this
//! round is about: with one global session, one position record per book and no
//! identity on either, the reader could not tell its own echo from another
//! device's move, and approximated it with a two-chunk slack. Two chunks is a
//! guess about *distance* standing in for a question about *identity*, and it
//! answers wrong in both directions — a real move of one chunk on the laptop is
//! swallowed, and a laptop reconnecting three chapters behind is followed.
//!
//! So a device says who it is, on every request, in a header:
//!
//! ```text
//! X-Narrator-Device:      2f8a1c94-...   a uuid the reader mints once and keeps
//! X-Narrator-Device-Name: iPhone          a label, for a human reading a list
//! ```
//!
//! **A header rather than a body field**, which is the one design decision here
//! worth defending. `?book=` is a body/query field because it is part of what a
//! request *means*; a device id is part of who *sent* it, and it has to ride on
//! requests that have no body to put it in — `/api/pause`, `/api/chapters`, and
//! above all `GET /api/events`, which is how the server learns which devices are
//! connected *right now*. That last one is not a nicety either: "render ahead on
//! the book somebody is actually reading" needs to know who is actually reading.
//!
//! **And a query parameter as well, for exactly one caller.** `EventSource` —
//! the browser API that consumes `/api/events`, and the reason the reader gets
//! reconnection for free — has no way to set a request header. None. So the one
//! endpoint that most needs to know who is connected is the one endpoint that
//! cannot be told in the ordinary way, and `?device=` is the only mechanism
//! left. The extractor therefore reads the header first and falls back to the
//! query, which keeps it one concept with two spellings rather than two
//! concepts. A device id in a URL does land in an access log; it is a random
//! uuid rather than a credential, it grants nothing, and the alternative is not
//! knowing which devices are live.
//!
//! **Additive, like everything else added here.** A client that sends no header
//! is the legacy device, whose id is the empty string — the Obsidian plugin, the
//! python-era reader, `curl`. Every table keyed by device therefore has a row for
//! `""`, and the behaviour a client gets by sending nothing is exactly the
//! behaviour it got before this existed.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use std::convert::Infallible;

pub const DEVICE_HEADER: &str = "x-narrator-device";
pub const DEVICE_NAME_HEADER: &str = "x-narrator-device-name";

/// The longest id accepted. A uuid is 36 characters; the slack is for a reader
/// that wants a prefix of its own, and the cap is because this string becomes a
/// SQLite primary key and is echoed to every other connected device.
const MAX_ID: usize = 64;
/// The longest label kept. It is shown in a list, never matched on.
const MAX_NAME: usize = 40;

/// The device behind a request.
///
/// `id` is empty for a client that sent no header, and that is a legitimate
/// value rather than an error: see the module doc. Nothing may refuse a request
/// for want of an id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Device {
    pub id: String,
    pub name: String,
}

impl Device {
    /// The legacy client: every reader that predates this header, plus the
    /// Obsidian plugin. Named rather than written as `Device::default()` at the
    /// call sites, because "this is the anonymous device" is a statement and
    /// `default()` is an accident.
    pub fn legacy() -> Self {
        Self::default()
    }

    /// Is this a device that actually told us who it is?
    ///
    /// The distinction matters exactly once, in the reader's own-echo test: an
    /// event carrying an id that equals mine is certainly mine, and an event
    /// carrying no id tells me nothing, so the chunk-distance backstop is what
    /// has to answer for it.
    pub fn known(&self) -> bool {
        !self.id.is_empty()
    }
}

/// Keep an id to characters that are safe in a primary key, a JSON payload and a
/// log line, and drop everything else.
///
/// Dropping rather than rejecting is deliberate: a device that sends a slightly
/// wrong id should be treated as a device, not refused. The worst case is two
/// readers that sanitise to the same id, which is the same as today's behaviour
/// with no ids at all.
fn clean_id(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(MAX_ID)
        .collect()
}

/// A label is for human eyes, so the rule is only "nothing that breaks a line".
fn clean_name(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .take(MAX_NAME)
        .collect::<String>()
        .trim()
        .to_string()
}

/// The query spellings, for `EventSource` and nothing else.
pub const DEVICE_QUERY: &str = "device";
pub const DEVICE_NAME_QUERY: &str = "device_name";

/// Pull one key out of a raw query string, percent-decoded.
///
/// Hand-rolled rather than `serde_urlencoded`, because this has to run on the
/// query of *any* request — `/api/events` already carries none, but a future one
/// might carry keys this does not model, and a strict deserialiser would fail
/// the whole parse and lose the id over a field it did not expect.
fn query_value(query: &str, key: &str) -> String {
    for pair in query.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if k == key {
            return percent_encoding::percent_decode_str(v)
                .decode_utf8_lossy()
                .replace('+', " ");
        }
    }
    String::new()
}

impl Device {
    /// Read the pair out of whatever headers a request carried.
    pub fn from_headers(h: &axum::http::HeaderMap) -> Self {
        Self::from_parts(h, None)
    }

    /// The header, or the query when there is no header. See the module doc for
    /// why the second source exists at all.
    pub fn from_parts(h: &axum::http::HeaderMap, query: Option<&str>) -> Self {
        let head = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).unwrap_or("");
        let q = query.unwrap_or("");
        let id = match clean_id(head(DEVICE_HEADER)) {
            s if !s.is_empty() => s,
            _ => clean_id(&query_value(q, DEVICE_QUERY)),
        };
        let name = match clean_name(head(DEVICE_NAME_HEADER)) {
            s if !s.is_empty() => s,
            _ => clean_name(&query_value(q, DEVICE_NAME_QUERY)),
        };
        Device { id, name }
    }
}

/// Infallible on purpose. A device id is metadata about the sender, and a
/// malformed one must never turn a reader's playhead report into a 400 — the
/// report is the thing that matters and the id is the decoration.
impl<S: Send + Sync> FromRequestParts<S> for Device {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Device::from_parts(&parts.headers, parts.uri.query()))
    }
}

// ------------------------------------------------------------------ presence
//
// Who is *here*, as opposed to who exists.
//
// This is deliberately **not** in the store, and the distinction is the same one
// the whole database rests on: `state.db` holds durable intent — what somebody
// asked for, where they got to — and presence is neither. It is true only for as
// long as a socket is open, it is meaningless after a restart, and writing it
// down would create a second class of fact that looks durable and lies. A
// process that comes back up with nobody connected is correct; a process that
// comes back up believing three devices are listening is the `render_idx` bug
// wearing a different hat.
//
// What reads it is the scheduler: "render ahead on the book somebody is actually
// reading" is a question about who is connected now, not about who once was.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// One connected device, for as long as its event stream is open.
#[derive(Debug, Clone)]
pub struct Presence {
    pub name: String,
    pub since: Instant,
    pub last_seen: Instant,
    /// The book this device last said it was on, by file name.
    pub book: Option<String>,
}

/// Everyone currently holding an `/api/events` stream.
#[derive(Default)]
pub struct Roster(Mutex<HashMap<String, Presence>>);

impl Roster {
    fn with<T>(&self, f: impl FnOnce(&mut HashMap<String, Presence>) -> T) -> Option<T> {
        match self.0.lock() {
            Ok(mut g) => Some(f(&mut g)),
            Err(p) => {
                tracing::error!("presence lock poisoned; continuing with its contents");
                Some(f(&mut p.into_inner()))
            }
        }
    }

    /// Note that a device did something. Called from every request that carries
    /// an id, so a device that never opens a stream — the Obsidian plugin, a
    /// script — is still visible while it is working.
    pub fn touch(&self, dev: &Device, book: Option<String>) {
        if !dev.known() {
            return;
        }
        let now = Instant::now();
        self.with(|m| {
            let e = m.entry(dev.id.clone()).or_insert_with(|| Presence {
                name: dev.name.clone(),
                since: now,
                last_seen: now,
                book: book.clone(),
            });
            e.last_seen = now;
            if !dev.name.is_empty() {
                e.name = dev.name.clone();
            }
            if book.is_some() {
                e.book = book;
            }
        });
    }

    pub fn drop_device(&self, id: &str) {
        self.with(|m| m.remove(id));
    }

    /// Everyone seen inside `window`, newest first. Anything older is treated as
    /// gone even if its socket is technically still open: a phone in a pocket on
    /// a dead tunnel holds a TCP connection for a long time after it has stopped
    /// being a reader.
    pub fn active(&self, window: std::time::Duration) -> Vec<(String, Presence)> {
        let mut v = self
            .with(|m| {
                m.iter()
                    .filter(|(_, p)| p.last_seen.elapsed() <= window)
                    .map(|(k, p)| (k.clone(), p.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        v.sort_by_key(|p| std::cmp::Reverse(p.1.last_seen));
        v
    }

    pub fn len(&self) -> usize {
        self.with(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Holds a device on the roster for the life of its event stream.
///
/// RAII and nothing else, for the same reason the STT gate is: a stream can end
/// by being read to completion, by the client vanishing, or by the task being
/// dropped mid-poll when a phone locks, and only a destructor covers all three.
/// A manual `drop_device` at the end of the handler would be skipped by exactly
/// the case that matters most.
pub struct Connected {
    roster: Arc<Roster>,
    id: String,
}

impl Connected {
    pub fn new(roster: Arc<Roster>, dev: &Device, book: Option<String>) -> Option<Self> {
        if !dev.known() {
            return None;
        }
        roster.touch(dev, book);
        Some(Self {
            roster,
            id: dev.id.clone(),
        })
    }
}

impl Drop for Connected {
    fn drop(&mut self) {
        self.roster.drop_device(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderName, HeaderValue};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            let name = HeaderName::from_bytes(k.as_bytes()).expect("header name");
            h.insert(name, HeaderValue::from_str(v).expect("header value"));
        }
        h
    }

    #[test]
    fn no_headers_is_the_legacy_device_not_an_error() {
        let d = Device::from_headers(&HeaderMap::new());
        assert_eq!(d, Device::legacy());
        assert!(!d.known());
    }

    #[test]
    fn a_uuid_survives_intact() {
        let id = "2f8a1c94-7b3d-4e21-9c55-0a1b2c3d4e5f";
        let d = Device::from_headers(&headers(&[(DEVICE_HEADER, id)]));
        assert_eq!(d.id, id);
        assert!(d.known());
    }

    #[test]
    fn an_id_is_cleaned_rather_than_refused() {
        // Quotes, spaces and a newline-ish control character all go; what is
        // left is still a usable identity, which is the point.
        let d = Device::from_headers(&headers(&[(DEVICE_HEADER, "ab c\"d/e.f-1_2")]));
        assert_eq!(d.id, "abcdef-1_2");
        assert!(d.known());
    }

    #[test]
    fn an_absurd_id_is_truncated_not_trusted() {
        let long = "a".repeat(500);
        let d = Device::from_headers(&headers(&[(DEVICE_HEADER, &long)]));
        assert_eq!(d.id.len(), MAX_ID);
    }

    #[test]
    fn an_id_that_cleans_to_nothing_is_the_legacy_device() {
        let d = Device::from_headers(&headers(&[(DEVICE_HEADER, "!!!///")]));
        assert!(!d.known());
        assert_eq!(d.id, "");
    }

    #[test]
    fn a_name_keeps_its_spaces_and_loses_its_edges() {
        let d = Device::from_headers(&headers(&[
            (DEVICE_HEADER, "x1"),
            (DEVICE_NAME_HEADER, "  Fernando's iPhone  "),
        ]));
        assert_eq!(d.name, "Fernando's iPhone");
    }

    #[test]
    fn a_name_is_capped() {
        let d = Device::from_headers(&headers(&[(DEVICE_NAME_HEADER, &"n".repeat(200))]));
        assert_eq!(d.name.len(), MAX_NAME);
    }

    // -------------------------------------------------- the EventSource route

    #[test]
    fn the_query_answers_when_there_is_no_header() {
        let d = Device::from_parts(&HeaderMap::new(), Some("device=abc123&device_name=iPhone"));
        assert_eq!(d.id, "abc123");
        assert_eq!(d.name, "iPhone");
    }

    #[test]
    fn a_header_beats_a_query() {
        let d = Device::from_parts(
            &headers(&[(DEVICE_HEADER, "fromheader")]),
            Some("device=fromquery"),
        );
        assert_eq!(d.id, "fromheader");
    }

    #[test]
    fn a_percent_encoded_name_comes_back_whole() {
        let d = Device::from_parts(
            &HeaderMap::new(),
            Some("device=x&device_name=Fernando%27s%20iPad"),
        );
        assert_eq!(d.name, "Fernando's iPad");
    }

    #[test]
    fn an_unmodelled_query_key_does_not_lose_the_id() {
        // The whole reason this is not a serde deserialiser: `/api/events` is
        // free to grow parameters, and one this does not know must not cost the
        // device its identity.
        let d = Device::from_parts(&HeaderMap::new(), Some("since=7&device=keepme&weird"));
        assert_eq!(d.id, "keepme");
    }

    #[test]
    fn no_query_at_all_is_the_legacy_device() {
        assert!(!Device::from_parts(&HeaderMap::new(), None).known());
    }

    // ------------------------------------------------------------- the roster

    fn dev(id: &str) -> Device {
        Device {
            id: id.into(),
            name: "iPhone".into(),
        }
    }

    const MINUTE: std::time::Duration = std::time::Duration::from_secs(60);

    #[test]
    fn an_anonymous_device_is_never_on_the_roster() {
        // It cannot be taken *off* it either, so putting it on would leave a
        // phantom reader the scheduler would render for forever.
        let r = Roster::default();
        r.touch(&Device::legacy(), Some("b.epub".into()));
        assert!(r.is_empty());
        assert!(Connected::new(Arc::new(Roster::default()), &Device::legacy(), None).is_none());
    }

    #[test]
    fn a_stream_holds_a_device_and_dropping_it_lets_go() {
        let r = Arc::new(Roster::default());
        {
            let _c = Connected::new(r.clone(), &dev("a"), Some("book.epub".into()));
            assert_eq!(r.len(), 1);
            assert_eq!(r.active(MINUTE)[0].1.book.as_deref(), Some("book.epub"));
        }
        assert!(
            r.is_empty(),
            "the guard's Drop is the only way off the roster"
        );
    }

    #[test]
    fn touching_keeps_the_book_when_a_later_call_does_not_name_one() {
        // /api/status carries a device and no book; it must not blank what
        // /api/open established.
        let r = Roster::default();
        r.touch(&dev("a"), Some("book.epub".into()));
        r.touch(&dev("a"), None);
        assert_eq!(r.active(MINUTE)[0].1.book.as_deref(), Some("book.epub"));
    }

    #[test]
    fn active_is_newest_first_and_forgets_nobody_inside_the_window() {
        let r = Roster::default();
        r.touch(&dev("a"), None);
        r.touch(&dev("b"), None);
        let a = r.active(MINUTE);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].0, "b", "b was touched last");
    }

    #[test]
    fn a_window_of_nothing_hides_everyone() {
        let r = Roster::default();
        r.touch(&dev("a"), None);
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(r.active(std::time::Duration::from_millis(1)).is_empty());
        assert_eq!(r.len(), 1, "still connected, just not recently heard from");
    }
}
