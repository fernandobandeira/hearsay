//! The library, watched.
//!
//! "A new book appeared" has exactly one honest source: the filesystem. Watching
//! both book roots covers a file dropped into `./books` and — the case this is
//! for — an epub the vault's git sync just pulled onto the server, with no git
//! coupling and nothing to remember to call.
//!
//! Directories that do not exist are skipped rather than fatal: the vault is an
//! optional mount, and a watcher that refuses to start would take the `books`
//! event away from the root that *does* exist.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use notify::{RecursiveMode, Watcher};
use serde_json::json;

use crate::state::AppState;

pub fn spawn(st: Arc<AppState>) {
    if !st.cfg.watch_books {
        return;
    }
    let roots: Vec<_> = st
        .cfg
        .books
        .iter()
        .filter(|d| d.is_dir())
        .cloned()
        .collect();
    if roots.is_empty() {
        tracing::info!("no book directory exists yet; not watching");
        return;
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
    let mut watcher =
        match notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                let _ = tx.send(ev);
            }
        }) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("could not start the library watcher: {e}");
                return;
            }
        };
    for r in &roots {
        if let Err(e) = watcher.watch(r, RecursiveMode::Recursive) {
            tracing::warn!("not watching {}: {e}", r.display());
        } else {
            tracing::info!("watching {}", r.display());
        }
    }
    tokio::spawn(async move {
        // Keep the watcher alive for the life of the task.
        let _watcher = watcher;
        let debounce = Duration::from_millis(500);
        let mut pending: BTreeSet<String> = BTreeSet::new();
        loop {
            let ev = match rx.recv().await {
                Some(e) => e,
                None => break,
            };
            collect(&ev, &mut pending);
            // Coalesce the burst a single copy produces into one event.
            while let Ok(Some(e)) = tokio::time::timeout(debounce, rx.recv()).await {
                collect(&e, &mut pending);
            }
            if !pending.is_empty() {
                let names: Vec<String> = std::mem::take(&mut pending).into_iter().collect();
                st.bus
                    .emit("books", json!({"changed": names, "count": names.len()}));
            }
        }
    });
}

fn collect(ev: &notify::Event, out: &mut BTreeSet<String>) {
    for p in &ev.paths {
        if p.extension()
            .is_some_and(|x| x.to_string_lossy().to_lowercase() == "epub")
        {
            if let Some(n) = p.file_name() {
                out.insert(n.to_string_lossy().to_string());
            }
        }
    }
}
