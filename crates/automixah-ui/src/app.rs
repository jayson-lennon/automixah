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
            status: "open a track to begin".to_owned(),
        }
    }
}

impl eframe::App for AutomixahUiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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
            self.cursor_time = response
                .hover_pos()
                .map(|p| time_at_left + (p.x - rect.left()) * seconds_per_pixel);
            let painter = ui.painter_at(rect);
            crate::view::grid::paint(
                &painter,
                &self.edit_grid,
                rect,
                seconds_per_pixel,
                time_at_left,
                end,
            );
        });

        // Keep the UI live while a track is loaded (playhead ticking later).
        if self.track.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}
