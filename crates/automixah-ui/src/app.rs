//! The eframe app shell: top bar + placeholder canvas.
//!
//! Runtime state (`UiState`) lives here, separate from the DI container
//! (`Services`), which is cloned in at construction and never mutated.

use eframe::egui;

use crate::services::Services;

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
    /// Last audio-time position of the pointer while dragging.
    drag_last_seconds: Option<f32>,
    /// Last frame instant for drag-velocity computation.
    last_frame_time: Option<std::time::Instant>,
    /// Shared PCM for the audio thread.
    pcm: Option<std::sync::Arc<Vec<f32>>>,
    /// Debounced save: the (hash, grid) to flush 500 ms after the last edit.
    pending_save: Option<(
        automixah_engine::timeline::types::TrackHash,
        crate::grid::EditableGrid,
    )>,
    /// Status line shown in the top bar.
    status: String,
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
            drag_last_seconds: None,
            last_frame_time: None,
            pcm: None,
            pending_save: None,
            status: "open a track to begin".to_owned(),
        }
    }
}

impl AutomixahUiApp {
    /// Marks the current grid dirty for the debounced save.
    fn schedule_save(&mut self) {
        if let Some(track) = self.track.as_ref() {
            self.pending_save = Some((track.hash.clone(), self.edit_grid));
        }
    }

    /// Flushes a pending save once it has been stable for 500 ms.
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
        let status = self.status.clone();
        self.services.handle.spawn(async move {
            match store.put(&hash, &grid).await {
                Ok(()) => {}
                Err(report) => eprintln!("grid save failed: {report:?} — {status}"),
            }
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
    /// Sends the current scrub command to the audio thread.
    fn push_command(&mut self) {
        let cmd = self.scrub.command();
        if let Some(engine) = self.engine.as_ref() {
            *engine.command.lock() = cmd;
        }
    }
}

impl eframe::App for AutomixahUiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.scrub.toggle_play();
            self.push_command();
        }
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let open = ui.button("Open…");
                if open.clicked() {
                    match crate::track::open_pick(&self.services) {
                        Ok(Some(track)) => {
                            let peaks = crate::audio::peaks::Peaks::build(
                                &track.audio.samples,
                                track.audio.sample_rate,
                            );
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
                        Ok(None) => {}
                        Err(report) => self.status = format!("open failed: {report:#}"),
                    }
                }
                ui.separator();
                ui.label(&self.status);
            });
        });

        egui::SidePanel::right("grid_controls").show(ctx, |ui| {
            let end = self.track.as_ref().map_or(0.0, |t| t.duration_seconds);
            ui.horizontal(|ui| {
                let cursor = self.cursor_time;
                if let Some(c) = cursor {
                    if ui.button("snap beat → cursor").clicked() {
                        self.edit_grid.snap_nearest_beat(c);
                    }
                    if ui.button("set downbeat @ cursor").clicked() {
                        self.edit_grid.set_downbeat_at(c);
                    }
                }
            });
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

            let (response, rect, sample_rate) =
                crate::view::waveform::show(ui, peaks, &mut self.view, None);
            let seconds_per_pixel = self.view.frames_per_pixel / sample_rate;
            let time_at_left = self.view.left_frame / sample_rate;
            let pointer_time = response
                .hover_pos()
                .map(|p| time_at_left + (p.x - rect.left()) * seconds_per_pixel);
            self.cursor_time = pointer_time;

            let frame_dt = self.last_frame_time.map_or(1.0 / 60.0, |t| {
                let dt = t.elapsed().as_secs_f32();
                if dt > 0.0 { dt } else { 1.0 / 240.0 }
            });
            if response.drag_started_by(egui::PointerButton::Primary) {
                self.scrub.drag_start();
                self.drag_last_seconds = pointer_time;
            }
            if response.dragged_by(egui::PointerButton::Primary) {
                if let (Some(now), Some(prev)) = (pointer_time, self.drag_last_seconds) {
                    self.scrub.drag_move(now - prev, frame_dt);
                }
                self.drag_last_seconds = pointer_time;
            }
            if response.drag_stopped_by(egui::PointerButton::Primary) {
                self.scrub.drag_end();
                self.drag_last_seconds = None;
                // Jump the engine to the release position.
                if let (Some(t), Some(engine)) = (pointer_time, self.engine.as_ref()) {
                    *engine.playhead().seek.write() = Some(t * sample_rate);
                }
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

            if let Some(engine) = self.engine.as_ref() {
                let pos = *engine.playhead().position.read() / sample_rate;
                let x = rect.left() + (pos - time_at_left) / seconds_per_pixel;
                let x = x.clamp(rect.left(), rect.right());
                painter.line_segment(
                    [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 210, 60)),
                );
            }
        });

        self.push_command();
        self.flush_save_if_due();
        self.last_frame_time = Some(std::time::Instant::now());

        // Keep the UI live while a track is loaded (playhead ticking).
        if self.track.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(16));
        }
    }
}
