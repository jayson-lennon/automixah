//! The UI event bus: the single channel every background task or thread
//! reports through, and the pump the frontend drains each frame.
//!
//! Ownership rule: nothing mutates event-derived frontend state except
//! [`Event`]-handling code fed by [`EventBus::drain`]. Tasks call
//! [`EventBus::sender`], do their work, and send exactly one outcome
//! event; waiting is expressed in the frontend's state enums, rendered
//! as spinners.
//!
//! Event dialects: **track events** (`TagsResolved`, `AnalysisStarted`,
//! `AnalysisDone`, `AnalysisFailed`) address tracks by content hash and
//! mutate records in the track database. **Playlist events** mutate
//! ordering only — store rowids never cross this bus. Analysis events
//! serve every playlist row referencing the hash, so one analysis pass
//! covers all references.
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
use crate::playlist::store::PlaylistSummary;
use crate::track::LoadStage;
use crate::tracks::{Analysis, TrackRecord, TrackTags};
use automixah_engine::timeline::types::TrackHash;
use djcore::decoder::DecodeAudio;

/// Repaint debounce window after a send.
pub const DEBOUNCE: Duration = Duration::from_millis(50);

/// Per-frame drain budget before rendering continues.
pub const DRAIN_BUDGET: Duration = Duration::from_millis(10);

/// Terminal outcome of an editor load: everything a fresh [`Deck`] needs.
#[derive(Debug)]
pub struct LoadOutcome {
    /// Identity of the loaded content.
    pub hash: TrackHash,
    /// Source file path (the re-analyze reload source).
    pub path: std::path::PathBuf,
    /// The analysis package as persisted/loaded for this content.
    pub analysis: Analysis,
    /// Decoded interleaved PCM.
    pub audio: DecodeAudio,
    /// Visual peaks for the waveform.
    pub peaks: Peaks,
}

/// One confirmed outcome from a background task or worker thread.
pub enum Event {
    // Editor load pipeline (hash → decode → analyze → peaks).
    /// A load stage transition (drives the status line / editor spinner).
    LoadStage(LoadStage),
    /// Terminal load outcome; the boxed payload keeps the event small.
    LoadDone(Box<Result<LoadOutcome, String>>),
    // Grid saves (debounced flush outcomes).
    /// A manual grid edit was persisted; the grid refreshes the record.
    GridSaved {
        hash: TrackHash,
        grid: crate::grid::EditableGrid,
    },
    /// A grid save failed; the string carries the rendered report.
    GridSaveFailed(String),
    // Cue saves (debounced flush outcomes).
    /// A cue-point edit was persisted; the cues refresh the record.
    CuesSaved {
        hash: TrackHash,
        cues: automixah_engine::timeline::types::CuePoints,
    },
    /// A cue save failed; the string carries the rendered report and the
    /// failed snapshot identifies which in-flight write completed.
    CuesSaveFailed {
        hash: TrackHash,
        cues: automixah_engine::timeline::types::CuePoints,
        message: String,
    },
    // Track events (address tracks by content hash; mutate records).
    /// Tags resolved for a hash (add-task or hydration).
    TagsResolved { hash: TrackHash, tags: TrackTags },
    /// Analysis started for a hash (never sent on a store fast path).
    AnalysisStarted { hash: TrackHash },
    /// Analysis known for a hash (freshly detected or a store fast path).
    AnalysisDone { hash: TrackHash, analysis: Analysis },
    /// Analysis failed for a hash; the message shows in the row tooltip.
    AnalysisFailed { hash: TrackHash, message: String },
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
    /// The selected playlist's contents: ordered hashes plus hydrated
    /// track records for any hash the stores know about.
    RowsLoaded {
        /// Which playlist the rows belong to.
        playlist_id: i64,
        /// Content hashes in position order.
        hashes: Vec<TrackHash>,
        /// Hydrated records (tags + store-known analysis).
        records: Vec<TrackRecord>,
    },
    /// A contents fetch failed.
    RowsLoadFailed {
        /// Which playlist failed to load.
        playlist_id: i64,
        /// Rendered error report.
        message: String,
    },
    // Ordering deltas (row identity is the content hash).
    /// A track was inserted into a playlist.
    RowAdded { playlist_id: i64, hash: TrackHash },
    /// A row was removed from a playlist.
    RowRemoved { playlist_id: i64, hash: TrackHash },
    /// Rows were reordered; the slice is the confirmed order for one request.
    RowsReordered {
        /// Which playlist owns the ordering.
        playlist_id: i64,
        /// FIFO request identity.
        sequence: u64,
        /// Position-ordered hashes.
        hashes: Vec<TrackHash>,
    },
    /// A reorder was rejected; `hashes` is the durable rollback order.
    ReorderFailed {
        /// Which playlist owns the ordering.
        playlist_id: i64,
        /// FIFO request identity.
        sequence: u64,
        /// Durable position-ordered hashes after rollback.
        hashes: Vec<TrackHash>,
        /// Displayable persistence error.
        message: String,
    },
    /// A reorder failed before the backend could provide a durable rollback.
    ReorderCommandFailed {
        /// Which playlist owns the request.
        playlist_id: i64,
        /// FIFO request identity.
        sequence: u64,
        /// Displayable persistence error.
        message: String,
    },
    /// An add was skipped because the content is already in the playlist.
    DuplicateSkipped {
        /// Which playlist was the add targeting.
        playlist_id: i64,
        /// The file that was skipped.
        path: String,
    },
    /// An add-track task batch started (opens the in-flight count window).
    AddStarted {
        /// Files picked in the batch.
        count: usize,
    },
    /// An add-track task failed (its terminal event; distinct from
    /// `CommandFailed` so the in-flight count is never corrupted).
    AddFailed { message: String },
    // Render pipeline (one job at a time; a singleton, so events
    // address nothing).
    /// Staged progress from the active mixdown (throttled upstream).
    RenderProgress { stage: RenderStage },
    /// Terminal: the WAV exists at the requested path.
    RenderDone { out: std::path::PathBuf },
    /// Terminal: the user cancelled; the partial file was removed.
    RenderCancelled,
    /// Terminal: the mixdown failed; the message carries the rendered
    /// report.
    RenderFailed { message: String },
    /// A fire-and-forget command failed.
    CommandFailed(String),
}

/// One progress report from the active mixdown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RenderStage {
    /// Track `done` of `total` decoded from disk.
    Decoding { done: usize, total: usize },
    /// Track `done` of `total` stretched and cue-sliced.
    Stretching { done: usize, total: usize },
    /// Session mixing, `fraction` of total samples in `[0, 1]`.
    Mixing { fraction: f32 },
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
            Self::GridSaved { hash, .. } => f
                .debug_struct("GridSaved")
                .field("hash", &hash)
                .finish_non_exhaustive(),
            Self::GridSaveFailed(message) => {
                f.debug_tuple("GridSaveFailed").field(message).finish()
            }
            Self::CuesSaved { hash, .. } => f
                .debug_struct("CuesSaved")
                .field("hash", hash)
                .finish_non_exhaustive(),
            Self::CuesSaveFailed { message, .. } => {
                f.debug_tuple("CuesSaveFailed").field(message).finish()
            }
            Self::TagsResolved { hash, .. } => f
                .debug_struct("TagsResolved")
                .field("hash", hash)
                .finish_non_exhaustive(),
            Self::AnalysisStarted { hash } => f
                .debug_struct("AnalysisStarted")
                .field("hash", hash)
                .finish(),
            Self::AnalysisDone { hash, .. } => f
                .debug_struct("AnalysisDone")
                .field("hash", hash)
                .finish_non_exhaustive(),
            Self::AnalysisFailed { hash, message } => f
                .debug_struct("AnalysisFailed")
                .field("hash", hash)
                .field("message", message)
                .finish(),
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
            Self::RowsLoaded {
                playlist_id,
                hashes,
                records,
            } => f
                .debug_struct("RowsLoaded")
                .field("playlist_id", playlist_id)
                .field("hashes", hashes)
                .field("records", records)
                .finish(),
            Self::RowsLoadFailed {
                playlist_id,
                message,
            } => f
                .debug_struct("RowsLoadFailed")
                .field("playlist_id", playlist_id)
                .field("message", message)
                .finish(),
            Self::RowAdded { playlist_id, hash } => f
                .debug_struct("RowAdded")
                .field("playlist_id", playlist_id)
                .field("hash", hash)
                .finish(),
            Self::RowRemoved { playlist_id, hash } => f
                .debug_struct("RowRemoved")
                .field("playlist_id", playlist_id)
                .field("hash", hash)
                .finish(),
            Self::RowsReordered {
                playlist_id,
                sequence,
                hashes,
            } => f
                .debug_struct("RowsReordered")
                .field("playlist_id", playlist_id)
                .field("sequence", sequence)
                .field("hashes", hashes)
                .finish(),
            Self::ReorderFailed {
                playlist_id,
                sequence,
                hashes,
                message,
            } => f
                .debug_struct("ReorderFailed")
                .field("playlist_id", playlist_id)
                .field("sequence", sequence)
                .field("hashes", hashes)
                .field("message", message)
                .finish(),
            Self::ReorderCommandFailed {
                playlist_id,
                sequence,
                message,
            } => f
                .debug_struct("ReorderCommandFailed")
                .field("playlist_id", playlist_id)
                .field("sequence", sequence)
                .field("message", message)
                .finish(),
            Self::DuplicateSkipped { playlist_id, path } => f
                .debug_struct("DuplicateSkipped")
                .field("playlist_id", playlist_id)
                .field("path", path)
                .finish(),
            Self::AddStarted { count } => {
                f.debug_struct("AddStarted").field("count", count).finish()
            }
            Self::AddFailed { message } => f.debug_tuple("AddFailed").field(message).finish(),
            Self::RenderProgress { stage } => f
                .debug_struct("RenderProgress")
                .field("stage", stage)
                .finish(),
            Self::RenderDone { out } => f
                .debug_struct("RenderDone")
                .field("out", &out.display().to_string())
                .finish(),
            Self::RenderCancelled => f.debug_tuple("RenderCancelled").finish(),
            Self::RenderFailed { message } => f.debug_tuple("RenderFailed").field(message).finish(),
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
