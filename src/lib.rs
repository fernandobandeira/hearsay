//! narrator — self-hosted TTS book reader, the Rust server.
//!
//! The python implementation at `~/git/narrator` is the reference; this crate
//! re-implements its HTTP contract, its cache layout, its chunking (bug for bug)
//! and its vault writes, so the two are interchangeable in front of the same
//! `work/` directory and the same Obsidian vault.

pub mod api;
pub mod book;
pub mod cache;
pub mod chapters;
pub mod config;
pub mod err;
pub mod events;
pub mod export;
pub mod plancache;
pub mod render;
pub mod state;
pub mod stt;
pub mod text;
pub mod tts;
pub mod vault;
pub mod watch;
pub mod wishlist;

/// Everything the process reads back out of its work directory before it starts
/// serving — the state a restart would otherwise have thrown away.
///
/// `main` calls this, and so does the test harness's `restart`, so "the box was
/// redeployed" in a test is the same sequence the box performs. Nothing here can
/// fail the boot: each restore is an `Option` and a missing one just means the
/// process starts the way it always did.
pub fn boot(st: &std::sync::Arc<state::AppState>) {
    // The prerender target is one number the user chose, and a redeploy used to
    // silently drop a 13-chapter buffer back to the default.
    if let Some(h) = api::session::load_prerender(st) {
        st.session().prerender_hours = Some(h);
        tracing::info!("prerender target restored: {h} h");
    }
    // The loaded book, so a reader that was mid-chapter when the container went
    // away is not left asking a server with no session for audio it will not
    // serve. See `api::session::restore_session`.
    match api::session::restore_session(st) {
        Some(key) => {
            let s = st.session();
            tracing::info!(
                "session restored: {key} at chapter {} chunk {}",
                s.chapter,
                s.playhead
            );
        }
        None => tracing::info!("no previous session to restore"),
    }
    // The chapters someone asked for and did not get. Strictly after the session
    // restore: a wishlist is chapter indices into one book's plan, so with no
    // book there is nothing it could be applied to — and applying it to the
    // wrong one is the failure worth designing against, not the one worth
    // risking. See `wishlist::resume`.
    if let Some(r) = wishlist::resume(st) {
        if !r.parked.is_empty() {
            tracing::error!(
                "queue: {} chapter(s) parked after {} attempts: {:?}",
                r.parked.len(),
                wishlist::MAX_ATTEMPTS,
                r.parked
            );
        }
        if !r.queued.is_empty() {
            tracing::info!(
                "queue restored: {} chapter(s) still wanted, starting in {:.0}s",
                r.queued.len(),
                st.cfg.queue_resume_delay_s
            );
            // Deliberately not here and now: the box has two cores and the memo
            // sweep may be about to want one of them.
            wishlist::start_soon(st, r.queued.len());
        }
    }
}
