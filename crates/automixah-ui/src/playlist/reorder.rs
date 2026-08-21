//! FIFO persistence coordinator for playlist reorder requests.
//!
//! Drops remain responsive because the frontend applies them optimistically,
//! while this worker serializes the corresponding store writes and reports
//! every terminal outcome through the single event bus.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;

use automixah_engine::timeline::types::TrackHash;

use crate::bus::Event;
use crate::playlist::store::{PlaylistStoreService, ReorderOutcome};
use crate::services::Services;

struct ReorderRequest {
    playlist_id: i64,
    sequence: u64,
    order: Vec<TrackHash>,
}

/// Handle for the app's single FIFO reorder worker.
#[derive(Debug)]
pub struct ReorderQueue {
    tx: tokio::sync::mpsc::UnboundedSender<ReorderRequest>,
    next_sequence: AtomicU64,
}

impl ReorderQueue {
    /// Starts the worker on the application's long-lived runtime.
    #[must_use]
    pub fn spawn(services: Services, events: Sender<Event>) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        services.runtime.handle().spawn(async move {
            while let Some(request) = rx.recv().await {
                persist_one(&services.playlist_store, &events, request).await;
            }
        });
        Self {
            tx,
            next_sequence: AtomicU64::new(0),
        }
    }

    /// Enqueues a complete post-drop order and returns its request identity.
    ///
    /// The error means the worker has already shut down; callers must not
    /// apply the optimistic order when this happens.
    pub fn enqueue(&self, playlist_id: i64, order: Vec<TrackHash>) -> Result<u64, Vec<TrackHash>> {
        let sequence = self.next_sequence.fetch_add(1, Ordering::Relaxed) + 1;
        self.tx
            .send(ReorderRequest {
                playlist_id,
                sequence,
                order,
            })
            .map(|()| sequence)
            .map_err(|request| request.0.order)
    }
}

async fn persist_one(
    store: &PlaylistStoreService,
    events: &Sender<Event>,
    request: ReorderRequest,
) {
    let playlist_id = request.playlist_id;
    let sequence = request.sequence;
    match store.reorder(playlist_id, &request.order).await {
        Ok(ReorderOutcome::Saved { order }) => {
            let _ = events.send(Event::RowsReordered {
                playlist_id,
                sequence,
                hashes: order,
            });
        }
        Ok(ReorderOutcome::Rejected { order, error }) => {
            let _ = events.send(Event::ReorderFailed {
                playlist_id,
                sequence,
                hashes: order,
                message: format!("reorder: {error:#}"),
            });
        }
        Err(report) => {
            let _ = events.send(Event::ReorderCommandFailed {
                playlist_id,
                sequence,
                message: format!("reorder: {report:#}"),
            });
        }
    }
}
