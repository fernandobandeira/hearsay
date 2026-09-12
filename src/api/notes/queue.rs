//! One memo, filed once — whoever is still listening.
//!
//! Two failures live here, and they are the same failure seen from both ends.
//!
//! **The client goes away mid-transcription.** A phone locks its screen, iOS
//! backgrounds the PWA, the tunnel blips; hyper drops the request future and
//! every `.await` in the handler is cancelled. The transcription — minutes of it
//! on the 2-core box — keeps running because it is a blocking task, but nothing
//! is left to take its result, so the note is never written. The memo is still
//! in IndexedDB on the phone, which is the only thing that saved it. So the work
//! is **detached**: [`Job::spawn`] hands it to the runtime, not to the request,
//! and the handler merely *watches* for the result. A dropped request now costs
//! the response, never the note.
//!
//! **The retry that follows.** A note filed after the client vanished leaves the
//! reader still holding the recording — it never saw its 2xx — so it posts the
//! same memo again, and on a box where one memo is five minutes of CPU a second
//! transcription is not a rounding error. So a memo carries an identity
//! ([`memo_id`]) and this module remembers what that identity produced:
//!
//! - already filed → the same 2xx body, replayed off disk, nothing transcribed;
//! - being transcribed right now → attach to that job and take its result;
//! - neither → this caller owns the job, and reuses the recording a previous
//!   attempt already wrote rather than filling `notes-audio/` with copies.
//!
//! The record is on disk (`work/notes-idem/<id>.json`) rather than in memory
//! because a restart mid-transcription is exactly when a duplicate would be
//! filed. It is written in two steps, and the shape of the crash decides what
//! happens next: a record with no `filed` in it is only a claim on a recording
//! and never blocks — the memo is transcribed again, which is right, because
//! nothing was written to the vault. Only a `filed` record short-circuits, and
//! it is written *after* the note is in the vault.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// What filing a memo produced: the note in the vault, and the words in it.
/// This is the 2xx body's payload, and what a replay answers with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filed {
    pub file: String,
    pub text: String,
    pub language: String,
}

/// A failure, carried to everyone attached to the job rather than only to the
/// caller that happened to start it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failed {
    pub status: u16,
    pub message: String,
}

impl Failed {
    pub fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

pub type Outcome = Result<Filed, Failed>;

/// The durable half. `filed` absent means "a recording was claimed under this
/// id and nothing was written" — a state that must never be sticky.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Record {
    id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audio: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filed: Option<Filed>,
}

/// What a caller may do about one memo id.
pub enum Claim {
    /// Filed already — replay that body, transcribe nothing.
    Done(Filed),
    /// Someone else is on it; wait for their result instead of starting a second.
    Attach(Waiter),
    /// Nobody is; this caller owns the job.
    Mine(Job),
}

const ID_MAX: usize = 64;

/// The memo's identity.
///
/// A client-supplied id is preferred — it is stable across re-encodings and
/// across the reader's own retries — but it is a file name here, so it is
/// checked rather than trusted: anything with a separator, a control character
/// or a surprise in it is not repaired, it is *ignored*, and the recording's own
/// content hash stands in. Falling back rather than refusing is deliberate. A
/// 400 for a malformed id would strand a memo in the outbox forever, and the
/// hash is what today's clients and the Obsidian plugin — which send no id at
/// all — are deduplicated by anyway.
pub fn memo_id(supplied: Option<&str>, audio: &[u8]) -> String {
    match supplied.map(str::trim).filter(|s| is_safe_id(s)) {
        Some(id) => format!("c-{id}"),
        None => format!("h-{}", hash128(audio)),
    }
}

fn is_safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= ID_MAX
        && s != "."
        && s != ".."
        && s
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// FNV-1a over 128 bits, with the length appended.
///
/// A cryptographic digest would be the reflex, but the threat here is accident,
/// not forgery: two different recordings hashing alike would file one note for
/// two thoughts. 128 bits plus the byte count makes that not happen, and it
/// costs no dependency on a path that already holds a whole recording in memory.
fn hash128(bytes: &[u8]) -> String {
    const OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut h = OFFSET;
    for b in bytes {
        h ^= u128::from(*b);
        h = h.wrapping_mul(PRIME);
    }
    format!("{h:032x}-{:x}", bytes.len())
}

pub fn dir(work: &Path) -> PathBuf {
    work.join("notes-idem")
}

fn record_path(work: &Path, id: &str) -> PathBuf {
    dir(work).join(format!("{id}.json"))
}

fn audio_dir(work: &Path) -> PathBuf {
    work.join("notes-audio")
}

type Jobs = HashMap<PathBuf, watch::Sender<Option<Outcome>>>;

/// In-flight jobs, keyed by the record's path so two work directories in one
/// process (which is what the test suite is) can never see each other's.
fn jobs() -> &'static Mutex<Jobs> {
    static J: OnceLock<Mutex<Jobs>> = OnceLock::new();
    J.get_or_init(|| Mutex::new(HashMap::new()))
}

/// A poisoned registry must not take the note path down with it: a panic in
/// some other request is not a reason to stop filing memos.
fn lock_jobs() -> std::sync::MutexGuard<'static, Jobs> {
    match jobs().lock() {
        Ok(g) => g,
        Err(poisoned) => {
            tracing::warn!("note queue: registry lock was poisoned, carrying on");
            poisoned.into_inner()
        }
    }
}

/// Decide what to do about this memo, atomically.
///
/// The disk read happens under the registry lock on purpose: "is it filed", "is
/// someone on it" and "then it is mine" have to be one decision, or two POSTs
/// arriving together both start a transcription.
pub fn claim(work: &Path, id: &str) -> Claim {
    let key = record_path(work, id);
    let mut map = lock_jobs();
    if let Some(tx) = map.get(&key) {
        return Claim::Attach(Waiter(tx.subscribe()));
    }
    let rec = read_record(&key);
    if let Some(filed) = rec.as_ref().and_then(|r| r.filed.clone()) {
        return Claim::Done(filed);
    }
    let (tx, _) = watch::channel(None);
    map.insert(key.clone(), tx.clone());
    Claim::Mine(Job {
        key,
        work: work.to_path_buf(),
        id: id.to_string(),
        audio: rec.and_then(|r| r.audio),
        tx: Some(tx),
    })
}

fn read_record(path: &Path) -> Option<Record> {
    let raw = std::fs::read(path).ok()?;
    match serde_json::from_slice::<Record>(&raw) {
        Ok(r) => Some(r),
        Err(e) => {
            // A truncated or hand-mangled record is treated as no record: file
            // the memo again rather than let a bad file block it forever.
            tracing::warn!("note queue: unreadable record {}: {e}", path.display());
            None
        }
    }
}

/// Ownership of one memo id: the right to do the work, and the duty to say what
/// happened.
pub struct Job {
    key: PathBuf,
    work: PathBuf,
    id: String,
    audio: Option<String>,
    /// Taken by `finish`, so `Drop` can tell an abandoned job from a done one.
    tx: Option<watch::Sender<Option<Outcome>>>,
}

impl Job {
    /// The recording an earlier attempt already saved, if it is still on disk.
    /// Reusing it is what stops a retried memo writing a second `.webm`.
    pub fn audio(&self) -> Option<PathBuf> {
        let p = audio_dir(&self.work).join(self.audio.as_ref()?);
        p.exists().then_some(p)
    }

    /// Record which recording this memo's audio landed in, before anything that
    /// can fail. This is the "pending" half: it dedups the audio file and
    /// nothing else — it never stands in for a filed note.
    pub fn claim_audio(&mut self, path: &Path) {
        let name = path.file_name().map(|s| s.to_string_lossy().to_string());
        self.audio = name.clone();
        self.write(Record {
            id: self.id.clone(),
            audio: name,
            filed: None,
        });
    }

    /// Run `work` detached from whoever asked for it.
    ///
    /// The point of the whole module: the future goes to the runtime, so a
    /// request that is dropped mid-transcription loses its response and nothing
    /// else. The returned [`Waiter`] is subscribed before the task starts, so a
    /// job that finishes immediately is still seen.
    pub fn spawn<F>(mut self, work: F) -> Waiter
    where
        F: Future<Output = Outcome> + Send + 'static,
    {
        let rx = match self.tx.as_ref() {
            Some(tx) => tx.subscribe(),
            // Unreachable: `tx` is only taken by `finish`, which consumes the
            // job. Degrading beats a panic on the one path that must not lose a
            // memo.
            None => {
                let (_, rx) = watch::channel(Some(Err(Failed::new(
                    500,
                    "note job was already finished",
                ))));
                return Waiter(rx);
            }
        };
        tokio::spawn(async move {
            let out = work.await;
            self.finish(out);
        });
        Waiter(rx)
    }

    /// Publish the result: the durable record first, then the registry, then the
    /// waiters. That order is what makes the handover seamless — the filed
    /// record exists before the id stops being in flight, so a POST arriving in
    /// between replays it instead of starting again.
    fn finish(&mut self, out: Outcome) {
        if let Ok(filed) = &out {
            self.write(Record {
                id: self.id.clone(),
                audio: self.audio.clone(),
                filed: Some(filed.clone()),
            });
        }
        let tx = self.tx.take();
        lock_jobs().remove(&self.key);
        if let Some(tx) = tx {
            tx.send_replace(Some(out));
        }
    }

    fn write(&self, rec: Record) {
        if let Err(e) = write_record(&self.key, &rec) {
            // Not fatal: the worst case is a memo transcribed twice, which costs
            // CPU. Losing the note would cost the thought.
            tracing::warn!("note queue: could not write {}: {e}", self.key.display());
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // A job dropped without a result — a handler that returned early, a
        // panic in the task — must not leave the id claimed: the next POST would
        // attach to a job nobody is running and wait forever.
        if let Some(tx) = self.tx.take() {
            lock_jobs().remove(&self.key);
            tx.send_replace(Some(Err(Failed::new(
                500,
                "the note task ended without a result",
            ))));
        }
    }
}

/// Written to a `.part` and renamed, like every other record this server keeps:
/// a killed container must not leave half a record that reads as a filed note.
fn write_record(path: &Path, rec: &Record) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.part");
    let body = serde_json::to_vec(rec).map_err(std::io::Error::other)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// A handle on someone else's job — or on one's own, which is the same thing
/// once the work is detached.
pub struct Waiter(watch::Receiver<Option<Outcome>>);

impl Waiter {
    pub async fn wait(mut self) -> Outcome {
        match self.0.wait_for(|v| v.is_some()).await {
            Ok(v) => v.clone().unwrap_or_else(|| {
                Err(Failed::new(500, "the note task ended without a result"))
            }),
            // The sender is gone without a value. `Drop` makes this unreachable;
            // a 500 keeps the recording queued if it ever happens anyway.
            Err(_) => Err(Failed::new(500, "the note task ended without a result")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn filed(file: &str) -> Filed {
        Filed {
            file: file.into(),
            text: "a thought".into(),
            language: "en".into(),
        }
    }

    fn mine(c: Claim) -> Job {
        match c {
            Claim::Mine(j) => j,
            Claim::Done(_) => panic!("expected a job, got a replay"),
            Claim::Attach(_) => panic!("expected a job, got an attach"),
        }
    }

    #[test]
    fn a_client_id_is_used_when_it_is_a_safe_file_name_and_ignored_when_it_is_not() {
        let audio = b"some webm".as_slice();
        assert_eq!(memo_id(Some("9f3b-4c"), audio), "c-9f3b-4c");
        assert_eq!(memo_id(Some(" 9f3b-4c "), audio), "c-9f3b-4c");
        // Anything that is not a plain file name falls back to the hash rather
        // than being repaired into a neighbouring id.
        for bad in [
            "../../etc/passwd",
            "a/b",
            "a\\b",
            "",
            "  ",
            ".",
            "..",
            "id with spaces",
            "id\u{7}",
            &"x".repeat(65),
        ] {
            let id = memo_id(Some(bad), audio);
            assert!(id.starts_with("h-"), "{bad:?} -> {id}");
            assert!(!id.contains('/') && !id.contains('\\'), "{id}");
        }
    }

    #[test]
    fn the_hash_identifies_the_recording_itself() {
        // No id from the client — today's reader and the Obsidian plugin — still
        // dedups, because the bytes are the memo.
        assert_eq!(memo_id(None, b"aaa"), memo_id(None, b"aaa"));
        assert_ne!(memo_id(None, b"aaa"), memo_id(None, b"aab"));
        assert_ne!(memo_id(None, b"aaa"), memo_id(None, b"aaaa"));
        assert_ne!(memo_id(None, b""), memo_id(None, b"a"));
    }

    #[tokio::test]
    async fn a_filed_memo_replays_instead_of_transcribing_again() {
        let d = tempfile::tempdir().expect("tempdir");
        let id = memo_id(None, b"a recording");
        let job = mine(claim(d.path(), &id));
        let w = job.spawn(async { Ok(filed("202609112201 a thought.md")) });
        assert_eq!(w.wait().await, Ok(filed("202609112201 a thought.md")));

        // The retry the reader makes when it never saw the 2xx: same body, no
        // second job, and nothing to transcribe.
        match claim(d.path(), &id) {
            Claim::Done(f) => assert_eq!(f, filed("202609112201 a thought.md")),
            _ => panic!("a filed memo must replay"),
        }
    }

    #[tokio::test]
    async fn a_filed_record_outlives_the_process() {
        // The registry is memory; the record is not. A container restarted
        // between the note being written and the phone hearing about it must
        // still replay rather than file a second note.
        let d = tempfile::tempdir().expect("tempdir");
        let id = memo_id(Some("restart-me"), b"x");
        let job = mine(claim(d.path(), &id));
        let w = job.spawn(async { Ok(filed("note.md")) });
        w.wait().await.expect("filed");

        // What a restart is, from this module's point of view: an empty registry
        // over the same work directory.
        lock_jobs().clear();
        match claim(d.path(), &id) {
            Claim::Done(f) => assert_eq!(f.file, "note.md"),
            _ => panic!("the record is the durable half"),
        }
    }

    #[tokio::test]
    async fn a_second_post_attaches_to_the_running_job_rather_than_starting_one() {
        let d = tempfile::tempdir().expect("tempdir");
        let id = memo_id(None, b"one memo");
        let job = mine(claim(d.path(), &id));
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let first = job.spawn(async move {
            let _ = rx.await;
            Ok(filed("once.md"))
        });
        // While it runs, the id is taken: no second whisper run, no second note.
        let second = match claim(d.path(), &id) {
            Claim::Attach(w) => w,
            _ => panic!("an in-flight id must not start a second job"),
        };
        let _ = tx.send(());
        assert_eq!(first.wait().await, Ok(filed("once.md")));
        assert_eq!(second.wait().await, Ok(filed("once.md")));
    }

    #[tokio::test]
    async fn the_result_reaches_a_client_that_was_never_there_to_start_it() {
        // The bug this module exists for: the request future is dropped while the
        // job runs. The job still finishes, still records, and the next POST gets
        // the answer the first one never heard.
        let d = tempfile::tempdir().expect("tempdir");
        let (started, wait) = tokio::sync::oneshot::channel::<()>();
        let (release, held) = tokio::sync::oneshot::channel::<()>();
        let dir = d.path().to_path_buf();
        let handler = tokio::spawn(async move {
            let job = mine(claim(&dir, "h-locked"));
            let w = job.spawn(async move {
                let _ = started.send(());
                let _ = held.await;
                Ok(filed("survived.md"))
            });
            w.wait().await
        });
        wait.await.expect("the job started");
        handler.abort(); // the phone went away
        let _ = release.send(());
        for _ in 0..200 {
            if let Claim::Done(f) = claim(d.path(), "h-locked") {
                assert_eq!(f.file, "survived.md");
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("a dropped request must not cost the note");
    }

    #[tokio::test]
    async fn a_crash_shaped_record_never_blocks_the_memo() {
        // A container killed between claiming the recording and writing the note
        // leaves a record with no `filed` in it. Nothing was written to the
        // vault, so the memo must be transcribed again — and it must reuse the
        // recording that is already on disk rather than write a second copy.
        let d = tempfile::tempdir().expect("tempdir");
        let adir = audio_dir(d.path());
        std::fs::create_dir_all(&adir).expect("audio dir");
        let recording = adir.join("20260911220000.webm");
        std::fs::write(&recording, b"...").expect("recording");

        let mut job = mine(claim(d.path(), "c-crashed"));
        job.claim_audio(&recording);
        drop(job); // the process died here

        let again = mine(claim(d.path(), "c-crashed"));
        assert_eq!(
            again.audio(),
            Some(recording.clone()),
            "the recording is reused, not duplicated"
        );
        let w = again.spawn(async { Ok(filed("finally.md")) });
        assert_eq!(w.wait().await.map(|f| f.file), Ok("finally.md".into()));
        assert_eq!(
            std::fs::read_dir(&adir).map(|d| d.count()).unwrap_or(0),
            1,
            "one recording, however many attempts"
        );
    }

    #[tokio::test]
    async fn a_recording_that_is_gone_is_not_offered_for_reuse() {
        let d = tempfile::tempdir().expect("tempdir");
        let mut job = mine(claim(d.path(), "c-missing"));
        job.claim_audio(&audio_dir(d.path()).join("20260911220000.webm"));
        drop(job);
        assert_eq!(mine(claim(d.path(), "c-missing")).audio(), None);
    }

    #[tokio::test]
    async fn an_abandoned_job_frees_its_id_and_never_hangs_a_waiter() {
        // A handler that returns 404 before it spawns anything, or a task that
        // panics: whoever is attached hears a failure, and the id is claimable
        // again rather than stuck in flight.
        let d = tempfile::tempdir().expect("tempdir");
        let job = mine(claim(d.path(), "c-abandoned"));
        let waiting = match claim(d.path(), "c-abandoned") {
            Claim::Attach(w) => w,
            _ => panic!("expected an attach"),
        };
        drop(job);
        let out = waiting.wait().await;
        assert_eq!(out.err().map(|f| f.status), Some(500));
        assert!(matches!(claim(d.path(), "c-abandoned"), Claim::Mine(_)));
    }

    #[tokio::test]
    async fn a_failure_is_not_recorded_and_is_tried_again() {
        // Only a filed note short-circuits. A 500 from whisper leaves the memo
        // exactly where it was — in the outbox, and claimable here.
        let d = tempfile::tempdir().expect("tempdir");
        let job = mine(claim(d.path(), "c-failed"));
        let w = job.spawn(async { Err(Failed::new(500, "transcription failed")) });
        assert_eq!(w.wait().await.err().map(|f| f.status), Some(500));
        assert!(matches!(claim(d.path(), "c-failed"), Claim::Mine(_)));
    }
}
