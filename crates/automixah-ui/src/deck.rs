//! The loaded deck: one track's media, playback, and editing surface.
//!
//! A `Deck` exists only while a track is loaded in the editor — it is
//! constructed atomically from a [`LoadOutcome`] and dropped whole on
//! the next load or re-analyze, so no stale PCM, grid, engine, or
//! gesture state can survive a track swap. Decoded PCM lives here and
//! nowhere else; the track database carries tags and analysis only.

use std::sync::Arc;
use std::time::Instant;

use crate::audio::output::OutputEngine;
use crate::audio::scrub_state::ScrubMachine;
use crate::bus::LoadOutcome;
use crate::grid::EditableGrid;
use crate::view::waveform::WaveformView;

/// Locked drag mode (chosen at drag start).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DragMode {
    #[default]
    None,
    /// Left-drag: scrub audio at drag velocity.
    Scrub,
    /// SHIFT+left-drag: move the grid anchor.
    MoveGrid,
}

/// The loaded deck: media + working grid + playback + gesture state.
pub struct Deck {
    /// Which track this deck plays (identity into the track database).
    pub hash: automixah_engine::timeline::types::TrackHash,
    /// Source file path (the re-analyze reload source).
    pub path: std::path::PathBuf,

    // Media — owned here, never in the track database.
    /// Decoded interleaved PCM shared with the audio thread.
    pub pcm: Arc<Vec<f32>>,
    /// Visual peaks for the waveform.
    pub peaks: crate::audio::peaks::Peaks,
    /// Source sample rate (frames per second).
    pub sample_rate: u32,
    /// Duration in seconds (source time).
    duration: f32,

    // Working + playback state, all reset on load.
    /// Live-editable grid (working copy; saves go to the store).
    pub edit_grid: EditableGrid,
    /// Dirty grid + hash to flush on the next frame (immediate save).
    pub pending_save: Option<(automixah_engine::timeline::types::TrackHash, EditableGrid)>,
    /// `true` for one frame after a grid gesture (the app schedules the
    /// save; the deck stays borrow-friendly).
    pub grid_dirty: bool,
    /// cpal output; `None` when audio is unavailable (grid editing
    /// still works; audio methods no-op).
    pub engine: Option<OutputEngine>,
    /// Scrub interaction state machine.
    pub scrub: ScrubMachine,
    /// Waveform zoom/pan state.
    pub view: WaveformView,

    // Gesture state (resets on every deck swap — a mid-drag load ends
    // the drag cleanly).
    /// Locked drag mode (chosen at drag start).
    pub drag_mode: DragMode,
    /// Pointer x on the previous drag frame; deltas are measured per
    /// frame so the waveform/grid tracks the cursor 1:1.
    pub drag_last_x: Option<f32>,
    /// View position (source frames) driven directly by the pointer
    /// while scrub-dragging.
    pub drag_view_frame: Option<f32>,
    /// Waveform hover position in seconds (action target).
    pub cursor_time: Option<f32>,
    /// When the playhead position last changed (for extrapolation).
    pub position_updated: Option<Instant>,
    /// The position value at that instant.
    pub position_at_update: f64,
}

impl Deck {
    /// Builds a complete deck from a load outcome — atomic: the engine
    /// either starts or the deck carries `None` (with a status note
    /// surfaced by the caller); no half-built decks exist.
    ///
    /// # Errors
    ///
    /// Returns an error string when engine construction fails in a way
    /// that should surface (audio remains optional on the deck).
    pub fn new(outcome: LoadOutcome) -> Result<Self, String> {
        #[expect(clippy::cast_precision_loss, reason = "frame count to f32")]
        let duration = outcome.audio.frames() as f32 / outcome.audio.sample_rate as f32;
        let pcm = Arc::new(outcome.audio.samples.clone());
        let engine = match OutputEngine::start(
            Arc::clone(&pcm),
            outcome.audio.sample_rate,
            outcome.audio.channels.max(1) as usize,
            0.0,
        ) {
            Ok(engine) => Some(engine),
            Err(report) => {
                // Audio unavailable: grid editing still works.
                let _ = format!("{report:?}");
                None
            }
        };
        Ok(Self {
            hash: outcome.hash,
            path: outcome.path,
            pcm,
            peaks: outcome.peaks,
            sample_rate: outcome.audio.sample_rate,
            duration,
            edit_grid: EditableGrid::from_grid(&outcome.analysis.grid),
            pending_save: None,
            grid_dirty: false,
            engine,
            // unit_speed: 1× in source frames; RateFolder does the
            // single rate conversion to the device inside the engine.
            scrub: ScrubMachine::new(1.0),
            view: WaveformView::default(),
            drag_mode: DragMode::None,
            drag_last_x: None,
            drag_view_frame: None,
            cursor_time: None,
            position_updated: None,
            position_at_update: 0.0,
        })
    }

    /// Duration in seconds (source time).
    #[must_use]
    pub fn duration_seconds(&self) -> f32 {
        self.duration
    }

    /// Sends the current scrub command to the audio thread (no-op when
    /// audio is unavailable).
    pub fn push_command(&mut self) {
        let cmd = self.scrub.command();
        if let Some(engine) = self.engine.as_ref() {
            *engine.command.lock() = cmd;
        }
    }
}
