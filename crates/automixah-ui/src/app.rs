//! The eframe app shell: top bar + placeholder canvas.
//!
//! Runtime state (`UiState`) lives here, separate from the DI container
//! (`Services`), which is cloned in at construction and never mutated.

use djcore::decoder::DecoderRegistry;
use eframe::egui;

use crate::bus::Event;
use crate::playlist::Contents;
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

    /// Pointer x on the previous drag frame; deltas are measured per frame
    /// so the waveform/grid tracks the cursor 1:1 (stops when it stops).
    drag_last_x: Option<f32>,
    /// View position (source frames) driven directly by the pointer while
    /// scrub-dragging. The audio speed is clamped (vinyl feel); the view
    /// must still track the cursor 1:1 at any zoom, so during a drag the
    /// view follows this accumulation, not the audio thread.
    drag_view_frame: Option<f32>,
    /// UI-owned session analysis cache (hash → detected grid); mutated
    /// only when applying events — background tasks receive entries as
    /// message inputs and report grids via the bus.
    analysis: crate::analysis::AnalysisCache,
    /// When the playhead position last changed (for extrapolation).
    position_updated: Option<std::time::Instant>,
    /// The position value at that instant.
    position_at_update: f64,
    /// Last frame instant for drag-velocity computation.
    last_frame_time: Option<std::time::Instant>,
    /// Shared PCM for the audio thread.
    pcm: Option<std::sync::Arc<Vec<f32>>>,
    /// Off-thread load in flight; drained each frame.
    loading: Option<std::sync::mpsc::Receiver<crate::track::LoadEvent>>,
    /// Dirty grid to flush on the next frame (immediate save).
    pending_save: Option<(
        automixah_engine::timeline::types::TrackHash,
        crate::grid::EditableGrid,
    )>,
    /// Status line shown in the top bar.
    status: String,
    /// Playlist section state (playlists + selected rows).
    pub(crate) playlist_state: crate::playlist::PlaylistState,
    /// Single-worker analysis queue (jobs in; bus events out).
    pub(crate) playlist_queue: crate::playlist::queue::AnalysisQueue,
    /// The UI event bus: every async outcome lands here; `update`
    /// drains it and applies events (the sole state mutation path).
    pub(crate) bus: crate::bus::EventBus,
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
    pub fn new(services: Services, bus: crate::bus::EventBus) -> Self {
        let playlist_queue =
            crate::playlist::queue::AnalysisQueue::spawn(services.clone(), bus.sender());
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
            analysis: crate::analysis::AnalysisCache::default(),
            scrub: crate::audio::scrub_state::ScrubMachine::new(1.0),
            engine: None,
            drag_mode: DragMode::None,
            drag_last_x: None,
            drag_view_frame: None,
            position_updated: None,
            position_at_update: 0.0,
            last_frame_time: None,
            loading: None,
            pcm: None,
            pending_save: None,
            status: "pick a track from the playlist to begin".to_owned(),
            playlist_state: crate::playlist::PlaylistState::default(),
            playlist_queue,
            bus,
        }
    }

    /// Spawns the startup playlist-list load (one `PlaylistsLoaded`).
    pub fn spawn_startup_load(&self) {
        let store = self.services.playlist_store.clone();
        let tx = self.bus.sender();
        self.services.runtime.handle().spawn(async move {
            match store.list_playlists().await {
                Ok(playlists) => {
                    let _ = tx.send(Event::PlaylistsLoaded(playlists));
                }
                Err(report) => {
                    let _ = tx.send(Event::CommandFailed(format!("list playlists: {report:#}")));
                }
            }
        });
    }

    /// Spawns a contents fetch for the selected playlist: the event
    /// applier replaces the contents when it lands.
    fn spawn_contents_load(&self, playlist_id: i64) {
        let store = self.services.playlist_store.clone();
        let grid_store = self.services.grid_store.clone();
        let tx = self.bus.sender();
        self.services.runtime.handle().spawn(async move {
            let persisted = match store.tracks_for(playlist_id).await {
                Ok(rows) => rows,
                Err(report) => {
                    let _ = tx.send(Event::RowsLoadFailed {
                        playlist_id,
                        message: format!("{report:#}"),
                    });
                    return;
                }
            };
            // Grid join: the grid store is the source of truth for both
            // backends; the overlay fills rows the join left null.
            let joined = join_grids(grid_store, persisted).await;
            let (rows, _reenqueue) = rows_from_persisted(joined);
            let _ = tx.send(Event::RowsLoaded { playlist_id, rows });
        });
    }
}

/// Builds UI rows from persisted tracks: complete rows load Ready;
/// incomplete ones (missing grid/key/duration) return their re-enqueue
/// jobs.
#[cfg(test)]
#[must_use]
pub(crate) fn rows_from_persisted_for_test(
    persisted: Vec<crate::playlist::store::PersistedTrack>,
) -> (
    Vec<crate::playlist::PlaylistRow>,
    Vec<(crate::playlist::queue::RowId, std::path::PathBuf)>,
) {
    rows_from_persisted(persisted)
}

#[must_use]
fn rows_from_persisted(
    persisted: Vec<crate::playlist::store::PersistedTrack>,
) -> (
    Vec<crate::playlist::PlaylistRow>,
    Vec<(crate::playlist::queue::RowId, std::path::PathBuf)>,
) {
    #[expect(clippy::cast_possible_truncation, reason = "f64 tag to f32 display")]
    let to_f32 = |d: f64| d as f32;
    let mut rows = Vec::with_capacity(persisted.len());
    let mut reenqueue = Vec::new();
    for track in persisted {
        let row_id = crate::playlist::queue::RowId(track.id);
        let key = track.grid.as_ref().and_then(|g| g.key.clone());
        let complete = track.grid.is_some() && key.is_some() && track.duration.is_some();
        let path = std::path::PathBuf::from(&track.added_path);
        if !complete {
            reenqueue.push((row_id, path.clone()));
        }
        rows.push(crate::playlist::PlaylistRow {
            row_id,
            position: track.position,
            path,
            hash: Some(track.track_hash.clone()),
            title: track.title.clone(),
            artist: track.artist.clone(),
            bpm: track.grid.as_ref().map(|g| g.grid_bpm),
            key,
            duration: track.duration.map(to_f32),
            status: if complete {
                crate::playlist::RowStatus::Ready
            } else {
                crate::playlist::RowStatus::Queued
            },
        });
    }
    (rows, reenqueue)
}

/// Overlays each persisted track's grid from the grid store.
async fn join_grids(
    grid_store: crate::store::GridStoreService,
    persisted: Vec<crate::playlist::store::PersistedTrack>,
) -> Vec<crate::playlist::store::PersistedTrack> {
    let mut joined = Vec::with_capacity(persisted.len());
    for track in persisted {
        let grid = grid_store.get(&track.track_hash).await.ok().flatten();
        let mut track = track;
        if track.grid.is_none() {
            track.grid = grid;
        }
        joined.push(track);
    }
    joined
}

impl AutomixahUiApp {
    /// Marks the current grid dirty; flushed on the next frame.
    fn schedule_save(&mut self) {
        if let Some(track) = self.track.as_ref() {
            self.pending_save = Some((track.hash.clone(), self.edit_grid));
        }
    }

    /// Flushes a pending save immediately; the spawned task reports back
    /// through the bus.
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
            // Manual edits carry no key — the COALESCE upsert preserves
            // whatever analysis stored.
            key: None,
        };
        let tx = self.bus.sender();
        self.services.runtime.handle().spawn(async move {
            let event = match store.put(&hash, &grid).await {
                Ok(()) => Event::GridSaved(hash.0.clone()),
                Err(report) => Event::GridSaveFailed(format!("{report:#}")),
            };
            let _ = tx.send(event);
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
                // unit_speed: 1× in source frames; RateFolder does the
                // single rate conversion to the device inside the engine.
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

    /// Drains the load channel, forwarding to the bus so `apply` stays
    /// the single mutation path. Stages become status events; `Done`
    /// becomes the terminal `LoadDone`.
    fn poll_loading(&mut self) {
        let Some(rx) = self.loading.take() else {
            return;
        };
        let mut terminal = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                crate::track::LoadEvent::Stage(stage) => {
                    self.bus.send(Event::LoadStage(stage));
                }
                crate::track::LoadEvent::Done(payload) => terminal = Some(payload),
            }
        }
        match terminal {
            None => self.loading = Some(rx),
            Some(boxed) => self.bus.send(Event::LoadDone(boxed)),
        }
    }
}

impl AutomixahUiApp {
    /// Pointer x movement since the previous drag frame, in points.
    ///
    /// Measured per frame against the last pointer position, so the dragged
    /// quantity tracks the cursor exactly: it moves when the cursor moves
    /// and stops when the cursor stops.
    fn pointer_drag_delta(&mut self, response: &egui::Response) -> f32 {
        let Some(pos) = response.interact_pointer_pos() else {
            return 0.0;
        };
        let dx = self.drag_last_x.map_or(0.0, |last| pos.x - last);
        self.drag_last_x = Some(pos.x);
        dx
    }

    /// Ends the drag gesture and releases the confined cursor.
    fn end_drag_gesture(&mut self, ctx: &egui::Context) {
        self.drag_mode = DragMode::None;
        self.drag_last_x = None;
        self.drag_view_frame = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(
            egui::viewport::CursorGrab::None,
        ));
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
        self.analysis.invalidate(&track.hash);
        let hash = track.hash.clone();
        let store = self.services.grid_store.clone();
        self.services.runtime.handle().spawn(async move {
            let _ = store.delete(&hash).await;
        });
        self.peaks = None;
        self.engine = None;
        self.loading = Some(crate::track::spawn_load(
            &self.services,
            track.path.clone(),
            None,
        ));
    }

    /// Drains the bus under the frame budget, applying each event.
    fn drain_bus(&mut self) {
        let mut pending_enqueue: Vec<crate::playlist::queue::QueueJob> = Vec::new();
        let mut derive = |event: &Event| {
            // Derive worker jobs at apply time: rows arriving Queued (from
            // a contents load or an add) get analysis jobs immediately.
            match event {
                Event::RowsLoaded { playlist_id, rows } => {
                    for row in rows {
                        if row.status == crate::playlist::RowStatus::Queued
                            && let Some(hash) = row.hash.clone()
                        {
                            pending_enqueue.push(crate::playlist::queue::QueueJob {
                                row_id: row.row_id,
                                playlist_id: *playlist_id,
                                path: row.path.clone(),
                                hash,
                            });
                        }
                    }
                }
                Event::RowAdded { playlist_id, row }
                    if row.status == crate::playlist::RowStatus::Queued
                        && let Some(hash) = row.hash.clone() =>
                {
                    pending_enqueue.push(crate::playlist::queue::QueueJob {
                        row_id: row.row_id,
                        playlist_id: *playlist_id,
                        path: row.path.clone(),
                        hash,
                    });
                }
                _ => {}
            }
        };
        let mut events = Vec::new();
        self.bus.drain(|event| events.push(event));
        for event in events {
            derive(&event);
            self.apply(event);
        }
        for job in pending_enqueue {
            self.playlist_queue.enqueue(job);
        }
    }

    /// The single frontend mutation path: applies drained bus events to
    /// runtime state. Everything async arrives here.
    fn apply(&mut self, event: Event) {
        match event {
            Event::PlaylistsLoaded(_)
            | Event::PlaylistCreated(_)
            | Event::PlaylistRenamed { .. }
            | Event::PlaylistDeleted(_)
            | Event::RowsLoaded { .. }
            | Event::RowsLoadFailed { .. }
            | Event::RowAdded { .. }
            | Event::RowRemoved { .. }
            | Event::RowsReordered { .. }
            | Event::RowAnalyzing { .. }
            | Event::RowReady { .. }
            | Event::RowFailed { .. }
            | Event::DuplicateSkipped { .. }
            | Event::AddStarted { .. } => {
                self.playlist_state.apply(&event);
            }
            Event::LoadStage(stage) => {
                self.status = match stage {
                    crate::track::LoadStage::Hashing => "hashing…".to_owned(),
                    crate::track::LoadStage::Decoding => "decoding…".to_owned(),
                    crate::track::LoadStage::Analyzing => "analyzing…".to_owned(),
                    crate::track::LoadStage::CacheHit => "cached analysis…".to_owned(),
                };
            }
            Event::LoadDone(boxed) => self.apply_load_done(*boxed),
            Event::GridSaved(hash) => {
                self.status = format!("grid saved ({hash:.8})");
            }
            Event::GridSaveFailed(message) => {
                self.status = format!("\u{26a0} save failed: {message}");
            }
            Event::CommandFailed(_) => {
                self.playlist_state.apply(&event);
                self.status = format!(
                    "\u{26a0} {}",
                    match &event {
                        Event::CommandFailed(message) => message.as_str(),
                        _ => unreachable!("matched above"),
                    }
                );
            }
        }
    }

    /// Applies a terminal load outcome to the editor state.
    fn apply_load_done(
        &mut self,
        payload: Result<(crate::track::LoadedTrack, crate::audio::peaks::Peaks), String>,
    ) {
        match payload {
            Ok((mut track, peaks)) => {
                // The detected grid lands in the UI-owned session cache so
                // the next open of this content starts at CacheHit.
                self.analysis.put(track.hash.clone(), track.grid.clone());
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
            Err(msg) => self.status = format!("\u{26a0} load failed: {msg}"),
        }
    }

    /// Applies the playlist panel's collected user intents.
    ///
    /// Store round-trips spawn on the runtime and report back as events;
    /// the panel never mutates state itself.
    fn handle_panel_actions(&mut self, actions: crate::playlist::view::PanelActions) {
        use crate::playlist::view::PanelAction;
        for action in actions.actions {
            match action {
                PanelAction::SelectPlaylist(id) => {
                    self.playlist_state.selected = Some(id);
                    self.playlist_state.contents = crate::playlist::Contents::Loading;
                    self.spawn_contents_load(id);
                }
                PanelAction::NewPlaylist => self.create_playlist(),
                PanelAction::RenamePlaylist { id, name } => self.rename_playlist(id, name),
                PanelAction::DeletePlaylist(id) => self.delete_playlist(id),
                PanelAction::AddTracks => self.add_tracks_dialog(),
                PanelAction::LoadRow(row_id) => self.load_row(row_id),
                PanelAction::MoveRow { from, to } => self.move_row_persist(from, to),
                PanelAction::RemoveRow { row, .. } => self.remove_row_persist(row),
            }
        }
    }

    /// Creates a playlist named `Playlist N` and reports back on the bus.
    fn create_playlist(&mut self) {
        let store = self.services.playlist_store.clone();
        let tx = self.bus.sender();
        self.services.runtime.handle().spawn(async move {
            let n = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.subsec_millis());
            match store.create_playlist(&format!("Playlist {n}")).await {
                Ok(summary) => {
                    let _ = tx.send(Event::PlaylistCreated(summary));
                }
                Err(report) => {
                    let _ = tx.send(Event::CommandFailed(format!("create playlist: {report:#}")));
                }
            }
        });
    }

    /// Renames a playlist; the panel's context menu carries the field.
    fn rename_playlist(&mut self, id: i64, name: String) {
        let store = self.services.playlist_store.clone();
        let tx = self.bus.sender();
        self.services.runtime.handle().spawn(async move {
            match store.rename_playlist(id, &name).await {
                Ok(()) => {
                    let _ = tx.send(Event::PlaylistRenamed { id, name });
                }
                Err(report) => {
                    let _ = tx.send(Event::CommandFailed(format!("rename playlist: {report:#}")));
                }
            }
        });
    }

    /// Deletes a playlist (no confirm; re-adding is cheap).
    fn delete_playlist(&mut self, id: i64) {
        let store = self.services.playlist_store.clone();
        let tx = self.bus.sender();
        self.services.runtime.handle().spawn(async move {
            match store.delete_playlist(id).await {
                Ok(()) => {
                    let _ = tx.send(Event::PlaylistDeleted(id));
                }
                Err(report) => {
                    let _ = tx.send(Event::CommandFailed(format!("delete playlist: {report:#}")));
                }
            }
        });
    }

    /// Opens the multi-select file dialog; each picked file becomes an
    /// add-track task (hash → probe → duplicate check → insert → rowid
    /// event → analysis enqueue on apply).
    fn add_tracks_dialog(&mut self) {
        let Some(playlist_id) = self.playlist_state.selected else {
            return;
        };
        let registry = DecoderRegistry::with_symphonia();
        let extensions = registry.supported_extensions();
        let paths = rfd::FileDialog::new()
            .set_title("Add tracks to playlist")
            .add_filter("audio", &extensions)
            .pick_files();
        let Some(paths) = paths else {
            return;
        };
        self.bus.send(Event::AddStarted {
            playlist_id,
            count: paths.len(),
        });
        for path in paths {
            self.spawn_add_track(playlist_id, path);
        }
    }

    /// One add-track task per file; the row appears on its insert event.
    fn spawn_add_track(&self, playlist_id: i64, path: std::path::PathBuf) {
        let services = self.services.clone();
        let tx = self.bus.sender();
        let path_display = path.display().to_string();
        let handle = services.runtime.handle().clone();
        handle.spawn(async move {
            let event = match add_track_task(&services, playlist_id, &path).await {
                Ok(Some(row)) => Event::RowAdded { playlist_id, row },
                Ok(None) => Event::DuplicateSkipped {
                    playlist_id,
                    path: path_display,
                },
                Err(message) => Event::CommandFailed(format!("add track: {message}")),
            };
            let _ = tx.send(event);
        });
    }

    /// Loads a ready row's file into the grid editor.
    fn load_row(&mut self, row_id: crate::playlist::queue::RowId) {
        let Contents::Loaded(rows) = &self.playlist_state.contents else {
            return;
        };
        let Some(row) = rows.iter().find(|r| r.row_id == row_id && r.is_ready()) else {
            return;
        };
        self.track = None;
        self.peaks = None;
        self.engine = None;
        // Hash may still be unknown (odd store states); a load then
        // simply proceeds without the session cache.
        let cached = row
            .hash
            .as_ref()
            .and_then(|h| self.analysis.get(h).cloned());
        self.loading = Some(crate::track::spawn_load(
            &self.services,
            row.path.clone(),
            cached,
        ));
    }

    /// Splices rows locally (instant visual feedback) and persists the
    /// new order; the store's confirmation event re-asserts order.
    fn move_row_persist(
        &mut self,
        from: crate::playlist::queue::RowId,
        to: crate::playlist::queue::RowId,
    ) {
        let Some(playlist_id) = self.playlist_state.selected else {
            return;
        };
        crate::playlist::move_row(&mut self.playlist_state, from, to);
        let Contents::Loaded(rows) = &self.playlist_state.contents else {
            return;
        };
        let hashes: Option<Vec<_>> = rows.iter().map(|r| r.hash.clone()).collect();
        let Some(hashes) = hashes else {
            // A row without a hash (still queued) cannot reorder yet.
            return;
        };
        let store = self.services.playlist_store.clone();
        let tx = self.bus.sender();
        let row_ids: Vec<_> = rows.iter().map(|r| r.row_id).collect();
        self.services.runtime.handle().spawn(async move {
            match store.reorder(playlist_id, &hashes).await {
                Ok(()) => {
                    let _ = tx.send(Event::RowsReordered {
                        playlist_id,
                        row_ids,
                    });
                }
                Err(report) => {
                    let _ = tx.send(Event::CommandFailed(format!("reorder: {report:#}")));
                }
            }
        });
    }

    /// Removes a row: local splice plus a persisted removal.
    fn remove_row_persist(&mut self, row: crate::playlist::queue::RowId) {
        let Some(playlist_id) = self.playlist_state.selected else {
            return;
        };
        let Some((position, _path)) = crate::playlist::remove_row(&mut self.playlist_state, row)
        else {
            return;
        };
        let store = self.services.playlist_store.clone();
        let tx = self.bus.sender();
        self.services.runtime.handle().spawn(async move {
            match store.remove_track(playlist_id, position).await {
                Ok(()) => {
                    let _ = tx.send(Event::RowRemoved {
                        playlist_id,
                        row_id: row,
                    });
                }
                Err(report) => {
                    let _ = tx.send(Event::CommandFailed(format!("remove track: {report:#}")));
                }
            }
        });
    }
}

/// One add-track task's body: hash → probe → duplicate check → insert.
/// Returns the new row (with its store-minted id) or `None` for a
/// duplicate (skipped silently — no row, no queue job).
async fn add_track_task(
    services: &Services,
    playlist_id: i64,
    path: &std::path::Path,
) -> Result<Option<crate::playlist::PlaylistRow>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let hash =
        automixah_engine::timeline::types::TrackHash(crate::playlist::queue::hex_sha256(&bytes));
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_lowercase();
    let probed = djcore::decoder::meta::probe_metadata(&bytes, &extension).ok();
    let (fallback_artist, fallback_title) = path.file_stem().and_then(|s| s.to_str()).map_or(
        (String::new(), String::new()),
        djcore::decoder::meta::filename_fallback,
    );
    let title = probed
        .as_ref()
        .and_then(|t| t.title.clone())
        .unwrap_or(fallback_title);
    let artist = probed
        .as_ref()
        .and_then(|t| t.artist.clone())
        .unwrap_or(fallback_artist);
    let duration = probed
        .as_ref()
        .and_then(|t| t.duration_seconds)
        .map(f64::from);

    if services
        .playlist_store
        .contains_hash(playlist_id, &hash)
        .await
        .map_err(|report| format!("{report:#}"))?
    {
        return Ok(None);
    }
    let id = services
        .playlist_store
        .ensure_track(
            playlist_id,
            &hash,
            &path.display().to_string(),
            &title,
            &artist,
            duration,
        )
        .await
        .map_err(|report| format!("{report:#}"))?;
    #[expect(clippy::cast_possible_truncation, reason = "f64 tag to f32 display")]
    let duration_f32 = duration.map(|d| d as f32);
    Ok(Some(crate::playlist::PlaylistRow {
        row_id: crate::playlist::queue::RowId(id),
        position: i64::MAX, // appended; renumbered on apply
        path: path.to_owned(),
        hash: Some(hash.clone()),
        title,
        artist,
        bpm: None,
        key: None,
        duration: duration_f32,
        status: crate::playlist::RowStatus::Queued,
    }))
}

impl eframe::App for AutomixahUiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The bus drain is the frame's first act: render with confirmed
        // state only. Leftover events (over the 10 ms budget) land next
        // frame — the drain requests a repaint in that case.
        self.drain_bus();
        self.poll_loading();
        // Bottom panel first: it registers before CentralPanel claims
        // the remaining space.
        let actions = crate::playlist::view::panel(ctx, &mut self.playlist_state);
        self.handle_panel_actions(actions);
        if ctx.input(|i| i.key_pressed(egui::Key::Space)) {
            self.scrub.toggle_play();
            self.push_command();
        }
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
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
                    if ui.button("snap beat @ cursor").clicked() {
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
                    ui.weak("no track loaded — pick one from the playlist below");
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
                // Phrase density at this zoom: beats per white line.
                let stride = crate::view::grid::white_stride(
                    self.edit_grid.beat_seconds(),
                    zoom / self
                        .track
                        .as_ref()
                        .map_or(44_100.0, |t| t.audio.sample_rate as f32),
                );
                ui.weak(format!("white line: every {stride} beats"));
            });
            self.view.frames_per_pixel = zoom;

            // Follow the playhead whenever the audio engine exists.
            // Position changes only at audio-callback rate (~ms bursts);
            // extrapolate with the callback-reported speed so the view
            // scrolls smoothly at display frame rate. During a scrub drag
            // the view follows the pointer accumulation instead: the
            // audio speed is ±8-clamped, so at high zoom-out following
            // the audio thread would lag the cursor.
            let follow = if self.drag_mode == DragMode::Scrub && self.drag_view_frame.is_some() {
                self.drag_view_frame.map(f64::from)
            } else {
                self.engine.as_ref().map(|e| {
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
                            .map_or(0.0, |t| f64::from(t.elapsed().as_secs_f32()));
                        raw + f64::from(speed) * elapsed
                    }
                })
            };
            // The display view is pixel-grained; f32 pinning is fine there
            // and keeps `WaveformView` in its f32 domain.
            let follow = follow.map(|frame| frame as f32);
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
                // Confine the cursor to the window so rapid drags keep
                // working at the edge (wayland-safe; no warp needed).
                ctx.send_viewport_cmd(egui::ViewportCommand::CursorGrab(
                    egui::viewport::CursorGrab::Confined,
                ));
                self.drag_last_x = response.interact_pointer_pos().map(|p| p.x);
                if shift_now {
                    self.drag_mode = DragMode::MoveGrid;
                } else {
                    self.drag_mode = DragMode::Scrub;
                    self.scrub.drag_start();
                    // Seed the view accumulation from wherever the view is
                    // right now; pointer deltas drive it from here.
                    self.drag_view_frame = Some(follow.unwrap_or(self.view.left_frame));
                }
            }
            // Per-frame pointer movement; positions track the cursor 1:1.
            let drag_dx = self.pointer_drag_delta(&response);

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
                        // Audio: velocity-driven varispeed from the smoothed
                        // drag speed — the audio thread advances the
                        // position itself (continuous output, no per-frame
                        // seek rebuilds, so no crackle).
                        let frame_dt = ctx.input(|i| i.unstable_dt);
                        self.scrub.drag_move(-drag_dx * seconds_per_pixel, frame_dt);
                        // View: raw pointer accumulation, 1:1 at any zoom.
                        // The audio speed is clamped (vinyl), so at high
                        // zoom-out the view must not follow the audio.
                        if let Some(view_frame) = self.drag_view_frame.as_mut() {
                            *view_frame =
                                drag_view_step(*view_frame, drag_dx, self.view.frames_per_pixel);
                        }
                    }
                    if response.drag_stopped_by(egui::PointerButton::Primary) {
                        self.scrub.drag_end();
                        // Snap audio to the pointer so following resumes
                        // without jumping back to the lagged audio position.
                        if let (Some(frame), Some(engine)) =
                            (self.drag_view_frame, self.engine.as_ref())
                        {
                            *engine.playhead().seek.write() = Some(f64::from(frame));
                            *engine.playhead().position.write() = f64::from(frame);
                        }
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
                *engine.playhead().seek.write() = Some(f64::from(t) * f64::from(sample_rate));
                *engine.playhead().position.write() = f64::from(t) * f64::from(sample_rate);
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
        self.flush_save_if_due();
        self.last_frame_time = Some(std::time::Instant::now());

        // Keep the UI live while a track is loaded (playhead ticking).
        if self.track.is_some() || self.loading.is_some() || self.playlist_state.any_pending() {
            ctx.request_repaint();
        }
    }
}

/// One frame's drag-view accumulation: the raw pointer delta in pixels
/// applied at the current zoom, unclamped. Extracted so the zoom-out
/// guarantee (view tracks the cursor 1:1 regardless of the ±8 scrub
/// speed clamp) is testable without egui.
fn drag_view_step(current: f32, drag_dx: f32, frames_per_pixel: f32) -> f32 {
    current - drag_dx * frames_per_pixel
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given a scrub drag of 1000 px at maximum zoom-out.
    // When the view accumulates one frame's pointer delta.
    // Then the view advances by pixels × frames_per_pixel — far beyond
    // what the ±8-clamped audio scrub could cover in the same interval
    // (the pre-fix behavior: the waveform lagged the cursor).
    #[test]
    fn drag_view_advances_unclamped_beyond_scrub_max() {
        let fpp = crate::view::waveform::FRAMES_PER_PIXEL_MAX;
        let drag_dx = -1000.0; // pointer moved right → view moves forward

        let advanced = drag_view_step(0.0, drag_dx, fpp);

        assert_eq!(advanced, 1000.0 * fpp, "raw 1:1 accumulation");
        // The audio clamp would cap at 8 source-seconds per wall-second;
        // at 44.1 kHz that is 352_800 frames per second — a single frame's
        // accumulation must exceed a full second of clamped scrub.
        assert!(
            advanced > 8.0 * 44_100.0,
            "view outpaces the scrub clamp: {advanced}"
        );
    }

    // Given a file already in the playlist (same content hash).
    // When the add-track task runs for it again.
    // Then it reports a duplicate skip, not an insert or a failure.
    #[test]
    fn duplicate_add_returns_skip_not_failure() {
        let services = crate::playlist::queue::tests::fake_services_for_app();
        let dir = tempfile::tempdir().expect("temp");
        let path = dir.path().join("Tenebrax - Impulse.wav");
        std::fs::write(&path, crate::playlist::queue::tests::wav_bytes(1.0)).expect("write wav");

        let playlist = services
            .runtime
            .block_on(async { services.playlist_store.create_playlist("dup").await })
            .expect("create");

        let first = services
            .runtime
            .block_on(async { add_track_task(&services, playlist.id, &path).await })
            .expect("first add inserts");
        assert!(first.is_some(), "first add creates a row");

        let second = services
            .runtime
            .block_on(async { add_track_task(&services, playlist.id, &path).await })
            .expect("second add is a skip, not an error");
        assert!(second.is_none(), "duplicate resolves to skip");
    }
}
