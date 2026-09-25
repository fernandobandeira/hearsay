//! The packer's files on disk: what a re-pack leaves behind, and what two
//! segmenters asked for the same chapter at once leave behind.
//!
//! Both are bugs of the same shape — a file under the real name that describes
//! something other than what is there — and both are invisible from the API: a
//! playlist that exists is served, whatever it was cut from.

use std::path::Path;
use std::sync::Arc;

use narrator::book::Chunk;
use narrator::cache;
use narrator::chapters;
use narrator::config::Config;

/// ffmpeg is not on the CI runner. Everything here is an encode or a segment, so
/// without it there is nothing to assert.
fn have_ffmpeg() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|paths| std::env::split_paths(&paths).any(|d| d.join("ffmpeg").is_file()))
}

fn chunks(n: usize) -> Vec<Chunk> {
    (0..n)
        .map(|i| Chunk {
            text: format!("Sentence {i}."),
            para: 0,
            silent: false,
        })
        .collect()
}

/// A chapter's chunks, each `secs` long, as a finished render would leave them.
fn seed(cfg: &Config, key: &str, ci: usize, n: usize, secs: f32) {
    let samples = (secs * narrator::tts::SR as f32) as usize;
    for i in 0..n {
        cache::write_wav(
            &cache::chunk_path(&cfg.work, key, ci, i),
            &vec![0.1f32; samples],
        )
        .expect("seed");
    }
}

fn pack(cfg: &Config, key: &str, ci: usize, n: usize) -> chapters::Manifest {
    chapters::build(
        cfg,
        key,
        ci,
        &chunks(n),
        &cache::chapter_dir(&cfg.work, key, ci),
        "Chapter",
        "Book",
    )
    .expect("pack")
}

/// Every segment a playlist names is on disk beside it.
fn playlist_is_whole(playlist: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(playlist) else {
        return false;
    };
    let dir = playlist.parent().expect("dir");
    let segs: Vec<&str> = text
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .collect();
    !segs.is_empty()
        && segs.iter().all(|l| {
            let name = l.rsplit('/').next().unwrap_or(l);
            dir.join(name).is_file()
        })
}

#[test]
fn a_repacked_chapter_does_not_keep_streaming_the_old_audio() {
    if !have_ffmpeg() {
        eprintln!("skipping: no ffmpeg");
        return;
    }
    let d = tempfile::tempdir().expect("tempdir");
    let cfg = Config::for_test(d.path());
    let (key, ci, n) = ("Book", 0usize, 3usize);

    seed(&cfg, key, ci, n, 1.0);
    let first = pack(&cfg, key, ci, n);
    let playlist = chapters::build_hls(&cfg, key, ci, "/hls/Book/0/").expect("hls");
    assert!(playlist.exists());

    // The chapter is re-rendered — longer this time — and packed again. The m4a
    // and the manifest are replaced; the HLS cut from the old m4a must not
    // survive them, or the player streams the old audio against new start times.
    seed(&cfg, key, ci, n, 2.0);
    let second = pack(&cfg, key, ci, n);
    assert!(
        second.duration > first.duration + 2.0,
        "the fixture re-rendered"
    );
    assert!(
        !chapters::hls_dir(&cfg.work, key, ci).exists(),
        "the stale HLS directory was left behind a re-pack"
    );

    // And the next ask segments the new file.
    let again = chapters::build_hls(&cfg, key, ci, "/hls/Book/0/").expect("hls");
    let text = std::fs::read_to_string(&again).expect("playlist");
    let listed: f64 = text
        .lines()
        .filter_map(|l| l.strip_prefix("#EXTINF:"))
        .filter_map(|l| l.trim_end_matches(',').parse::<f64>().ok())
        .sum();
    assert!(
        (listed - second.duration).abs() < 1.0,
        "the playlist is {listed}s, the chapter is {}s",
        second.duration
    );
}

#[test]
fn two_segmenters_on_one_chapter_do_not_clobber_each_other() {
    if !have_ffmpeg() {
        eprintln!("skipping: no ffmpeg");
        return;
    }
    let d = tempfile::tempdir().expect("tempdir");
    let cfg = Arc::new(Config::for_test(d.path()));
    let (key, ci, n) = ("Book", 1usize, 6usize);
    seed(&cfg, key, ci, n, 2.0);
    pack(&cfg, key, ci, n);

    // What a player and its service worker do to a chapter nobody has
    // segmented yet: ask for it at the same moment, from different threads.
    let barrier = Arc::new(std::sync::Barrier::new(6));
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let cfg = cfg.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                chapters::build_hls(&cfg, key, ci, "/hls/Book/1/")
            })
        })
        .collect();
    for h in handles {
        let r = h.join().expect("thread");
        assert!(r.is_ok(), "a concurrent segmenter failed: {r:?}");
    }
    let playlist = chapters::hls_dir(&cfg.work, key, ci).join("index.m3u8");
    assert!(
        playlist_is_whole(&playlist),
        "the playlist names segments that are not on disk"
    );
    assert!(
        !chapters::hls_dir(&cfg.work, key, ci)
            .with_file_name(format!("ch{ci:03}.part"))
            .exists(),
        "a segmenter's temp directory was left behind"
    );
}

#[test]
fn a_written_chunk_is_whole_and_leaves_no_part_behind() {
    // The fsync itself is not observable without pulling the power; what is, and
    // what the rename relies on, is that the name only ever appears complete.
    let d = tempfile::tempdir().expect("tempdir");
    let p = cache::chunk_path(d.path(), "Book", 0, 0);
    cache::write_wav(&p, &vec![0.25f32; narrator::tts::SR as usize]).expect("write");
    assert!(!p.with_extension("wav.part").exists());
    let (_, _, _, secs) = cache::wav_info(&p).expect("info");
    assert!((secs - 1.0).abs() < 1e-9, "{secs}");
}
