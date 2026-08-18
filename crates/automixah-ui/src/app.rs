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
                            self.status = format!(
                                "loaded {} ({:.1}s, {:.3} BPM, {} visual samples)",
                                track.path.display(),
                                track.duration_seconds,
                                track.grid.grid_bpm,
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

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(peaks) = self.peaks.as_ref() else {
                ui.centered_and_justified(|ui| {
                    ui.weak("no track loaded — use Open… to pick an audio file");
                });
                return;
            };

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
            crate::view::waveform::show(ui, peaks, &mut self.view, None);
        });

        // Keep the UI live while a track is loaded (playhead ticking later).
        if self.track.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }
    }
}
