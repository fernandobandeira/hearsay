//! narrator — the server.
//!
//! Nothing in here is allowed to take the process down once it is listening. The
//! model may be missing, the vault may be an unmounted path, espeak-ng may not be
//! installed, ffmpeg may fail on one chapter: each of those is a logged, typed
//! error and a degraded capability, never a panic. A reader that serves text and
//! records voice notes with no engine at all is worth far more than a process
//! that exited cleanly.

use std::sync::Arc;

use narrator::{api, config::Config, render, state::AppState, watch};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            // tower_http's per-request spans are noise at info level.
            EnvFilter::new("info,tower_http=warn")
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

    // The prerender target belongs on disk: it is one number the user chose, and
    // a redeploy used to silently drop a 13-chapter buffer back to the default.
    if let Some(h) = api::session::load_prerender(&state) {
        state.session().prerender_hours = Some(h);
        tracing::info!("prerender target restored: {h} h");
    }
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
