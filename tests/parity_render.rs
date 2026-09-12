//! Cache-layout and render-worker parity.
//!
//! Two things are being protected here. The first is that a Rust deploy must be
//! able to **adopt the VPS cache in place**: the same directory names, the same
//! zero padding, the same wav format, the same `plan.json` and packed-chapter
//! placement, so nothing is re-rendered and no stored position moves.
//!
//! The second is the disk-truth invariant — the production bug this rewrite was
//! asked to fix. The python worker trusts `render_idx` as the record of what has
//! been rendered; `gc_audio`, a forward `/api/playhead` jump and a container
//! restart can each leave a hole *behind* that frontier, and the chunk the
//! reader is waiting on then never gets rendered at all.

mod harness;

use std::path::Path;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use harness::Harness;
use narrator::cache;
use serde_json::json;

/// Wait for a predicate, or fail with what was on disk when time ran out.
async fn until(what: &str, timeout_s: f64, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while t0.elapsed().as_secs_f64() < timeout_s {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out waiting for {what}");
}

fn key_of(h: &Harness) -> String {
    h.state.session().key_or_x()
}

#[tokio::test]
async fn the_cache_layout_is_the_one_python_writes() {
    let h = Harness::new().await;
    let load = h.load().await;
    let key = load["key"].as_str().unwrap_or("").to_string();
    assert_eq!(key, "Fixture (2026)", "the stem, truncated to 50");

    // plan.json sits next to the chapter directories, where `narrator export`
    // looks for it.
    let plan = cache::plan_path(&h.work(), &key);
    assert!(plan.exists(), "{}", plan.display());
    let parsed: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&plan).expect("plan")).expect("json");
    assert!(parsed.is_array());
    assert_eq!(
        parsed[0]["chunks"][0]["text"].as_str(),
        load["chapters"][0]["title"].as_str(),
        "the fixture's first chunk is its heading"
    );

    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;
    let p = cache::chunk_path(&h.work(), &key, 0, 0);
    until("the first chunk", 20.0, || p.exists()).await;
    assert_eq!(
        p.strip_prefix(h.work()).map(Path::to_path_buf),
        Ok(Path::new("audio/Fixture (2026)/ch000/00000.wav").to_path_buf())
    );
    let (ch, rate, width, secs) = cache::wav_info(&p).expect("wav header");
    assert_eq!((ch, rate, width), (1, 24_000, 2), "24 kHz mono s16le");
    assert!(secs > 0.0);
}

#[tokio::test]
async fn open_renders_the_opened_chunk_even_with_holes_in_the_cache() {
    let h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);
    let n = h.state.session().plan[0].chunks.len();
    assert!(n >= 5, "the fixture chapter needs a few chunks");

    // A cache full of holes, as gc or a half-finished render leaves it: every
    // chunk present except the one the reader is about to open.
    let target = n - 2;
    for i in 0..n {
        if i != target {
            cache::write_wav(&cache::chunk_path(&h.work(), &key, 0, i), &[0.0f32; 2400])
                .expect("seed");
        }
    }
    let missing = cache::chunk_path(&h.work(), &key, 0, target);
    assert!(!missing.exists());

    // The python worker would set render_idx = target, render it, and move on -
    // which happens to work. The interesting case is the next test.
    let (code, _) = h
        .post_json("/api/open", json!({"chapter": 0, "chunk": target}))
        .await;
    assert_eq!(code, StatusCode::OK);
    until("the opened chunk", 20.0, || missing.exists()).await;
}

#[tokio::test]
async fn a_hole_behind_the_render_frontier_is_still_filled() {
    // The production bug, reproduced: the renderer has run to the end of the
    // chapter (render_idx == n), and only *then* does the playhead land on a
    // chunk that is not on disk - evicted by gc, or lost to a restart. With
    // render_idx as the record of truth there is nothing left to do and the
    // reader waits on a 404 forever.
    let h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);
    let n = h.state.session().plan[0].chunks.len();
    for i in 0..n {
        cache::write_wav(&cache::chunk_path(&h.work(), &key, 0, i), &[0.0f32; 2400]).expect("seed");
    }
    // Drive the worker to the end of the chapter and let it settle: with every
    // chunk on disk there is nothing left in this chapter to do, so the frontier
    // is at the end and the status leaves "starting".
    h.post_json("/api/open", json!({"chapter": 0, "chunk": n - 1}))
        .await;
    until("the worker to finish this chapter", 20.0, || {
        !matches!(h.state.session().status.as_str(), "starting" | "idle")
    })
    .await;

    // Now punch the hole, behind the frontier, under the playhead.
    let hole = 1usize;
    let p = cache::chunk_path(&h.work(), &key, 0, hole);
    std::fs::remove_file(&p).expect("evict");
    assert!(!p.exists());
    h.post_json("/api/playhead", json!({"chunk": hole})).await;

    until("the evicted chunk to be re-rendered", 20.0, || p.exists()).await;
    // And the API serves it again.
    let (code, _) = h.get(&format!("/api/chunk/0/{hole:05}.wav")).await;
    assert_eq!(code, StatusCode::OK);
}

#[tokio::test]
async fn a_forward_jump_does_not_strand_the_chunk_under_the_playhead() {
    let h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);
    let n = h.state.session().plan[0].chunks.len();
    // Render nothing; open at 0 so the worker starts, then jump forward. The
    // chunk under the playhead must come first, not after everything between.
    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;
    let far = n - 1;
    h.post_json("/api/playhead", json!({"chunk": far})).await;
    let p = cache::chunk_path(&h.work(), &key, 0, far);
    until("the jumped-to chunk", 20.0, || p.exists()).await;
}

/// The other half of rule 1: it has no exit.
///
/// A chunk under the playhead that will not render is re-attempted every time
/// round the worker loop, and before the backoff that was a full-speed retry —
/// an espeak spawn and a warn line per iteration for as long as the reader sits
/// there. The invariant still has to hold (the chunk is never abandoned), so
/// what is asserted here is both halves: it is still retried, and it is not
/// retried thousands of times.
#[cfg(unix)]
#[tokio::test]
async fn a_chunk_that_will_not_render_backs_off_instead_of_spinning() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;

    let ro = |p: &Path, yes: bool| {
        std::fs::set_permissions(
            p,
            std::fs::Permissions::from_mode(if yes { 0o555 } else { 0o755 }),
        )
    };

    let h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);
    // Every write into chapter 0 fails: the directory is there and is not
    // writable, which is what a read-only mount or a full disk looks like from
    // inside `render_one`.
    let dir = cache::chapter_dir(&h.work(), &key, 0);
    std::fs::create_dir_all(&dir).expect("dir");
    ro(&dir, true).expect("chmod");
    // root ignores the mode, and then there is nothing to test.
    let probe = dir.join(".probe");
    if std::fs::write(&probe, b"x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        let _ = ro(&dir, false);
        eprintln!("skipping: the directory is writable anyway (root?)");
        return;
    }

    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let tries = h.state.render_attempts.load(Ordering::Relaxed);
    ro(&dir, false).expect("restore");

    assert!(
        tries >= 2,
        "the failing chunk must still be retried: {tries}"
    );
    assert!(
        tries <= 12,
        "a failing chunk must not spin the worker: {tries} attempts in 2.5 s"
    );
    // And it heals the moment the cause does: the next tick renders.
    let p = cache::chunk_path(&h.work(), &key, 0, 0);
    until(
        "the chunk once the directory is writable again",
        20.0,
        || p.exists(),
    )
    .await;
}

#[tokio::test]
async fn gc_evicts_by_age_but_never_the_chapter_being_read() {
    let h = Harness::with(|c| {
        // A cap small enough that everything is over it.
        c.max_audio_gb = 0.000_000_1;
        c.prerender_chapters = 0;
    })
    .await;
    h.load().await;
    let key = key_of(&h);
    let chapters = h.state.session().plan.len();
    assert!(chapters >= 2);
    for ci in 0..chapters {
        for i in 0..2usize {
            cache::write_wav(
                &cache::chunk_path(&h.work(), &key, ci, i),
                &[0.0f32; 24_000],
            )
            .expect("seed");
        }
    }
    h.state.session().chapter = 1;
    h.state.session().queue.push(chapters - 1);

    let keep = h.state.session().gc_keep(&h.state.cfg);
    cache::gc_audio(&h.work(), h.state.cfg.max_audio_gb, &keep);

    // The chapter being read survives.
    assert!(
        cache::chunk_path(&h.work(), &key, 1, 0).exists(),
        "current chapter"
    );
    // So does a chapter the manager was told to render - its wavs are the input
    // to a pack, and evicting them leaves it permanently unpackable.
    assert!(
        cache::chunk_path(&h.work(), &key, chapters - 1, 0).exists(),
        "queued chapter"
    );
    // Chapter 0 is neither, so it goes.
    assert!(
        !cache::chunk_path(&h.work(), &key, 0, 0).exists(),
        "evictable"
    );
}

#[tokio::test]
async fn a_packed_chapter_lands_where_python_puts_it_with_the_same_manifest() {
    if which("ffmpeg").is_none() {
        eprintln!("skipping: no ffmpeg");
        return;
    }
    let h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);
    let chunks = h.state.session().plan[0].chunks.clone();
    let n = chunks.len();
    // One second per chunk, so the manifest arithmetic is checkable by hand.
    for i in 0..n {
        cache::write_wav(&cache::chunk_path(&h.work(), &key, 0, i), &[0.0f32; 24_000])
            .expect("seed");
    }
    let (code, body) = h
        .post_json("/api/chapters/build", json!({"chapters": [0]}))
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");

    let (m4a, jf) = narrator::chapters::chapter_files(&h.work(), &key, 0);
    until("the m4a", 30.0, || m4a.exists() && jf.exists()).await;
    assert_eq!(
        m4a.strip_prefix(h.work()).map(Path::to_path_buf),
        Ok(Path::new("chapters/Fixture (2026)/ch000.m4a").to_path_buf())
    );

    let man = narrator::chapters::read_manifest(&h.work(), &key, 0).expect("manifest");
    assert_eq!(man.chunks, n);
    assert_eq!(man.starts.len(), n);
    assert_eq!(man.sample_rate, 24_000);
    assert_eq!(man.gap, 0.30);
    assert_eq!(man.para_gap, 0.60);
    // start[i] = i seconds of audio plus the gaps before it, 0.30 within a
    // paragraph and 0.60 across one.
    let mut t = 0.0f64;
    for i in 0..n {
        if i > 0 {
            t += if chunks[i].para != chunks[i - 1].para {
                0.60
            } else {
                0.30
            };
        }
        assert!(
            (man.starts[i] - (t * 1000.0).round() / 1000.0).abs() < 1e-9,
            "start[{i}] = {} want {t}",
            man.starts[i]
        );
        t += 1.0;
    }
    assert!((man.duration - t).abs() < 1e-6);

    // The chunk -> second mapping inverts.
    assert_eq!(narrator::chapters::chunk_at(&man, 0.0), 0);
    assert_eq!(narrator::chapters::chunk_at(&man, man.starts[n - 1]), n - 1);
}

#[tokio::test]
async fn the_packed_chapter_serves_byte_ranges_and_an_hls_rendition() {
    if which("ffmpeg").is_none() {
        eprintln!("skipping: no ffmpeg");
        return;
    }
    let h = Harness::new().await;
    let load = h.load().await;
    let key = key_of(&h);
    let raw_key = load["key"].as_str().unwrap_or("");
    // The server builds HLS URLs with percent-encoding (space -> %20), not the
    // form encoding (space -> +), because they are path segments.
    let enc: String =
        percent_encoding::utf8_percent_encode(raw_key, percent_encoding::NON_ALPHANUMERIC)
            .to_string();
    let n = h.state.session().plan[0].chunks.len();
    for i in 0..n {
        cache::write_wav(&cache::chunk_path(&h.work(), &key, 0, i), &[0.0f32; 24_000])
            .expect("seed");
    }
    h.post_json("/api/chapters/build", json!({"chapters": [0]}))
        .await;
    let (m4a, _) = narrator::chapters::chapter_files(&h.work(), &key, 0);
    until("the m4a", 30.0, || m4a.exists()).await;
    let size = m4a.metadata().map(|m| m.len()).unwrap_or(0);

    // iOS probes with a HEAD and then a Range before it will play anything.
    let (code, body) = h.head(&format!("/api/chapters/0.m4a?book={enc}")).await;
    assert_eq!(code, StatusCode::OK);
    assert!(body.is_empty(), "HEAD carries no body");

    let (code, body) = h
        .get_with(
            &format!("/api/chapters/0.m4a?book={enc}"),
            ("range", "bytes=0-99"),
        )
        .await;
    assert_eq!(code, StatusCode::PARTIAL_CONTENT);
    assert_eq!(body.len(), 100);

    let (code, body) = h
        .get_with(
            &format!("/api/chapters/0.m4a?book={enc}"),
            ("range", "bytes=-64"),
        )
        .await;
    assert_eq!(code, StatusCode::PARTIAL_CONTENT, "suffix range");
    assert_eq!(body.len(), 64);

    let (code, _) = h
        .get_with(
            &format!("/api/chapters/0.m4a?book={enc}"),
            ("range", &format!("bytes={}-", size + 10)),
        )
        .await;
    assert_eq!(code, StatusCode::RANGE_NOT_SATISFIABLE);

    // An unrecognised range *unit* is ignored, not rejected (RFC 9110 14.2).
    let (code, body) = h
        .get_with(
            &format!("/api/chapters/0.m4a?book={enc}"),
            ("range", "items=0-5"),
        )
        .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body.len() as u64, size);

    // HLS is built lazily on the first request, stream-copied from the m4a.
    let (code, playlist) = h.get(&format!("/api/chapters/0.m3u8?book={enc}")).await;
    assert_eq!(code, StatusCode::OK);
    let text = String::from_utf8_lossy(&playlist).to_string();
    assert!(text.starts_with("#EXTM3U"), "{text}");
    // ffmpeg writes EXT-X-MAP as a bare filename; it must be rewritten or the
    // player 404s before it plays a note.
    assert!(
        text.contains(&format!("#EXT-X-MAP:URI=\"/api/hls/{enc}/0/init.mp4\"")),
        "{text}"
    );
    let seg = text
        .lines()
        .find(|l| l.contains(".m4s"))
        .expect("a segment line");
    let (code, bytes) = h.get(seg.trim()).await;
    assert_eq!(code, StatusCode::OK, "{seg}");
    assert!(!bytes.is_empty());

    // And nothing else under /api/hls is servable: the name is matched against
    // `init.mp4|segNNNNN.m4s` and nothing else gets near the filesystem.
    for name in ["evil.sh", "seg1.m4s", "init.mp5", "..", "%2e%2e"] {
        let (code, _) = h.get(&format!("/api/hls/{enc}/0/{name}")).await;
        assert_eq!(code, StatusCode::NOT_FOUND, "{name}");
    }
    // A traversal with extra segments does not match the route at all; whatever
    // answers it, it is never the file.
    let (code, body) = h.get("/api/hls/x/0/../../../../etc/passwd").await;
    assert_ne!(code, StatusCode::OK);
    assert!(!String::from_utf8_lossy(&body).contains("root:"), "{code}");
}

#[tokio::test]
async fn an_existing_cache_is_adopted_rather_than_re_rendered() {
    let h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);
    let n = h.state.session().plan[0].chunks.len();
    // A cache written by the python server: same paths, same format.
    for i in 0..n {
        cache::write_wav(&cache::chunk_path(&h.work(), &key, 0, i), &[0.0f32; 24_000])
            .expect("seed");
    }
    let stamps: Vec<_> = (0..n)
        .map(|i| {
            cache::chunk_path(&h.work(), &key, 0, i)
                .metadata()
                .and_then(|m| m.modified())
                .ok()
        })
        .collect();

    let (_, rows) = h.get_json("/api/chapters").await;
    assert_eq!(rows["chapters"][0]["rendered"], serde_json::json!(n));

    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    for (i, was) in stamps.iter().enumerate() {
        let now = cache::chunk_path(&h.work(), &key, 0, i)
            .metadata()
            .and_then(|m| m.modified())
            .ok();
        assert_eq!(&now, was, "chunk {i} was rewritten");
    }
}

#[tokio::test]
async fn a_manifest_that_disagrees_with_the_plan_is_reported_as_unpacked() {
    let h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);
    // An m4a and a manifest from before a re-chunking: the file exists but its
    // start times point at the wrong words. That is a trap, not a packed
    // chapter.
    let (m4a, jf) = narrator::chapters::chapter_files(&h.work(), &key, 0);
    std::fs::create_dir_all(m4a.parent().unwrap_or(Path::new("."))).expect("dir");
    std::fs::write(&m4a, b"pretend this is aac").expect("m4a");
    std::fs::write(
        &jf,
        serde_json::to_vec(&json!({
            "book": key, "chapter": 0, "title": "", "chunks": 9999,
            "starts": [0.0], "duration": 1.0, "gap": 0.3, "para_gap": 0.6,
            "bitrate": "64k", "sample_rate": 24000, "bytes": 19,
            "built": "2026-01-01T00:00:00", "build_s": 0.0
        }))
        .unwrap_or_default(),
    )
    .expect("manifest");

    let (_, rows) = h.get_json("/api/chapters").await;
    assert_eq!(
        rows["chapters"][0]["m4a"],
        serde_json::json!(false),
        "a stale manifest must not be advertised as packed"
    );
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|d| d.join(bin))
            .find(|p| p.is_file())
    })
}

// ------------------------------------------------------ the STT priority gate

/// Whisper outranks the renderer, and the renderer has to actually stand down.
///
/// The A1 is two cores. With chapters queued, a ~45 s memo took over seven
/// minutes to come back because Kokoro and whisper were taking turns on the same
/// two cores — and a memo is the one artifact here that exists in exactly one
/// place (IndexedDB on a phone) until `/api/note` answers. A rendered chunk can
/// always be made again.
#[tokio::test]
async fn a_transcription_parks_the_renderer_until_it_is_done() {
    let h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);
    let p = cache::chunk_path(&h.work(), &key, 0, 0);

    // What `Whisper::transcribe` holds for the length of a memo.
    let busy = h.state.whisper.gate().enter();
    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;
    // Long enough that the fake engine would have rendered the whole chapter.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(
        !p.exists(),
        "the renderer must not take a core while a memo is being transcribed"
    );

    drop(busy);
    until("the renderer to resume", 20.0, || p.exists()).await;
}

/// And the packer: a new ffmpeg encode does not start while a memo is in flight.
/// One already running is left to finish on purpose — killing an encode
/// mid-chapter throws away everything it has done.
#[tokio::test]
async fn a_transcription_holds_back_a_new_pack() {
    let h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);
    let n = h.state.session().plan[0].chunks.len();
    for i in 0..n {
        cache::write_wav(&cache::chunk_path(&h.work(), &key, 0, i), &[0.0f32; 24_000])
            .expect("seed");
    }

    let busy = h.state.whisper.gate().enter();
    let (code, body) = h
        .post_json("/api/chapters/build", json!({"chapters": [0]}))
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        h.state.session().building,
        None,
        "no encode may start while a memo is being transcribed"
    );
    // The request was accepted and is still waiting, not dropped.
    assert!(h.state.session().pack_queue.contains(&0));

    drop(busy);
    if which("ffmpeg").is_none() {
        eprintln!("skipping the second half: no ffmpeg");
        return;
    }
    let (m4a, _) = narrator::chapters::chapter_files(&h.work(), &key, 0);
    until("the pack once the memo is filed", 30.0, || m4a.exists()).await;
}

/// The guard belongs to the blocking work, not to the request future.
///
/// `/api/note` transcribes inside `spawn_blocking`. A phone that locks mid-memo,
/// an iOS PWA going to the background, a tunnel blip: axum drops the request
/// future and the `.await` is cancelled, but the blocking job runs on. If the
/// gate were held by the future, that cancellation would leak it and park the
/// renderer for the life of the process — a far worse failure than the one the
/// gate exists to fix. It is held by the closure, so it goes when the work does.
#[tokio::test]
async fn a_cancelled_request_cannot_leak_the_gate() {
    let h = Harness::new().await;
    let st = h.state.clone();
    let job = tokio::task::spawn_blocking(move || {
        let _busy = st.whisper.gate().enter();
        std::thread::sleep(Duration::from_millis(200));
    });
    until("the gate to be claimed", 5.0, || {
        h.state.whisper.gate().held()
    })
    .await;
    // The client went away: the future is dropped, the blocking job is not.
    drop(job);
    until("the gate to clear on its own", 10.0, || {
        !h.state.whisper.gate().held()
    })
    .await;
}

// ------------------------------------------------- the packer's CPU policy

/// The packer stands down for a renderer that is stalled under the playhead.
///
/// One rank below the STT gate above, and the same shape for a related reason.
/// Two ARM cores, Kokoro at a quarter of realtime: an ffmpeg encode running
/// while the renderer is stuck on the chunk somebody is listening to *right now*
/// is a reader waiting longer for the chapter in their hands so that a chapter
/// for tonight can be filed. It matters more since the wishlist, because a
/// resumed download can fill the pack queue seconds after boot with no client
/// involved — nobody is watching to notice the trade being made badly.
///
/// The policy in one line: pack when the renderer is ahead or idle, hold while
/// it is behind. Only rule 1 counts as behind — the lookahead is 80 chunks and
/// a packer that waited for that would never run at all — and the hold is
/// bounded (`PACK_HOLD_MAX_S`), because a renderer that is wedged rather than
/// slow must not turn a download into a deadlock.
#[cfg(unix)]
#[tokio::test]
async fn a_pack_holds_back_while_the_renderer_is_stalled_under_the_playhead() {
    use std::os::unix::fs::PermissionsExt;

    let ro = |p: &Path, yes: bool| {
        std::fs::set_permissions(
            p,
            std::fs::Permissions::from_mode(if yes { 0o555 } else { 0o755 }),
        )
    };

    let h = Harness::new().await;
    h.load().await;
    let key = key_of(&h);

    // Chapter 1 is complete, so it is packable the moment it is asked for.
    let n1 = h.state.session().plan[1].chunks.len();
    for i in 0..n1 {
        cache::write_wav(&cache::chunk_path(&h.work(), &key, 1, i), &[0.0f32; 24_000])
            .expect("seed");
    }

    // Chapter 0 cannot be written to, so the chunk under the playhead will not
    // render: rule 1 with no exit, which is exactly the stalled condition.
    let dir = cache::chapter_dir(&h.work(), &key, 0);
    std::fs::create_dir_all(&dir).expect("dir");
    ro(&dir, true).expect("chmod");
    let probe = dir.join(".probe");
    if std::fs::write(&probe, b"x").is_ok() {
        let _ = std::fs::remove_file(&probe);
        let _ = ro(&dir, false);
        eprintln!("skipping: the directory is writable anyway (root?)");
        return;
    }

    h.post_json("/api/open", json!({"chapter": 0, "chunk": 0}))
        .await;
    until("the renderer to report itself stalled", 20.0, || {
        h.state.stalled_for() > 0.0
    })
    .await;

    // The download goes in while the renderer is stuck. It is accepted and
    // queued; what must not happen is an encode starting on top of it.
    let (code, body) = h
        .post_json(
            "/api/chapters/render",
            json!({"chapters": [1], "pack": true}),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["packing"], json!([1]), "complete, so queued to pack");
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        h.state.session().building,
        None,
        "no encode while the reader is waiting on a chunk"
    );
    assert!(
        h.state.session().pack_queue.contains(&1),
        "held back, not dropped"
    );

    // The disk comes back: the renderer catches up, stops being stalled, and
    // the packer takes the chapter it was holding.
    ro(&dir, false).expect("restore");
    until("the renderer to catch up", 40.0, || {
        h.state.stalled_for() == 0.0
    })
    .await;
    until("the packer to take it", 40.0, || {
        let s = h.state.session();
        s.building == Some(1) || !s.pack_queue.contains(&1)
    })
    .await;
}
