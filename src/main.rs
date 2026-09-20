//! narrator — the server.
//!
//! Nothing in here is allowed to take the process down once it is listening. The
//! model may be missing, the vault may be an unmounted path, espeak-ng may not be
//! installed, ffmpeg may fail on one chapter: each of those is a logged, typed
//! error and a degraded capability, never a panic. A reader that serves text and
//! records voice notes with no engine at all is worth far more than a process
//! that exited cleanly.

use std::sync::Arc;

use narrator::{api, config::Config, export, migrate, render, state::AppState, watch};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // `narrator --openapi` prints the contract on stdout and exits: that is what
    // scripts/gen-client.sh feeds to @hey-api/openapi-ts, so generating the
    // TypeScript client never needs a running server or a work directory. No
    // subscriber is installed first, so nothing can share that stdout.
    if std::env::args().any(|a| a == "--openapi") {
        let (_, spec) = api::router(AppState::new(Config::from_env()));
        println!("{}", serde_json::to_string_pretty(&spec)?);
        return Ok(());
    }
    // `narrator --healthcheck` is the container's HEALTHCHECK: it asks the
    // running server for /healthz over the loopback and exits 0 or 1. Written by
    // hand against a TcpStream rather than pulling in an HTTP client or putting
    // curl in the runtime image for one request.
    if std::env::args().any(|a| a == "--healthcheck") {
        std::process::exit(if healthcheck() { 0 } else { 1 });
    }
    // `narrator export --book <file.epub>` packs the rendered chunks in the work
    // directory into an .m4b and exits. A CLI path, like `app/export.py` was:
    // nothing is waiting on it, it runs for minutes, and it has to work against
    // a work dir whose server is not running.
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("export") {
        std::process::exit(run_export(&argv[1..]));
    }
    // `narrator migrate` re-chunks the cached books and drops what that
    // invalidated. Destructive, one-shot and slow: the same reasons `export` is
    // a CLI path and not an endpoint.
    if argv.first().map(String::as_str) == Some("migrate") {
        std::process::exit(run_migrate(&argv[1..]));
    }
    // `narrator retrim` does the same to the wavs already on disk that the
    // renderer now does on the way out: takes off Kokoro's padding.
    if argv.first().map(String::as_str) == Some("retrim") {
        std::process::exit(run_retrim(&argv[1..]));
    }
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            // tower_http's per-request spans and ONNX Runtime's arena
            // bookkeeping are both noise at info level; the engine's own
            // warnings and errors still come through.
            EnvFilter::new("info,tower_http=warn,ort=warn")
        }))
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();

    let cfg = Config::from_env();
    std::fs::create_dir_all(&cfg.work)?;
    tracing::info!(
        "work={} books={:?} vault={:?} web={}",
        cfg.work.display(),
        cfg.books,
        cfg.vault,
        cfg.web.display()
    );
    let port = cfg.port;
    let state = AppState::new(cfg);

    // Whatever the last process knew and wrote down: the prerender target and
    // the book it was on. A restart is a deploy, and a deploy must not leave a
    // reader mid-chapter talking to a server with no session.
    narrator::boot(&state);
    // Load the engine off the request path so the first chunk is not also the
    // first 325 MB read. A failure here is a warning, not an exit.
    {
        let st = state.clone();
        std::thread::Builder::new()
            .name("engine-preload".into())
            .spawn(move || {
                st.engine.load();
                st.session().model_ready = st.engine.ready();
            })
            .ok();
    }
    watch::spawn(state.clone());
    render::ensure_build_thread(&state);

    let (app, spec) = api::router(state.clone());
    tracing::info!("openapi: {} paths at /openapi.json", spec.paths.paths.len());

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown(state))
        .await?;
    Ok(())
}

async fn shutdown(state: Arc<AppState>) {
    let ctrl_c = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    tracing::info!("shutting down");
    state.stop.store(true, std::sync::atomic::Ordering::SeqCst);
    state.run.set();
    state.build_ev.set();
}

/// `narrator retrim` — report by default, rewrite only on `--apply`.
fn run_retrim(argv: &[String]) -> i32 {
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", migrate::RETRIM_USAGE);
        return 0;
    }
    let cfg = Config::from_env();
    let args = match migrate::args_from(argv, &cfg) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("narrator retrim: {e}\n\n{}", migrate::RETRIM_USAGE);
            return 2;
        }
    };
    let keys: Vec<String> = match &args.only {
        Some(k) => vec![k.clone()],
        None => migrate::cached_books(&args.work),
    };
    let mut total = migrate::TrimReport::default();
    for key in &keys {
        let r = migrate::retrim(&args.work, key, args.apply);
        println!(
            "{key}: {} wavs, {} to trim, {} unreadable; {:.1} min -> {:.1} min (-{:.1} min)",
            r.examined,
            r.trimmed,
            r.failed,
            r.seconds_before / 60.0,
            r.seconds_after / 60.0,
            r.saved() / 60.0
        );
        total.examined += r.examined;
        total.trimmed += r.trimmed;
        total.failed += r.failed;
        total.seconds_before += r.seconds_before;
        total.seconds_after += r.seconds_after;
    }
    println!(
        "\n{} wavs, {} {}, {:.1} min of silence {}",
        total.examined,
        total.trimmed,
        if args.apply {
            "trimmed"
        } else {
            "would be trimmed"
        },
        total.saved() / 60.0,
        if args.apply {
            "removed"
        } else {
            "would come off"
        }
    );
    if !args.apply {
        println!("--dry-run: nothing written. re-run with --apply to do it.");
    } else if total.failed > 0 {
        println!(
            "{} wavs could not be read and were left alone.",
            total.failed
        );
    }
    if args.apply && total.trimmed > 0 {
        println!("re-pack any chapter you want the shorter gaps in; the manifest is rebuilt from the durations on disk.");
    }
    i32::from(total.failed > 0 && total.examined == total.failed)
}

/// `narrator migrate` — report by default, change the cache only on `--apply`.
///
/// Everything is planned for every book before anything is deleted, so a run
/// that cannot resolve one book's epub stops with the cache untouched rather
/// than half migrated.
fn run_migrate(argv: &[String]) -> i32 {
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", migrate::USAGE);
        return 0;
    }
    let cfg = Config::from_env();
    let args = match migrate::args_from(argv, &cfg) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("narrator migrate: {e}\n\n{}", migrate::USAGE);
            return 2;
        }
    };
    let keys: Vec<String> = match &args.only {
        Some(k) => vec![k.clone()],
        None => migrate::cached_books(&args.work),
    };
    if keys.is_empty() {
        println!("no cached books under {}", args.work.display());
        return 0;
    }

    let mut plans = Vec::new();
    for key in &keys {
        match migrate::plan_book(&args, key) {
            Ok(p) => plans.push(p),
            Err(e) => {
                eprintln!("narrator migrate: {e}");
                return 1;
            }
        }
    }

    let mut wavs = 0usize;
    let mut derived = 0usize;
    for p in &plans {
        let verdict = if p.is_noop() {
            "unchanged".to_string()
        } else {
            format!(
                "{} of {} chapters re-chunk; {} chunk wavs and {} packed/HLS artifacts to delete",
                p.changed.len(),
                p.chapters_total,
                p.wavs.len(),
                p.derived.len()
            )
        };
        println!(
            "{}: {} -> {} chunks; {verdict}",
            p.key, p.chunks_before, p.chunks_after
        );
        wavs += p.wavs.len();
        derived += p.derived.len();
    }

    let fixes = match args.positions.as_ref() {
        Some(d) => migrate::position_fixes(&narrator::vault::load_positions(d), &plans),
        None => Vec::new(),
    };
    for f in &fixes {
        println!(
            "position: {} chapter {} chunk {} -> {} (the last chunk both chunkings agree on)",
            f.book, f.chapter, f.from, f.to
        );
    }

    if !args.apply {
        println!("\n--dry-run: nothing written. {wavs} wavs and {derived} artifacts would go; {} positions would move.", fixes.len());
        println!("re-run with --apply to do it.");
        return 0;
    }

    for p in &plans {
        if p.is_noop() {
            continue;
        }
        if let Err(e) = migrate::apply_book(&args, p) {
            eprintln!("narrator migrate: {e}");
            return 1;
        }
        println!("migrated {}", p.key);
    }
    if let Some(d) = args.positions.as_ref() {
        if let Err(e) = migrate::apply_position_fixes(d, &fixes, &plans) {
            eprintln!("narrator migrate: {e}");
            return 1;
        }
    }
    println!(
        "done: {wavs} wavs and {derived} artifacts deleted, {} positions moved.",
        fixes.len()
    );
    println!("the render worker refills from the playhead; nothing else has to be called.");
    0
}

/// `narrator export` — print progress on stdout, problems on stderr, and return
/// the exit code. No tracing subscriber is installed on this path, so the
/// warnings the exporter logs are its own printed lines and nothing else.
fn run_export(argv: &[String]) -> i32 {
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", export::USAGE);
        return 0;
    }
    let cfg = Config::from_env();
    let args = match export::args_from(argv, cfg.work.clone()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("narrator export: {e}\n\n{}", export::USAGE);
            return 2;
        }
    };
    println!(
        "packing {} from {} ...",
        args.book.display(),
        args.dir
            .clone()
            .unwrap_or_else(|| args.work.join("audio"))
            .display()
    );
    match export::run(&args) {
        Ok(r) => {
            println!(
                "done: {}  ({:.0} MB, {} chapters, {:.1} h{})",
                r.out.display(),
                r.bytes as f64 / 1e6,
                r.chapters,
                r.seconds / 3600.0,
                if r.cover { ", cover embedded" } else { "" }
            );
            if !r.missing.is_empty() {
                println!(
                    "note: {} incomplete chapter(s) were left out",
                    r.missing.len()
                );
            }
            0
        }
        Err(e) => {
            eprintln!("narrator export: {e}");
            1
        }
    }
}

/// One loopback GET of `/healthz`. Anything other than a 200 - including no
/// answer at all - is unhealthy.
fn healthcheck() -> bool {
    use std::io::{Read, Write};
    let port: u16 = std::env::var("NARRATOR_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(7870);
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let timeout = std::time::Duration::from_secs(10);
    let Ok(mut s) = std::net::TcpStream::connect_timeout(&addr, timeout) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(timeout));
    let _ = s.set_write_timeout(Some(timeout));
    if s.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut buf = [0u8; 64];
    let Ok(n) = s.read(&mut buf) else {
        return false;
    };
    buf[..n].starts_with(b"HTTP/1.1 200")
}
