//! The memo queue: one note per memo, written whatever happens to anything else.
//!
//! Saving the note is the most important work this server does. A recording
//! lives in IndexedDB on a phone and nowhere else until `POST /api/note` answers
//! with the note it filed, and transcription here is minutes long — twenty of
//! them is fine, latency is not the point — so the window in which something can
//! go wrong is enormous. Three things go wrong, and this module is the answer to
//! all three.
//!
//! **The client goes away mid-transcription.** A phone locks its screen, iOS
//! backgrounds the PWA, the tunnel blips; hyper drops the request future and
//! every `.await` in the handler is cancelled. The blocking transcription keeps
//! running — and nothing is left to take its result, so the note is never
//! written. So the work is **detached**: [`Job::spawn`] hands it to the runtime,
//! not to the request, and the handler merely *watches* for the result. A dropped
//! request now costs the response, never the note.
//!
//! **The process goes away mid-transcription.** A deploy, the watchdog, someone
//! restarting the unit: a detached task dies with the process exactly like the
//! request did, and the recording on disk would be all that was left. So the
//! record on disk is a **work queue**, not just a receipt — it carries everything
//! needed to write the note without the client (which passage, which book, which
//! words around it), and [`unfiled`] is swept at startup so an interrupted memo
//! resumes by itself. A memo that reached this server once becomes a note even if
//! the phone never comes back.
//!
//! **The phone retries, because it never saw its 2xx.** Which it must — that is
//! the delivery contract, and the second copy is the point. But a retry must cost
//! nothing: a memo already filed is answered with the note it produced, a memo
//! being transcribed right now is attached to rather than started again, and the
//! recording an earlier attempt saved is reused instead of copied.
//!
//! The record (`work/notes-idem/<id>.json`) is written before the recording it
//! describes and updated as the memo moves, every write `.part`-then-renamed and
//! fsynced, so a crash can leave a stale record but never a half-written one and
//! never a truncated recording that a later sweep would feed to whisper. The
//! shape of a crash decides what happens next: a record with no `filed` in it is
//! work still owed and never blocks anything; only `filed` short-circuits, and it
//! is written *after* the note is in the vault.

use std::collections::HashMap;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// How many times the startup sweep will take a memo on before leaving it alone.
///
/// Not a give-up: the recording and its record stay on disk, a POST from the
/// reader still retries it, and every startup says so in the log. It exists
/// because a recording that cannot be decoded at all would otherwise spend a core
/// on every restart forever, and this box restarts on a timer.
pub const MAX_RESUMES: u32 = 5;

/// What filing a memo produced: the note in the vault, and the words in it.
/// This is the 2xx body's payload, and what a replay answers with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Filed {
    pub file: String,
    pub text: String,
    pub language: String,
}

/// Everything needed to write the note, minus the recording itself.
///
/// Stored because the client is not required to come back: with this on disk, a
/// process that starts up and finds an unfiled memo can finish the job alone, and
/// the note it writes is the note the first attempt would have written — same
/// stamp, so the same file name and the same `captured` in the frontmatter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    /// When the memo arrived, in milliseconds since the epoch.
    pub stamp_ms: i64,
    pub prompt: String,
    pub title: String,
    pub name: String,
    pub ctitle: String,
    pub ctx: String,
    pub ci: usize,
    pub i: usize,
    pub n: usize,
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

/// The durable half.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Record {
    id: String,
    /// The recording's file name under `notes-audio/`. Present means "these bytes
    /// are complete and are this memo's".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    audio: Option<String>,
    /// Work still owed: what the note needs to say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pending: Option<Pending>,
    /// How many times the startup sweep has taken this memo on.
    #[serde(default)]
    resumes: u32,
    /// Written last, after the note is in the vault. The only field that ends a
    /// memo's life.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    filed: Option<Filed>,
}

/// What a caller may do about one memo id.
pub enum Claim {
    /// Filed already — replay that body, transcribe nothing.
    Done(Filed),
    /// Someone else is on it; wait for their result instead of starting a second.
    Attach(Waiter),
    /// Nobody is; this caller owns the job. Boxed because a `Job` carries the
    /// whole work order and the other two variants are a word or two.
    Mine(Box<Job>),
}

/// A memo this server owes the vault: the recording is on disk, the note is not.
pub struct Unfiled {
    pub id: String,
    pub audio: PathBuf,
    pub pending: Pending,
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
    match supplied.and_then(client_id) {
        Some(id) => id,
        None => format!("h-{}", hash128(audio)),
    }
}

/// The id a client sent, if it is one this server will use as a file name.
/// `None` means "identify this memo by its bytes instead".
pub fn client_id(supplied: &str) -> Option<String> {
    let s = supplied.trim();
    is_safe_id(s).then(|| format!("c-{s}"))
}

fn is_safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= ID_MAX
        && s != "."
        && s != ".."
        && s.chars()
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

pub fn audio_dir(work: &Path) -> PathBuf {
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
    let rec = read_record(&key).unwrap_or_else(|| Record {
        id: id.to_string(),
        ..Record::default()
    });
    if let Some(filed) = rec.filed.clone() {
        return Claim::Done(filed);
    }
    let (tx, _) = watch::channel(None);
    map.insert(key.clone(), tx.clone());
    Claim::Mine(Box::new(Job {
        key,
        work: work.to_path_buf(),
        rec,
        tx: Some(tx),
    }))
}

/// Ask an id's record what happened, without claiming anything. The cheap probe
/// behind "was my memo filed?", for a reader that no longer wants to re-upload.
pub fn filed(work: &Path, id: &str) -> Option<Filed> {
    read_record(&record_path(work, id)).and_then(|r| r.filed)
}

/// Every memo whose recording is on disk and whose note is not yet written.
///
/// The startup sweep's input, and the reason an interrupted transcription is a
/// delay rather than a loss. Records that have been resumed [`MAX_RESUMES`] times
/// are left out and named in the log: something about that recording is not
/// working, and spending a core on it at every restart helps nobody.
pub fn unfiled(work: &Path) -> Vec<Unfiled> {
    let Ok(entries) = std::fs::read_dir(dir(work)) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let Some(rec) = read_record(&p) else { continue };
        if rec.filed.is_some() {
            continue;
        }
        let (Some(audio), Some(pending)) = (rec.audio.clone(), rec.pending.clone()) else {
            // A record with no recording behind it: the process died between
            // writing the two. Nothing was lost that this could recover — the
            // phone still holds the only copy of those bytes — and a POST will
            // start it over.
            continue;
        };
        let audio = audio_dir(work).join(audio);
        if !audio.exists() {
            continue;
        }
        if rec.resumes >= MAX_RESUMES {
            tracing::error!(
                "note {}: unfiled after {} attempts, not resuming again — the recording is at {}",
                rec.id,
                rec.resumes,
                audio.display()
            );
            continue;
        }
        out.push(Unfiled {
            id: rec.id,
            audio,
            pending,
        });
    }
    // Oldest first: the memo that has waited longest through the most restarts.
    out.sort_by_key(|u| u.pending.stamp_ms);
    out
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
    rec: Record,
    /// Taken by `finish`, so `Drop` can tell an abandoned job from a done one.
    tx: Option<watch::Sender<Option<Outcome>>>,
}

impl Job {
    /// The recording an earlier attempt already saved, if it is still on disk.
    /// Reusing it is what stops a retried memo writing a second `.webm` — the
    /// production failure was the same 135 kB five times over.
    pub fn audio(&self) -> Option<PathBuf> {
        let p = audio_dir(&self.work).join(self.rec.audio.as_ref()?);
        p.exists().then_some(p)
    }

    /// Write down what this memo is and what its note must say, *before* the
    /// recording lands and long before the slow part starts. From here on the
    /// server can finish the job with no client and no memory of the request.
    pub fn accept(&mut self, audio: &Path, pending: Pending) {
        self.rec.audio = audio.file_name().map(|s| s.to_string_lossy().to_string());
        self.rec.pending = Some(pending);
        self.write();
    }

    /// Count one unattended attempt, so a recording nothing can transcribe stops
    /// being retried at every restart.
    pub fn resuming(&mut self) -> u32 {
        self.rec.resumes = self.rec.resumes.saturating_add(1);
        self.write();
        self.rec.resumes
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
                let (_, rx) =
                    watch::channel(Some(Err(Failed::new(500, "note job was already finished"))));
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
    ///
    /// A *failure* is written nowhere, which is the point: the record stays
    /// exactly as it was, still owed, and the next startup sweep picks it up.
    fn finish(&mut self, out: Outcome) {
        if let Ok(filed) = &out {
            self.rec.filed = Some(filed.clone());
            self.rec.pending = None; // nothing is owed any more
            self.write();
        }
        let tx = self.tx.take();
        lock_jobs().remove(&self.key);
        if let Some(tx) = tx {
            tx.send_replace(Some(out));
        }
    }

    fn write(&self) {
        let body = match serde_json::to_vec(&self.rec) {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("note queue: could not encode {}: {e}", self.rec.id);
                return;
            }
        };
        if let Err(e) = write_durable(&self.key, &body) {
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

/// Write a file so that it is either entirely there or not there at all.
///
/// `.part`, fsync, rename, fsync the directory — the pattern the chunk cache
/// already uses, plus the fsyncs, because these bytes are the memo. A partial
/// `.webm` under the real name would be a recording the resume sweep would
/// happily feed to whisper, and a partial record would be a note nobody can
/// finish writing. The directory fsync is what makes the rename itself survive a
/// power cut rather than only the bytes.
pub fn write_durable(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "tmp".into());
    let part = parent.join(format!("{name}.part"));
    {
        let mut f = std::fs::File::create(&part)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&part, path)?;
    // Best effort: a filesystem that will not let a directory be opened for this
    // is not a reason to fail a write that has already landed.
    if let Ok(d) = std::fs::File::open(parent) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// A handle on someone else's job — or on one's own, which is the same thing
/// once the work is detached.
pub struct Waiter(watch::Receiver<Option<Outcome>>);

impl Waiter {
    pub async fn wait(mut self) -> Outcome {
        match self.0.wait_for(|v| v.is_some()).await {
            Ok(v) => v
                .clone()
                .unwrap_or_else(|| Err(Failed::new(500, "the note task ended without a result"))),
            // The sender is gone without a value. `Drop` makes this unreachable;
            // a 500 keeps the recording queued if it ever happens anyway.
            Err(_) => Err(Failed::new(500, "the note task ended without a result")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drop one id's in-flight entry, which is all a process restart is from
    /// here. Never the whole map: the registry is process-wide and the rest of
    /// the suite is running alongside this test.
    fn forget(work: &Path, id: &str) {
        lock_jobs().remove(&record_path(work, id));
    }

    fn filed_note(file: &str) -> Filed {
        Filed {
            file: file.into(),
            text: "a thought".into(),
            language: "en".into(),
        }
    }

    fn pending() -> Pending {
        Pending {
            stamp_ms: 1_760_000_000_000,
            prompt: "a book, a chapter".into(),
            title: "A Book".into(),
            name: "A Book (2016).epub".into(),
            ctitle: "3: The Sea".into(),
            ctx: "the words around it".into(),
            ci: 3,
            i: 40,
            n: 76,
        }
    }

    fn mine(c: Claim) -> Job {
        match c {
            Claim::Mine(j) => *j,
            Claim::Done(_) => panic!("expected a job, got a replay"),
            Claim::Attach(_) => panic!("expected a job, got an attach"),
        }
    }

    /// A recording on disk, written the way the handler writes one.
    fn record_audio(work: &Path, name: &str) -> PathBuf {
        let p = audio_dir(work).join(name);
        write_durable(&p, b"...webm...").expect("recording");
        p
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

    #[test]
    fn a_durable_write_leaves_nothing_half_written() {
        let d = tempfile::tempdir().expect("tempdir");
        let p = d.path().join("deep/er/note.json");
        write_durable(&p, b"one").expect("write");
        write_durable(&p, b"two").expect("rewrite");
        assert_eq!(std::fs::read(&p).expect("read"), b"two");
        // The `.part` is gone, so nothing can mistake it for a recording.
        assert!(!p.with_file_name("note.json.part").exists());
    }

    #[tokio::test]
    async fn a_filed_memo_replays_instead_of_transcribing_again() {
        let d = tempfile::tempdir().expect("tempdir");
        let id = memo_id(None, b"a recording");
        let job = mine(claim(d.path(), &id));
        let w = job.spawn(async { Ok(filed_note("202609112201 a thought.md")) });
        assert_eq!(w.wait().await, Ok(filed_note("202609112201 a thought.md")));

        // The retry the reader makes when it never saw the 2xx: same body, no
        // second job, and nothing to transcribe.
        match claim(d.path(), &id) {
            Claim::Done(f) => assert_eq!(f, filed_note("202609112201 a thought.md")),
            _ => panic!("a filed memo must replay"),
        }
        // And the cheap probe — "was it filed?" — says the same without claiming
        // anything.
        assert_eq!(
            filed(d.path(), &id).map(|f| f.file).as_deref(),
            Some("202609112201 a thought.md")
        );
        assert_eq!(filed(d.path(), "c-never-seen"), None);
    }

    #[tokio::test]
    async fn a_filed_record_outlives_the_process() {
        // The registry is memory; the record is not. A container restarted
        // between the note being written and the phone hearing about it must
        // still replay rather than file a second note.
        let d = tempfile::tempdir().expect("tempdir");
        let id = memo_id(Some("restart-me"), b"x");
        let job = mine(claim(d.path(), &id));
        let w = job.spawn(async { Ok(filed_note("note.md")) });
        w.wait().await.expect("filed");

        // What a restart is, from this module's point of view: nothing in the
        // registry, and the same work directory. (This id only — the registry is
        // process-wide and the rest of the suite is running in it.)
        forget(d.path(), &id);
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
            Ok(filed_note("once.md"))
        });
        // While it runs, the id is taken: no second whisper run, no second note.
        let second = match claim(d.path(), &id) {
            Claim::Attach(w) => w,
            _ => panic!("an in-flight id must not start a second job"),
        };
        let _ = tx.send(());
        assert_eq!(first.wait().await, Ok(filed_note("once.md")));
        assert_eq!(second.wait().await, Ok(filed_note("once.md")));
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
                Ok(filed_note("survived.md"))
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
    async fn a_memo_the_process_died_on_is_owed_and_says_what_it_owes() {
        // The restart case, which is the one the box does most: killed between
        // accepting the recording and writing the note. The record is a work
        // order — passage, book, surrounding words, the stamp — so the next
        // process can finish it with no client anywhere.
        let d = tempfile::tempdir().expect("tempdir");
        let recording = record_audio(d.path(), "20260911220000.webm");
        let mut job = mine(claim(d.path(), "c-crashed"));
        job.accept(&recording, pending());
        drop(job); // the process died here

        let owed = unfiled(d.path());
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].id, "c-crashed");
        assert_eq!(owed[0].audio, recording);
        assert_eq!(owed[0].pending, pending());

        // Finishing it clears the debt — and the recording was never duplicated.
        let mut again = mine(claim(d.path(), "c-crashed"));
        assert_eq!(again.audio(), Some(recording));
        assert_eq!(again.resuming(), 1);
        let w = again.spawn(async { Ok(filed_note("finally.md")) });
        assert_eq!(w.wait().await.map(|f| f.file), Ok("finally.md".into()));
        assert!(
            unfiled(d.path()).is_empty(),
            "nothing is owed once it is filed"
        );
        assert_eq!(
            std::fs::read_dir(audio_dir(d.path()))
                .map(|d| d.count())
                .unwrap_or(0),
            1,
            "one recording, however many attempts"
        );
    }

    #[tokio::test]
    async fn a_memo_nothing_can_transcribe_stops_being_swept_but_is_never_deleted() {
        let d = tempfile::tempdir().expect("tempdir");
        let recording = record_audio(d.path(), "20260911221500.webm");
        let mut job = mine(claim(d.path(), "c-hopeless"));
        job.accept(&recording, pending());
        for n in 1..=MAX_RESUMES {
            assert_eq!(job.resuming(), n);
        }
        drop(job);

        assert!(
            unfiled(d.path()).is_empty(),
            "a restart does not spend another core on it"
        );
        // Everything it would need is still there, and a POST from the reader
        // still starts it: only the unattended sweep gives up.
        assert!(recording.exists());
        assert!(matches!(claim(d.path(), "c-hopeless"), Claim::Mine(_)));
    }

    #[tokio::test]
    async fn a_record_with_no_recording_behind_it_is_not_swept() {
        // The process died between the record and the bytes. There is nothing to
        // transcribe, and the phone still holds the only copy.
        let d = tempfile::tempdir().expect("tempdir");
        let mut job = mine(claim(d.path(), "c-no-bytes"));
        job.accept(&audio_dir(d.path()).join("20260911220000.webm"), pending());
        drop(job);
        assert!(unfiled(d.path()).is_empty());
        assert_eq!(mine(claim(d.path(), "c-no-bytes")).audio(), None);
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
        // exactly where it was — owed, and claimable here.
        let d = tempfile::tempdir().expect("tempdir");
        let recording = record_audio(d.path(), "20260911223000.webm");
        let mut job = mine(claim(d.path(), "c-failed"));
        job.accept(&recording, pending());
        let w = job.spawn(async { Err(Failed::new(500, "transcription failed")) });
        assert_eq!(w.wait().await.err().map(|f| f.status), Some(500));
        assert!(matches!(claim(d.path(), "c-failed"), Claim::Mine(_)));
        assert_eq!(unfiled(d.path()).len(), 1, "still owed");
    }
}
