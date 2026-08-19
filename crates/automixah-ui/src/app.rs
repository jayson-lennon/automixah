//! The eframe app shell: top bar + placeholder canvas.
//!
//! Runtime state (`UiState`) lives here, separate from the DI container
//! (`Services`), which is cloned in at construction and never mutated.

use djcore::decoder::DecoderRegistry;
use eframe::egui;

use crate::services::Services;

/// Locked drag mode (chosen at drag start).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum DragMode {
    #[default]
    None,
    /// Left-drag: scrub audio at drag velocity.
    Scrub,
    /// SHIFT+left-drag: move the grid anchor.
    MoveGrid,
}

/// Terminal state of one grid save, reported to the status line.
enum SaveOutcome {
    Saved(String),
    Failed(String),
}

/// `HH:MM:SS` (UTC) rendering of a unix duration for the status line.
fn format_hhmmss(d: std::time::Duration) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        d.as_secs() / 3600,
        (d.as_secs() / 60) % 60,
        d.as_secs() % 60
    )
}

/// The automixah-ui application.
pub struct AutomixahUiApp {
    /// DI container (paths, grid store, async handle). Clone-cheap.
    services: Services,
    /// Track-dependent runtime state; `None` until a track is opened.
    track: Option<crate::track::LoadedTrack>,
    /// Visual-rate peaks for the loaded track (built at load).
    peaks: Option<crate::audio::peaks::Peaks>,
    /// Waveform zoom/pan state.
    view: crate::view::waveform::WaveformView,
    /// Live-editable grid for the loaded track.
    edit_grid: crate::grid::EditableGrid,
    /// Waveform hover position in seconds (action target).
    cursor_time: Option<f32>,
    /// Scrub interaction state machine.
    scrub: crate::audio::scrub_state::ScrubMachine,
    /// cpal output engine; `None` until a track loads or if audio fails.
    engine: Option<crate::audio::output::OutputEngine>,
    /// Locked drag mode (chosen at drag start).
    drag_mode: DragMode,

    /// Direct-drag anchor: audio position at drag start (seconds).
    drag_accum_position: f32,
    /// Warp anchor: pointer is re-centered here each drag frame so the
    /// cursor never reaches a screen edge (hidden cursor, infinite drag).
    drag_warp_anchor: Option<egui::Pos2>,
    /// When the playhead position last changed (for extrapolation).
    position_updated: Option<std::time::Instant>,
    /// The position value at that instant.
    position_at_update: f32,
    /// Last frame instant for drag-velocity computation.
    last_frame_time: Option<std::time::Instant>,
    /// Shared PCM for the audio thread.
    pcm: Option<std::sync::Arc<Vec<f32>>>,
    /// Off-thread load in flight; drained each frame.
    loading: Option<std::sync::mpsc::Receiver<crate::track::LoadEvent>>,
    /// Grid-save completion channel (sender cloned per flush).
    save_outcomes: (
        std::sync::mpsc::Sender<SaveOutcome>,
        std::sync::mpsc::Receiver<SaveOutcome>,
    ),
    /// Dirty grid to flush on the next frame (immediate save).
    pending_save: Option<(
        automixah_engine::timeline::types::TrackHash,
        crate::grid::EditableGrid,
    )>,
    /// Status line shown in the top bar.
    status: String,
}

/// Integration-test hooks: drive the save path without egui.
#[cfg(any(test, feature = "__test-hooks"))]
impl AutomixahUiApp {
    /// Simulates a loaded track for save-path testing.
    pub fn inject_track_for_test(&mut self, hash: automixah_engine::timeline::types::TrackHash) {
        self.track = Some(crate::track::LoadedTrack {
            path: std::path::PathBuf::from("test.ogg"),
            hash,
            audio: djcore::decoder::DecodeAudio {
                samples: Vec::new(),
                sample_rate: 44_100,
                channels: 2,
            },
            duration_seconds: 60.0,
            grid: djcore::analyzer::BeatGrid::default(),
            grid_source: crate::track::GridSource::Auto,
        });
    }

    /// Applies a grid shift and marks dirty, like the gesture path.
    pub fn test_shift_grid(&mut self, delta: f32) {
        self.edit_grid.shift_by(delta);
        self.schedule_save();
    }

    /// Changes the downbeat phase and marks dirty.
    pub fn test_set_downbeat_phase(&mut self, phase: u8) {
        self.edit_grid.downbeat_phase = phase;
        self.schedule_save();
    }

    /// Flushes a pending save now.
    pub fn flush_save_if_due_for_test(&mut self) {
        self.flush_save_if_due();
    }
}

impl AutomixahUiApp {
    /// Builds the app around the assembled services.
    #[must_use]
    pub fn new(services: Services) -> Self {
        Self {
            services,
            track: None,
            peaks: None,
            view: crate::view::waveform::WaveformView::default(),
            edit_grid: crate::grid::EditableGrid {
                grid_bpm: 120.0,
                anchor_seconds: 0.0,
                downbeat_phase: 0,
            },
            cursor_time: None,
            scrub: crate::audio::scrub_state::ScrubMachine::new(1.0),
            engine: None,
            drag_mode: DragMode::None,
            drag_accum_position: 0.0,
            drag_warp_anchor: None,
            position_updated: None,
            position_at_update: 0.0,
            last_frame_time: None,
            loading: None,
            save_outcomes: std::sync::mpsc::channel(),
            pcm: None,
            pending_save: None,
            status: "open a track to begin".to_owned(),
        }
    }
}

impl AutomixahUiApp {
    /// Marks the current grid dirty; flushed on the next frame.
    fn schedule_save(&mut self) {
        if let Some(track) = self.track.as_ref() {
            self.pending_save = Some((track.hash.clone(), self.edit_grid));
        }
    }

    /// Drains save outcomes into the status line (one per flush).
    fn poll_save_outcomes(&mut self) {
        while let Ok(outcome) = self.save_outcomes.1.try_recv() {
            self.status = match outcome {
                SaveOutcome::Saved(at) => format!("grid saved {at}"),
                SaveOutcome::Failed(msg) => format!("⚠ save failed: {msg}"),
            };
        }
    }

    /// Flushes a pending save immediately; the spawned task reports back
    /// through the outcomes channel.
    fn flush_save_if_due(&mut self) {
        let Some((hash, grid)) = self.pending_save.take() else {
            return;
        };
        let store = self.services.grid_store.clone();
        let grid = crate::store::GridOverride {
            grid_bpm: grid.grid_bpm,
            anchor_seconds: grid.anchor_seconds,
            downbeat_phase: grid.downbeat_phase,
            updated_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64),
        };
        let tx = self.save_outcomes.0.clone();
        self.services.runtime.handle().spawn(async move {
            let _ = tx.send(match store.put(&hash, &grid).await {
                Ok(()) => SaveOutcome::Saved(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or_else(|_| "?".to_owned(), format_hhmmss),
                ),
                Err(report) => SaveOutcome::Failed(format!("{report:#}")),
            });
        });
    }

    /// Rebuilds the output engine for a freshly loaded track.
    fn start_engine(&mut self, track: &crate::track::LoadedTrack) {
        let pcm = std::sync::Arc::new(track.audio.samples.clone());
        self.engine = match crate::audio::output::OutputEngine::start(
            std::sync::Arc::clone(&pcm),
            track.audio.sample_rate,
            track.audio.channels.max(1) as usize,
            0.0,
        ) {
            Ok(engine) => {
                // unit_speed: 1× playback rate-folded to the device.
                self.scrub = crate::audio::scrub_state::ScrubMachine::new(1.0);
                Some(engine)
            }
            Err(report) => {
                self.status = format!("audio unavailable: {report:?}");
                None
            }
        };
        self.pcm = Some(pcm);
    }
}

impl AutomixahUiApp {
    /// Accumulated pointer x movement since the last warp, in points.
    ///
    /// Each drag frame measures the pointer against the warp anchor and then
    /// re-warps the (hidden) cursor back, so the gesture never reaches a
    /// screen edge. Returns 0.0 when no anchor is set (gesture not started).
    fn pointer_delta_since_warp(&mut self, response: &egui::Response) -> f32 {
        let Some(anchor) = self.drag_warp_anchor else {
            return 0.0;
        };
        let Some(pos) = response.interact_pointer_pos() else {
            return 0.0;
        };
        let dx = pos.x - anchor.x;
        // Re-center immediately so the next frame measures only new movement.
        // (If the platform rejects warps, deltas just saturate at the screen
        // edge — no worse than the previous confined-cursor behavior.)
        response
            .ctx
            .send_viewport_cmd(egui::ViewportCommand::CursorPosition(anchor));
        dx
    }

    /// Restores cursor visibility after a drag gesture ends.
    fn end_drag_gesture(&mut self, ctx: &egui::Context) {
        self.drag_mode = DragMode::None;
        self.drag_warp_anchor = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(true));
    }

    /// Sends the current scrub command to the audio thread.
    fn push_command(&mut self) {
        let cmd = self.scrub.command();
        if let Some(engine) = self.engine.as_ref() {
            *engine.command.lock() = cmd;
        }
    }

    /// Re-analyzes the current track: drops the session cache entry and the
    /// stored manual override, clears the waveform, and reloads so the
    /// analyzing stage runs fresh.
    fn reanalyze_current(&mut self) {
        let Some(track) = self.track.take() else {
            return;
        };
        self.services.analysis.invalidate(&track.hash);
        let hash = track.hash.clone();
        let store = self.services.grid_store.clone();
        self.services.runtime.handle().spawn(async move {
            let _ = store.delete(&hash).await;
        });
        self.peaks = None;
        self.engine = None;
        self.loading = Some(crate::track::spawn_load(&self.services, track.path.clone()));
    }

    /// Drains pending load events; applies the track when the load lands.
    fn poll_loading(&mut self) {
        let Some(rx) = self.loading.take() else {
            return;
        };
        let mut terminal = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                crate::track::LoadEvent::Stage(stage) => {
                    self.status = match stage {
                        crate::track::LoadStage::Hashing => "hashing…".to_owned(),
                        crate::track::LoadStage::Decoding => "decoding…".to_owned(),
                        crate::track::LoadStage::Analyzing => "analyzing…".to_owned(),
                        crate::track::LoadStage::CacheHit => "cached analysis…".to_owned(),
                    };
                }
                crate::track::LoadEvent::Done(payload) => terminal = Some(payload),
            }
        }
        match terminal {
            None => self.loading = Some(rx),
            Some(boxed) => match *boxed {
                Ok((mut track, peaks)) => {
                    // Override lookup on the UI thread (single SQLite point-read).
                    if let Ok(Some(override_grid)) =
                        crate::track::apply_stored_override(&self.services, &track.hash)
                    {
                        track.grid = override_grid;
                        track.grid_source = crate::track::GridSource::Manual;
                    }
                    self.edit_grid = crate::grid::EditableGrid::from_grid(&track.grid);
                    self.pending_save = None;
                    self.start_engine(&track);
                    self.status = format!(
                        "loaded {} ({:.1}s, {:.3} BPM, {} visual samples)",
                        track.path.display(),
                        track.duration_seconds,
                        self.edit_grid.grid_bpm,
                        peaks.data.len()
                    );
                    self.view = crate::view::waveform::WaveformView::default();
                    self.peaks = Some(peaks);
                    self.track = Some(track);
                }
                Err(msg) => self.status = format!("⚠ load failed: {msg}"),
            },
        }
    }
}

impl eframe::App for AutomixahUiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_loading();
        if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.scrub.toggle_play();
            self.push_command();
        }
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let open = ui.add_enabled(self.loading.is_none(), egui::Button::new("Open…"));
                if open.clicked() {
                    let registry = DecoderRegistry::with_symphonia();
                    let extensions = registry.supported_extensions();
                    let dialog = rfd::FileDialog::new()
                        .set_title("Open audio track")
                        .add_filter("audio", &extensions);
                    if let Some(path) = dialog.pick_file() {
                        // Clear the current track while the load runs:
                        // the waveform disappears until the new one lands.
                        self.track = None;
                        self.peaks = None;
                        self.engine = None;
                        self.loading = Some(crate::track::spawn_load(&self.services, path));
                    }
                }
                if self.loading.is_some() {
                    ui.spinner();
                }
                let can_reanalyze = self.loading.is_none() && self.track.is_some();
                let reanalyze = ui.add_enabled(can_reanalyze, egui::Button::new("re-analyze"));
                if reanalyze.clicked() {
                    self.reanalyze_current();
                }
                ui.separator();
                ui.label(&self.status);
            });
        });

        egui::SidePanel::right("grid_controls").show(ctx, |ui| {
            let end = self.track.as_ref().map_or(0.0, |t| t.duration_seconds);
            ui.horizontal(|ui| {
                if let Some(c) = self.cursor_time {
                    if ui.button("snap beat → cursor").clicked() {
                        self.edit_grid.snap_nearest_beat(c);
                        self.schedule_save();
                    }
                    if ui.button("set downbeat @ cursor").clicked() {
                        self.edit_grid.set_downbeat_at(c);
                        self.schedule_save();
                    }
                }
            });
            ui.add(
                egui::Slider::new(&mut self.view.playhead_frac, 0.05..=0.95)
                    .text("playhead x")
                    .custom_formatter(|n, _| format!("{n:.0}%")),
            );
            if crate::view::grid::controls(ui, &mut self.edit_grid, end) {
                self.schedule_save();
                self.status = format!(
                    "grid: {:.3} BPM, anchor {:.3} s, phase {}",
                    self.edit_grid.grid_bpm,
                    self.edit_grid.anchor_seconds,
                    self.edit_grid.downbeat_phase
                );
            }
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(peaks) = self.peaks.as_ref() else {
                ui.centered_and_justified(|ui| {
                    ui.weak("no track loaded — use Open… to pick an audio file");
                });
                return;
            };
            let end = self.track.as_ref().map_or(0.0, |t| t.duration_seconds);

            let mut zoom = self.view.frames_per_pixel;
            ui.horizontal(|ui| {
                ui.label("zoom");
                ui.add(
                    egui::Slider::new(
                        &mut zoom,
                        crate::view::waveform::FRAMES_PER_PIXEL_MIN
                            ..=crate::view::waveform::FRAMES_PER_PIXEL_MAX,
                    )
                    .logarithmic(true),
                );
            });
            self.view.frames_per_pixel = zoom;

            // Follow the playhead whenever the audio engine exists.
            // Position changes only at audio-callback rate (~ms bursts);
            // extrapolate with the callback-reported speed so the view
            // scrolls smoothly at display frame rate.
            let follow = self.engine.as_ref().map(|e| {
                let ph = e.playhead();
                let raw = *ph.position.read();
                if raw != self.position_at_update {
                    self.position_at_update = raw;
                    self.position_updated = Some(std::time::Instant::now());
                    raw
                } else {
                    let speed = *ph.speed.read();
                    let elapsed = self
                        .position_updated
                        .map_or(0.0, |t| t.elapsed().as_secs_f32());
                    raw + speed * elapsed
                }
            });
            let (response, rect, sample_rate) =
                crate::view::waveform::show(ui, peaks, &mut self.view, follow);
            let seconds_per_pixel = self.view.frames_per_pixel / sample_rate;
            let time_at_left = self.view.left_frame / sample_rate;
            let pointer_time = response
                .hover_pos()
                .map(|p| time_at_left + (p.x - rect.left()) * seconds_per_pixel);
            // Latch: keep the last valid cursor time so the cursor buttons
            // stay visible when the pointer leaves the waveform.
            if pointer_time.is_some() {
                self.cursor_time = pointer_time;
            }

            // Drag mode locks at drag start: SHIFT → grid move, else scrub.
            let shift_now = ctx.input(|i| i.modifiers.shift);
            if response.drag_started_by(egui::PointerButton::Primary) {
                // Infinite drag: hide the cursor and re-warp it to the
                // anchor every frame, so the pointer never reaches a screen
                // edge and the gesture can continue indefinitely.
                ctx.send_viewport_cmd(egui::ViewportCommand::CursorVisible(false));
                self.drag_warp_anchor = response.interact_pointer_pos();
                if shift_now {
                    self.drag_mode = DragMode::MoveGrid;
                } else {
                    self.drag_mode = DragMode::Scrub;
                    self.scrub.drag_start();
                    self.drag_accum_position = follow.unwrap_or(0.0) / sample_rate;
                }
            }
            // Physical pointer movement since the last warp; the cursor is
            // warped back to the anchor afterwards so it never nears an edge.
            let drag_dx = self.pointer_delta_since_warp(&response);

            match self.drag_mode {
                DragMode::MoveGrid => {
                    if response.dragged_by(egui::PointerButton::Primary) {
                        self.edit_grid.shift_by(drag_dx * seconds_per_pixel);
                        self.schedule_save();
                        self.status = format!(
                            "grid shifted: anchor {:.3} s",
                            self.edit_grid.anchor_seconds
                        );
                    }
                    if response.drag_stopped_by(egui::PointerButton::Primary) {
                        self.end_drag_gesture(ctx);
                    }
                }
                DragMode::Scrub => {
                    if response.dragged_by(egui::PointerButton::Primary) {
                        // Direct position control: the position follows the
                        // accumulated pointer movement 1:1 (like dragging
                        // the waveform strip itself).
                        self.drag_accum_position -= drag_dx * seconds_per_pixel;
                        let target = self.drag_accum_position.clamp(0.0, end);
                        if let Some(engine) = self.engine.as_ref() {
                            *engine.playhead().seek.write() = Some(target * sample_rate);
                            *engine.playhead().position.write() = target * sample_rate;
                        }
                        ctx.request_repaint();
                    }
                    if response.drag_stopped_by(egui::PointerButton::Primary) {
                        self.scrub.drag_end();
                        self.end_drag_gesture(ctx);
                    }
                }
                DragMode::None => {}
            }
            // Plain click (no drag) seeks the playhead; grid untouched.
            // Position is written too so the pinned view re-centers on the
            // very next paint instead of waiting for an audio callback.
            if response.clicked()
                && !shift_now
                && let (Some(t), Some(engine)) = (pointer_time, self.engine.as_ref())
            {
                *engine.playhead().seek.write() = Some(t * sample_rate);
                *engine.playhead().position.write() = t * sample_rate;
                ctx.request_repaint();
            }

            let painter = ui.painter_at(rect);
            crate::view::grid::paint(
                &painter,
                &self.edit_grid,
                rect,
                seconds_per_pixel,
                time_at_left,
                end,
            );

            if self.engine.is_some() {
                // Pinned playhead: fixed x at `playhead_frac` of the viewport.
                let x = rect.left() + self.view.playhead_frac * rect.width();
                painter.line_segment(
                    [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                    egui::Stroke::new(3.0, egui::Color32::from_rgb(255, 210, 60)),
                );
            }
        });

        self.push_command();
        self.poll_save_outcomes();
        self.flush_save_if_due();
        self.last_frame_time = Some(std::time::Instant::now());

        // Keep the UI live while a track is loaded (playhead ticking).
        if self.track.is_some() || self.loading.is_some() {
            ctx.request_repaint();
        }
    }
}
