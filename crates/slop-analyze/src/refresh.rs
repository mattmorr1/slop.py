//! Coalesced background reindexing. An edit makes a document `Reparsed` at once
//! (`overlay`); this restores exact edges later without blocking any request.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};

use crate::snapshot::{CaptureRequest, Freshness, RepositorySnapshot, SnapshotId};

pub struct Refresher {
    requests: mpsc::Sender<()>,
    /// Snapshots already sent: one whose edits a reindex cannot clear (an indexer
    /// failure memo) must not be requested again, or refresh would loop.
    sent: Arc<Mutex<HashSet<SnapshotId>>>,
}

impl Refresher {
    /// One worker per repository; `on_done` runs after each reindex that succeeded.
    pub fn spawn(repo: PathBuf, index: Option<PathBuf>, on_done: impl Fn() + Send + 'static) -> Self {
        let (requests, pending) = mpsc::channel::<()>();
        std::thread::spawn(move || {
            while pending.recv().is_ok() {
                while pending.try_recv().is_ok() {}
                let request = CaptureRequest { repo: &repo, index: index.as_deref(), policy: None, freshness: Freshness::Reindex };
                match RepositorySnapshot::capture(request) {
                    Ok(_) => on_done(),
                    Err(error) => eprintln!("slop: background reindex failed: {error:#}"),
                }
            }
        });
        Self { requests, sent: Arc::default() }
    }

    /// Queue a reindex if `snapshot` holds edits the index has not seen; returns whether it did.
    pub fn request_if_edited(&self, snapshot: &RepositorySnapshot) -> bool {
        if !snapshot.has_unindexed_edits() {
            return false;
        }
        let fresh = self.sent.lock().map(|mut sent| sent.insert(snapshot.id().clone())).unwrap_or(false);
        fresh && self.requests.send(()).is_ok()
    }
}
