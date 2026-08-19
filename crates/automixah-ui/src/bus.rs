//! The UI event bus: the single channel every background task or thread
//! reports through, and the pump the frontend drains each frame.
//!
//! Ownership rule: nothing mutates event-derived frontend state except
//! [`Event`]-handling code fed by [`EventBus::drain`]. Tasks call
//! [`EventBus::sender`], do their work, and send exactly one outcome
//! event; waiting is expressed in the frontend's state enums, rendered
//! as spinners.
//!
//! Timing contract: a send schedules a repaint no later than
//! [`DEBOUNCE`] later (bursts coalesce into one window), and each frame
//! drains for at most [`DRAIN_BUDGET`] before rendering with whatever
//! landed — leftovers are picked up on the immediately-scheduled next
//! frame, so no event is ever dropped.

use std::sync::mpsc::{Receiver, Sender};
use std::time::{Duration, Instant};

use parking_lot::Mutex;

use crate::audio::peaks::Peaks;
use crate::playlist::PlaylistRow;
use crate::playlist::queue::{RowId, TrackMeta};
use crate::playlist::store::PlaylistSummary;
use crate::track::{LoadStage, LoadedTrack};

/// Repaint debounce window after a send.
pub const DEBOUNCE: Duration = Duration::from_millis(50);

/// Per-frame drain budget before rendering continues.
pub const DRAIN_BUDGET: Duration = Duration::from_millis(10);

/// One confirmed outcome from a background task or worker thread.
pub enum Event {
    // Editor load pipeline (hash → decode → analyze → peaks).
    /// A load stage transition (drives the status line / editor spinner).
    LoadStage(LoadStage),
    /// Terminal load outcome; the boxed pair keeps the event small.
    LoadDone(Box<Result<(LoadedTrack, Peaks), String>>),
    // Grid saves (debounced flush outcomes).
    /// A manual grid edit was persisted; the string names the hash.
    GridSaved(String),
    /// A grid save failed; the string carries the rendered report.
    GridSaveFailed(String),
    // Playlist list (one load at startup; deltas afterwards).
    /// The full playlist list, sent once at startup.
    PlaylistsLoaded(Vec<PlaylistSummary>),
    /// A playlist was created.
    PlaylistCreated(PlaylistSummary),
    /// A playlist was renamed.
    PlaylistRenamed { id: i64, name: String },
    /// A playlist was deleted.
    PlaylistDeleted(i64),
    // Playlist contents (fetched on selection).
    /// The selected playlist's rows, ready to replace the view.
    RowsLoaded {
        /// Which playlist the rows belong to.
        playlist_id: i64,
        /// Rows in position order, ids minted by the store.
        rows: Vec<PlaylistRow>,
    },
    /// A contents fetch failed.
    RowsLoadFailed {
        /// Which playlist failed to load.
        playlist_id: i64,
        /// Rendered error report.
        message: String,
    },
    // Row deltas.
    /// A track was inserted into a playlist (id minted by the store).
    RowAdded {
        /// Which playlist gained the row.
        playlist_id: i64,
        /// The inserted row, queued or ready.
        row: PlaylistRow,
    },
    /// A row was removed from a playlist.
    RowRemoved {
        /// Which playlist lost the row.
        playlist_id: i64,
        /// Store id of the removed row.
        row_id: RowId,
    },
    /// Rows were reordered; the slice is the new id order.
    RowsReordered {
        /// Which playlist was reordered.
        playlist_id: i64,
        /// New id order, position 0 first.
        row_ids: Vec<RowId>,
    },
    /// An add was skipped because the content is already in the playlist.
    DuplicateSkipped {
        /// Which playlist was the add targeting.
        playlist_id: i64,
        /// The file that was skipped.
        path: String,
    },
    /// An add-track task batch started (drives the Add busy indicator;
    /// clears on the batch's last RowAdded/DuplicateSkipped/CommandFailed).
    AddStarted {
        /// Which playlist the add targets.
        playlist_id: i64,
        /// Files picked in the batch.
        count: usize,
    },
    // Analysis queue (addressed by store rowid).
    /// The worker started this row's job (spinner state).
    RowAnalyzing {
        /// Row that moved to analyzing.
        row_id: RowId,
    },
    /// The row is ready: analysis (or a library hit) produced metadata.
    RowReady {
        /// Row that became ready.
        row_id: RowId,
        /// Full metadata for the row.
        meta: TrackMeta,
    },
    /// The row's analysis failed; the message shows in a tooltip.
    RowFailed {
        /// Row that failed.
        row_id: RowId,
        /// Rendered error report.
        message: String,
    },
    /// A fire-and-forget command failed (former silent `let _ =` writes).
    CommandFailed(String),
}

impl std::fmt::Debug for Event {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            // PCM payloads debug as size only — formatting samples is
            // useless and enormous.
            Self::LoadDone(boxed) => f
                .debug_struct("LoadDone")
                .field("ok", &boxed.is_ok())
                .finish_non_exhaustive(),
            Self::LoadStage(stage) => f.debug_tuple("LoadStage").field(stage).finish(),
            Self::GridSaved(hash) => f.debug_tuple("GridSaved").field(hash).finish(),
            Self::GridSaveFailed(message) => {
                f.debug_tuple("GridSaveFailed").field(message).finish()
            }
            Self::PlaylistsLoaded(playlists) => {
                f.debug_tuple("PlaylistsLoaded").field(playlists).finish()
            }
            Self::PlaylistCreated(summary) => {
                f.debug_tuple("PlaylistCreated").field(summary).finish()
            }
            Self::PlaylistRenamed { id, name } => f
                .debug_struct("PlaylistRenamed")
                .field("id", id)
                .field("name", name)
                .finish(),
            Self::PlaylistDeleted(id) => f.debug_tuple("PlaylistDeleted").field(id).finish(),
            Self::RowsLoaded { playlist_id, rows } => f
                .debug_struct("RowsLoaded")
                .field("playlist_id", playlist_id)
                .field("rows", rows)
                .finish(),
            Self::RowsLoadFailed {
                playlist_id,
                message,
            } => f
                .debug_struct("RowsLoadFailed")
                .field("playlist_id", playlist_id)
                .field("message", message)
                .finish(),
            Self::RowAdded { playlist_id, row } => f
                .debug_struct("RowAdded")
                .field("playlist_id", playlist_id)
                .field("row", row)
                .finish(),
            Self::RowRemoved {
                playlist_id,
                row_id,
            } => f
                .debug_struct("RowRemoved")
                .field("playlist_id", playlist_id)
                .field("row_id", row_id)
                .finish(),
            Self::RowsReordered {
                playlist_id,
                row_ids,
            } => f
                .debug_struct("RowsReordered")
                .field("playlist_id", playlist_id)
                .field("row_ids", row_ids)
                .finish(),
            Self::DuplicateSkipped { playlist_id, path } => f
                .debug_struct("DuplicateSkipped")
                .field("playlist_id", playlist_id)
                .field("path", path)
                .finish(),
            Self::AddStarted { playlist_id, count } => f
                .debug_struct("AddStarted")
                .field("playlist_id", playlist_id)
                .field("count", count)
                .finish(),
            Self::RowAnalyzing { row_id } => f
                .debug_struct("RowAnalyzing")
                .field("row_id", row_id)
                .finish(),
            Self::RowReady { row_id, .. } => f
                .debug_struct("RowReady")
                .field("row_id", row_id)
                .finish_non_exhaustive(),
            Self::RowFailed { row_id, message } => f
                .debug_struct("RowFailed")
                .field("row_id", row_id)
                .field("message", message)
                .finish(),
            Self::CommandFailed(message) => f.debug_tuple("CommandFailed").field(message).finish(),
        }
    }
}

/// Debounce decision: `Some(delay)` when a repaint window must open now,
/// `None` while one is already open. Pure for testing.
fn coalesced_deadline(pending: Option<Instant>, now: Instant) -> Option<Duration> {
    match pending {
        // No window open (or it elapsed): open one.
        None => Some(DEBOUNCE),
        Some(deadline) if deadline <= now => Some(DEBOUNCE),
        // A window is already open; egui's earliest-wins request merging
        // keeps it, so no new request is needed.
        Some(_) => None,
    }
}

/// The bus: cloneable senders for tasks, one receiver + drain for the
/// UI thread. Construct with an [`egui::Context`] in the app; tests
/// construct without one.
pub struct EventBus {
    tx: Sender<Event>,
    rx: Receiver<Event>,
    repaint: Option<egui::Context>,
    /// Open repaint window deadline (anchor of the debounce).
    deadline: Mutex<Option<Instant>>,
}

impl EventBus {
    /// A bus that schedules repaints on `repaint`.
    #[must_use]
    pub fn new(repaint: egui::Context) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx,
            rx,
            repaint: Some(repaint),
            deadline: Mutex::new(None),
        }
    }

    /// A bus with no repaint side effects (tests).
    #[must_use]
    pub fn without_repaint() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            tx,
            rx,
            repaint: None,
            deadline: Mutex::new(None),
        }
    }

    /// A cloneable sender for spawned tasks and worker threads.
    #[must_use]
    pub fn sender(&self) -> Sender<Event> {
        self.tx.clone()
    }

    /// Direct receiver access (tests block on it; the UI drains).
    #[cfg(test)]
    pub(crate) fn receiver_for_test(&self) -> &Receiver<Event> {
        &self.rx
    }

    /// Sends an event and schedules a repaint within [`DEBOUNCE`] (bursts
    /// coalesce: while a window is open, later sends add no request).
    pub fn send(&self, event: Event) {
        let _ = self.tx.send(event);
        let now = Instant::now();
        let mut deadline = self.deadline.lock();
        if let Some(delay) = coalesced_deadline(*deadline, now) {
            *deadline = Some(now + delay);
            if let Some(ctx) = &self.repaint {
                ctx.request_repaint_after(delay);
            }
        }
    }

    /// Drains events into `apply` until the channel is empty or the
    /// [`DRAIN_BUDGET`] elapses. If events remain, an immediate repaint
    /// is requested so the next frame continues grabbing; the receiver
    /// is never cleared, so nothing is dropped.
    pub fn drain(&self, mut apply: impl FnMut(Event)) {
        *self.deadline.lock() = None; // a frame is running; new sends open a fresh window
        let start = Instant::now();
        loop {
            match self.rx.try_recv() {
                Ok(event) => {
                    apply(event);
                    if start.elapsed() >= DRAIN_BUDGET {
                        if let Some(ctx) = &self.repaint {
                            ctx.request_repaint();
                        }
                        return;
                    }
                }
                Err(_) => return,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given no open debounce window.
    // When a coalescing decision is made.
    // Then a full window opens.
    #[test]
    fn coalesced_deadline_opens_window_when_idle() {
        let now = Instant::now();

        let decision = coalesced_deadline(None, now);

        assert_eq!(decision, Some(DEBOUNCE));
    }

    // Given a burst of sends while a window is open.
    // When each send coalesces.
    // Then only the first opens a window; the rest are absorbed.
    #[test]
    fn coalesced_deadline_opens_one_window_per_burst() {
        let now = Instant::now();
        let deadline = now + DEBOUNCE;

        let first = coalesced_deadline(Some(deadline), now);
        let second = coalesced_deadline(Some(deadline), now + Duration::from_millis(10));

        assert_eq!(first, None, "window already open");
        assert_eq!(second, None, "burst absorbed");
        // After the window elapses a new one opens.
        assert_eq!(
            coalesced_deadline(Some(deadline), now + DEBOUNCE + Duration::from_millis(1)),
            Some(DEBOUNCE)
        );
    }

    // Given more events than one frame can apply within the budget.
    // When drained.
    // Then the drain returns with events remaining and (with a repaint
    // context) an immediate repaint was requested.
    #[test]
    fn drain_respects_time_budget_and_keeps_rest() {
        let bus = EventBus::without_repaint();
        let tx = bus.sender();
        let flood = 100_000;
        for i in 0..flood {
            let _ = tx.send(Event::CommandFailed(format!("e{i}")));
        }

        let mut applied = 0usize;
        bus.drain(|_| {
            applied += 1;
            std::thread::sleep(Duration::from_micros(200)); // blow the 10 ms budget
        });

        assert!(applied < flood, "budget-limited, applied {applied}");
        // The remainder is still queued and reachable (never dropped):
        // the fast second drain applies strictly more events and the
        // channel keeps serving them until empty.
        let mut survivors = 0usize;
        bus.drain(|_| survivors += 1); // no sleep: fast drain
        assert!(
            survivors > 0,
            "leftovers kept: {survivors} of {} skipped",
            flood - applied
        );
        // And the channel fully empties with a no-budget-style consumer
        // (drain repeatedly until it stops returning early).
        let mut total = applied + survivors;
        loop {
            let mut batch = 0usize;
            bus.drain(|_| batch += 1);
            if batch == 0 {
                break;
            }
            total += batch;
        }
        assert_eq!(total, flood, "every event eventually applied");
    }
}
