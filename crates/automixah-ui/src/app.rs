//! The eframe app shell: top bar + waveform canvas.
//!
//! Runtime state (`UiState`) lives here, separate from the DI container
//! (`Services`), which is cloned in at construction and never mutated.
//! Track facts live in [`crate::tracks::Tracks`] (the database);
//! playlist ordering in [`crate::playlist::PlaylistState`]; the loaded
//! track's media and playback state in [`crate::deck::Deck`]. All
//! three mutate only through bus-event application in [`Self::apply`]
//! (plus the enqueue derivation in `drain_bus`).

use eframe::egui;

use crate::bus::Event;
use crate::deck::Deck;
use crate::playlist::Contents;
use crate::services::Services;
use crate::tracks::{AnalysisState, TrackRecord};
use automixah_engine::timeline::types::TrackHash;

/// The automixah-ui application.
pub struct AutomixahUiApp {
    /// DI container (paths, grid store, async handle). Clone-cheap.
    services: Services,
    /// The frontend track database (hash → tags + analysis).
    tracks: crate::tracks::Tracks,
    /// Playlist section state (playlists + selected ordering).
    pub(crate) playlist_state: crate::playlist::PlaylistState,
    /// Single-worker analysis queue (jobs in; bus events out).
    pub(crate) playlist_queue: crate::playlist::queue::AnalysisQueue,
    /// The loaded deck; `None` until a track loads (the one lifecycle
    /// Option in the app).
    pub(crate) deck: Option<Deck>,
    /// `true` while an editor load task is in flight (spinner, disables
    /// re-analyze; cleared by `LoadDone`).
    load_in_flight: bool,
    /// A playlist click on a still-pending row: the hash to load once
    /// its analysis lands (the waveform area shows an analysis message
    /// meanwhile). The deck's only armed source.
    pending_deck_load: Option<TrackHash>,
    /// Status line shown in the top bar.
    status: String,
    /// The UI event bus: every async outcome lands here; `update`
    /// drains it and applies events (the sole state mutation path).
    pub(crate) bus: crate::bus::EventBus,
}

impl AutomixahUiApp {
    /// Builds the app around the assembled services.
    #[must_use]
    pub fn new(services: Services, bus: crate::bus::EventBus) -> Self {
        let playlist_queue =
            crate::playlist::queue::AnalysisQueue::spawn(services.clone(), bus.sender());
        Self {
            services,
            tracks: crate::tracks::Tracks::default(),
            playlist_state: crate::playlist::PlaylistState::default(),
            playlist_queue,
            deck: None,
            load_in_flight: false,
            pending_deck_load: None,
            status: "pick a track from the playlist to begin".to_owned(),
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
            let (hashes, records) = hydrate_records(joined);
            let _ = tx.send(Event::RowsLoaded {
                playlist_id,
                hashes,
                records,
            });
        });
    }
}

/// Builds contents hashes + hydrated records from persisted tracks:
/// complete entries (grid + key + duration) carry a `Ready` analysis;
/// incomplete ones stay `Queued` (the enqueue derivation re-enqueues).
#[must_use]
fn hydrate_records(
    persisted: Vec<crate::playlist::store::PersistedTrack>,
) -> (Vec<TrackHash>, Vec<TrackRecord>) {
    let mut hashes = Vec::with_capacity(persisted.len());
    let mut records = Vec::new();
    for track in persisted {
        hashes.push(track.track_hash.clone());
        let key = track.grid.as_ref().and_then(|g| g.key.clone());
        let complete = track.grid.is_some() && key.is_some() && track.duration.is_some();
        if !complete {
            continue;
        }
        let grid = track.grid.expect("checked above");
        let key = key.expect("checked above");
        let duration = track.duration.expect("checked above");
        let editable = crate::grid::EditableGrid {
            grid_bpm: grid.grid_bpm,
            anchor_seconds: grid.anchor_seconds,
            downbeat_phase: grid.downbeat_phase,
        };
        records.push(TrackRecord {
            hash: track.track_hash.clone(),
            tags: crate::tracks::TrackTags {
                title: track.title,
                artist: track.artist,
                path: std::path::PathBuf::from(&track.added_path),
            },
            analysis: AnalysisState::Ready(crate::tracks::Analysis {
                grid: editable.project(),
                bpm: editable.grid_bpm,
                key,
                #[expect(clippy::cast_possible_truncation, reason = "f64 tag to f32 display")]
                duration_seconds: duration as f32,
            }),
        });
    }
    (hashes, records)
}

/// Hydration for tests.
#[cfg(test)]
#[must_use]
pub(crate) fn hydrate_records_for_test(
    persisted: Vec<crate::playlist::store::PersistedTrack>,
) -> (Vec<TrackHash>, Vec<TrackRecord>) {
    hydrate_records(persisted)
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
    /// Flushes a pending grid save from the deck; the spawned task
    /// reports back through the bus.
    fn flush_save_if_due(&mut self) {
        let Some(deck) = self.deck.as_mut() else {
            return;
        };
        let Some((hash, grid)) = deck.pending_save.take() else {
            return;
        };
        deck.edit_grid = grid;
        let store = self.services.grid_store.clone();
        let grid_override = crate::store::GridOverride {
            grid_bpm: grid.grid_bpm,
            anchor_seconds: grid.anchor_seconds,
            downbeat_phase: grid.downbeat_phase,
            updated_at: crate::track::identity::now_unix(),
            // Manual edits carry no key — the COALESCE upsert preserves
            // whatever analysis stored.
            key: None,
        };
        let tx = self.bus.sender();
        self.services.runtime.handle().spawn(async move {
            let event = match store.put(&hash, &grid_override).await {
                Ok(()) => Event::GridSaved { hash, grid },
                Err(report) => Event::GridSaveFailed(format!("{report:#}")),
            };
            let _ = tx.send(event);
        });
    }

    /// Marks the deck's grid dirty; flushed on the next frame.
    fn schedule_save(&mut self) {
        if let Some(deck) = self.deck.as_mut() {
            let grid = deck.edit_grid;
            deck.pending_save = Some((deck.hash.clone(), grid));
        }
    }

    /// Drains the bus under the frame budget, applying each event.
    fn drain_bus(&mut self) {
        let mut pending_enqueue: Vec<crate::playlist::queue::QueueJob> = Vec::new();
        let mut derive = |event: &Event, tracks: &mut crate::tracks::Tracks| {
            // Derive worker jobs at apply time: hashes arriving without
            // analysis knowledge get analysis jobs immediately.
            match event {
                Event::RowsLoaded {
                    hashes, records, ..
                } => {
                    for record in records {
                        tracks.upsert(record.clone());
                    }
                    for hash in hashes {
                        enqueue_if_needed(tracks, hash, &mut pending_enqueue);
                    }
                }
                Event::RowAdded { hash, .. } | Event::TagsResolved { hash, .. } => {
                    enqueue_if_needed(tracks, hash, &mut pending_enqueue);
                }
                _ => {}
            }
        };
        let mut events = Vec::new();
        self.bus.drain(|event| events.push(event));
        for event in events {
            derive(&event, &mut self.tracks);
            self.apply(event);
        }
        for job in pending_enqueue {
            self.playlist_queue.enqueue(job);
        }
    }

    /// The single frontend mutation path: applies drained bus events to
    /// runtime state. Everything async arrives here.
    fn apply(&mut self, event: Event) {
        // Playlist ordering first (its appliers own the add counter).
        self.playlist_state.apply(&event);

        match event {
            Event::LoadStage(stage) => {
                self.status = match stage {
                    crate::track::LoadStage::Hashing => "hashing…".to_owned(),
                    crate::track::LoadStage::Decoding => "decoding…".to_owned(),
                    crate::track::LoadStage::Analyzing => "analyzing…".to_owned(),
                    crate::track::LoadStage::CacheHit => "cached analysis…".to_owned(),
                };
            }
            Event::LoadDone(boxed) => {
                self.load_in_flight = false;
                match *boxed {
                    Ok(outcome) => self.apply_load_done(outcome),
                    Err(message) => self.status = format!("\u{26a0} load failed: {message}"),
                }
            }
            Event::AnalysisStarted { hash } => {
                self.tracks.set_analysis(&hash, AnalysisState::Analyzing);
            }
            Event::AnalysisDone { hash, analysis } => {
                self.tracks
                    .set_analysis(&hash, AnalysisState::Ready(analysis));
                // The armed click fires now: the worker just persisted
                // this analysis, so the load is a cache-hit (one pass).
                if self.pending_deck_load.as_ref() == Some(&hash)
                    && let Some(path) = self.tracks.path_of(&hash).cloned()
                {
                    self.pending_deck_load = None;
                    self.load_in_flight = true;
                    let tx = self.bus.sender();
                    crate::track::spawn_load(&self.services, tx, path);
                }
            }
            Event::AnalysisFailed { hash, message } => {
                self.tracks
                    .set_analysis(&hash, AnalysisState::Failed(message.clone()));
                if self.pending_deck_load.as_ref() == Some(&hash) {
                    self.pending_deck_load = None;
                    self.status = format!("\u{26a0} analysis failed: {message}");
                }
            }
            Event::TagsResolved { hash, tags } => {
                let mut record = self.tracks.get(&hash).cloned().unwrap_or(TrackRecord {
                    hash: hash.clone(),
                    tags: tags.clone(),
                    analysis: AnalysisState::Queued,
                });
                record.tags = tags;
                self.tracks.upsert(record);
            }
            Event::GridSaved { hash, grid } => {
                self.tracks.refresh_grid(&hash, &grid);
                self.status = format!("grid saved ({:.8})", hash.0);
            }
            Event::GridSaveFailed(message) => {
                self.status = format!("\u{26a0} save failed: {message}");
            }
            Event::RowsLoaded { hashes, .. } => {
                // Retry semantics: hashes whose store hydration found no
                // analysis clear a terminal Failed state.
                self.tracks.retry_failed(&hashes);
            }
            Event::AddFailed { message } => {
                self.status = format!("\u{26a0} add failed: {message}");
            }
            Event::CommandFailed(message) => {
                self.status = format!("\u{26a0} {message}");
            }
            // Ordering events were applied by the playlist state above.
            Event::PlaylistsLoaded(_)
            | Event::PlaylistCreated(_)
            | Event::PlaylistRenamed { .. }
            | Event::PlaylistDeleted(_)
            | Event::RowsLoadFailed { .. }
            | Event::RowAdded { .. }
            | Event::RowRemoved { .. }
            | Event::RowsReordered { .. }
            | Event::DuplicateSkipped { .. }
            | Event::AddStarted { .. } => {}
        }
    }

    /// Applies a terminal load outcome: the record gains the analysis
    /// package and a fresh deck is built atomically (the prior deck,
    /// with its stale PCM/grid/engine, is dropped).
    fn apply_load_done(&mut self, outcome: crate::bus::LoadOutcome) {
        self.tracks.set_analysis(
            &outcome.hash,
            AnalysisState::Ready(outcome.analysis.clone()),
        );
        let hash = outcome.hash.clone();
        let path = outcome.path.clone();
        let display = path.display();
        let duration = outcome.analysis.duration_seconds;
        let bpm = outcome.analysis.bpm;
        match Deck::new(outcome) {
            Ok(deck) => {
                self.status = format!("loaded {display} ({duration:.1}s, {bpm:.3} BPM)");
                self.deck = Some(deck);
            }
            Err(message) => {
                self.status = format!("\u{26a0} audio unavailable: {message}");
                let _ = hash;
            }
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
                    self.playlist_state.contents = Contents::Loading;
                    self.spawn_contents_load(id);
                }
                PanelAction::NewPlaylist => self.create_playlist(),
                PanelAction::RenamePlaylist { id, name } => self.rename_playlist(id, name),
                PanelAction::DeletePlaylist(id) => self.delete_playlist(id),
                PanelAction::AddTracks => self.add_tracks_dialog(),
                PanelAction::LoadRow(hash) => self.load_row(&hash),
                PanelAction::MoveRow { from, to } => self.move_row_persist(&from, &to),
                PanelAction::RemoveRow { hash } => self.remove_row_persist(&hash),
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

    /// Renames a playlist; the panel's inline editor carries the field.
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
    /// add-track task (hash → tags → duplicate check → insert → events).
    fn add_tracks_dialog(&mut self) {
        let Some(playlist_id) = self.playlist_state.selected else {
            return;
        };
        let registry = djcore::decoder::DecoderRegistry::with_symphonia();
        let extensions = registry.supported_extensions();
        let paths = rfd::FileDialog::new()
            .set_title("Add tracks to playlist")
            .add_filter("audio", &extensions)
            .pick_files();
        let Some(paths) = paths else {
            return;
        };
        self.bus.send(Event::AddStarted { count: paths.len() });
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
            match add_track_task(&services, playlist_id, &path).await {
                Ok(Some((hash, tags))) => {
                    // Tags first: the record (with its path) must exist
                    // before the row event makes the enqueue derivation
                    // run — otherwise the job has no path to decode.
                    let _ = tx.send(Event::TagsResolved {
                        hash: hash.clone(),
                        tags,
                    });
                    let _ = tx.send(Event::RowAdded { playlist_id, hash });
                }
                Ok(None) => {
                    let _ = tx.send(Event::DuplicateSkipped {
                        playlist_id,
                        path: path_display,
                    });
                }
                Err(message) => {
                    let _ = tx.send(Event::AddFailed { message });
                }
            }
        });
    }

    /// Loads a playlist row into the deck — the deck's only entry point.
    /// A ready row loads immediately; a pending row (queued/analyzing)
    /// drops the current deck and arms the hash: when its analysis
    /// lands, the load fires (the worker just persisted the grid, so
    /// the load is a cache-hit — one analysis pass total).
    fn load_row(&mut self, hash: &TrackHash) {
        self.deck = None;
        self.pending_deck_load = None;
        if self.tracks.is_ready(hash) {
            let Some(path) = self.tracks.path_of(hash).cloned() else {
                return;
            };
            self.load_in_flight = true;
            let tx = self.bus.sender();
            crate::track::spawn_load(&self.services, tx, path);
        } else {
            self.pending_deck_load = Some(hash.clone());
        }
    }

    /// Re-analyze: the deck drops, the record's analysis clears (rows
    /// everywhere derive "needs analysis"), and a fresh load deletes the
    /// stored grid before re-analyzing (ordered inside the task).
    fn reanalyze_current(&mut self) {
        let Some(deck) = self.deck.take() else {
            return;
        };
        let hash = deck.hash.clone();
        // The analysis data is GONE: every row referencing this hash
        // derives "needs analysis" on the next frame, then queued —
        // no row-addressed event exists to send.
        self.tracks.clear_analysis(&hash);
        self.load_in_flight = true;
        // Analysis concern, not a deck concern: enqueue a forced job on
        // the single-worker queue. The deck stays dropped (the waveform
        // shows the no-track placeholder); nothing loads into the deck
        // until the user clicks a playlist row.
        let path = deck.path.clone();
        self.playlist_queue
            .enqueue(crate::playlist::queue::QueueJob {
                hash,
                path,
                force: true,
            });
    }

    /// Splices rows locally (instant visual feedback) and persists the
    /// new order; the store's confirmation event re-asserts order.
    fn move_row_persist(&mut self, from: &TrackHash, to: &TrackHash) {
        let Some(playlist_id) = self.playlist_state.selected else {
            return;
        };
        let _ = crate::playlist::move_row(&mut self.playlist_state, from, to);
        let Contents::Loaded(hashes) = &self.playlist_state.contents else {
            return;
        };
        let store = self.services.playlist_store.clone();
        let tx = self.bus.sender();
        let order = hashes.clone();
        self.services.runtime.handle().spawn(async move {
            match store.reorder(playlist_id, &order).await {
                Ok(()) => {
                    let _ = tx.send(Event::RowsReordered {
                        playlist_id,
                        hashes: order,
                    });
                }
                Err(report) => {
                    let _ = tx.send(Event::CommandFailed(format!("reorder: {report:#}")));
                }
            }
        });
    }

    /// Removes a row: local splice plus a persisted removal.
    fn remove_row_persist(&mut self, hash: &TrackHash) {
        let Some(playlist_id) = self.playlist_state.selected else {
            return;
        };
        let Some(position) = crate::playlist::remove_row(&mut self.playlist_state, hash) else {
            return;
        };
        let store = self.services.playlist_store.clone();
        let tx = self.bus.sender();
        let hash = hash.clone();
        #[expect(clippy::cast_possible_wrap, reason = "usize index to i64 position")]
        let position = position as i64;
        self.services.runtime.handle().spawn(async move {
            match store.remove_track(playlist_id, position).await {
                Ok(()) => {
                    let _ = tx.send(Event::RowRemoved { playlist_id, hash });
                }
                Err(report) => {
                    let _ = tx.send(Event::CommandFailed(format!("remove track: {report:#}")));
                }
            }
        });
    }
}

/// Enqueues an analysis job for `hash` when the database holds no
/// analysis knowledge (hash dedup: `Ready`/in-flight/`Failed` all
/// suppress).
fn enqueue_if_needed(
    tracks: &mut crate::tracks::Tracks,
    hash: &TrackHash,
    pending: &mut Vec<crate::playlist::queue::QueueJob>,
) {
    if !tracks.needs_job(hash) {
        return;
    }
    tracks.mark_queued(hash);
    // The job needs a real source path; a placeholder record (no tags
    // yet) waits for the tags event to re-run this derivation.
    if let Some(path) = tracks.path_of(hash).cloned()
        && !path.as_os_str().is_empty()
    {
        pending.push(crate::playlist::queue::QueueJob {
            hash: hash.clone(),
            path,
            force: false,
        });
    }
}

/// One add-track task's body: hash → tags → duplicate check → insert.
/// Returns the new row's hash or `None` for a duplicate (skipped
/// silently — no row, no queue job).
async fn add_track_task(
    services: &Services,
    playlist_id: i64,
    path: &std::path::Path,
) -> Result<Option<(TrackHash, crate::tracks::TrackTags)>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let hash = TrackHash(crate::track::identity::hex_sha256(&bytes));
    let tags = crate::track::identity::resolve_tags(&bytes, path);
    let duration = crate::track::identity::probe_duration(&bytes, path);

    if services
        .playlist_store
        .contains_hash(playlist_id, &hash)
        .await
        .map_err(|report| format!("{report:#}"))?
    {
        return Ok(None);
    }

    services
        .playlist_store
        .ensure_track(
            playlist_id,
            &hash,
            &path.display().to_string(),
            &tags.title,
            &tags.artist,
            duration,
        )
        .await
        .map_err(|report| format!("{report:#}"))?;

    Ok(Some((hash, tags)))
}

impl eframe::App for AutomixahUiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The bus drain is the frame's first act: render with confirmed
        // state only. Leftover events (over the 10 ms budget) land next
        // frame — the drain requests a repaint in that case.
        self.drain_bus();
        // Bottom panel first: it registers before CentralPanel claims
        // the remaining space.
        let actions = crate::playlist::view::panel(ctx, &mut self.playlist_state, &self.tracks);
        self.handle_panel_actions(actions);
        if ctx.input(|i| i.key_pressed(egui::Key::Space))
            && let Some(deck) = self.deck.as_mut()
        {
            deck.scrub.toggle_play();
            deck.push_command();
        }
        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if self.load_in_flight {
                    ui.spinner();
                }
                let can_reanalyze = !self.load_in_flight && self.deck.is_some();
                let reanalyze = ui.add_enabled(can_reanalyze, egui::Button::new("re-analyze"));
                if reanalyze.clicked() {
                    self.reanalyze_current();
                }
                ui.separator();
                ui.label(&self.status);
            });
        });

        self.render_grid_controls(ctx);

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(deck) = self.deck.as_mut() else {
                ui.centered_and_justified(|ui| {
                    if let Some(hash) = &self.pending_deck_load {
                        let title = self
                            .tracks
                            .get(hash)
                            .map_or_else(String::new, |r| r.tags.title.clone());
                        ui.spinner();
                        ui.weak(format!(
                            "analyzing \u{2013} {title} \u{2013} loads when done"
                        ));
                    } else {
                        ui.weak("no track loaded \u{2014} pick one from the playlist below");
                    }
                });
                return;
            };
            crate::view::waveform::deck_panel(ui, deck, &mut self.status);
        });

        if let Some(deck) = self.deck.as_mut() {
            if deck.grid_dirty {
                deck.grid_dirty = false;
                self.schedule_save();
            }
            self.flush_save_if_due();
        }

        // Keep the UI live while a deck is loaded (playhead ticking), a
        // load is in flight, or any shown row is pending.
        if self.deck.is_some()
            || self.load_in_flight
            || self.playlist_state.any_pending(&self.tracks)
        {
            ctx.request_repaint();
        }
    }
}

impl AutomixahUiApp {
    /// The right side panel: grid controls (zoom slider + grid editor).
    fn render_grid_controls(&mut self, ctx: &egui::Context) {
        egui::SidePanel::right("grid_controls").show(ctx, |ui| {
            let Some(deck) = self.deck.as_mut() else {
                return;
            };
            let end = deck.duration_seconds();
            ui.horizontal(|ui| {
                if let Some(c) = deck.cursor_time {
                    if ui.button("snap beat @ cursor").clicked() {
                        deck.edit_grid.snap_nearest_beat(c);
                        deck.grid_dirty = true;
                    }
                    if ui.button("set downbeat @ cursor").clicked() {
                        deck.edit_grid.set_downbeat_at(c);
                        deck.grid_dirty = true;
                    }
                }
            });
            ui.add(
                egui::Slider::new(&mut deck.view.playhead_frac, 0.05..=0.95)
                    .text("playhead x")
                    .custom_formatter(|n, _| format!("{n:.0}%")),
            );
            if crate::view::grid::controls(ui, &mut deck.edit_grid, end) {
                deck.grid_dirty = true;
            }
        });
    }
}

/// Integration-test hooks: drive the save path without egui.
#[cfg(any(test, feature = "__test-hooks"))]
impl AutomixahUiApp {
    /// Simulates a loaded deck for save-path testing.
    pub fn inject_deck_for_test(&mut self, outcome: crate::bus::LoadOutcome) {
        self.deck = Some(crate::deck::Deck::new(outcome).expect("deck"));
    }

    /// Applies a grid shift and marks dirty, like the gesture path.
    pub fn test_shift_grid(&mut self, delta: f32) {
        let deck = self.deck.as_mut().expect("deck");
        deck.edit_grid.shift_by(delta);
        deck.pending_save = Some((deck.hash.clone(), deck.edit_grid));
    }

    /// Changes the downbeat phase and marks dirty.
    pub fn test_set_downbeat_phase(&mut self, phase: u8) {
        let deck = self.deck.as_mut().expect("deck");
        deck.edit_grid.downbeat_phase = phase;
        deck.pending_save = Some((deck.hash.clone(), deck.edit_grid));
    }

    /// Flushes a pending save now.
    pub fn flush_save_if_due_for_test(&mut self) {
        self.flush_save_if_due();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given a complete persisted track (grid + key + duration).
    // When hydrated.
    // Then its record carries a Ready analysis and the hash lists first.
    #[test]
    fn hydrate_records_makes_complete_tracks_ready() {
        let complete = crate::playlist::store::PersistedTrack {
            id: 1,
            position: 0,
            track_hash: TrackHash("done".to_owned()),
            title: "T".to_owned(),
            artist: "A".to_owned(),
            added_path: "/done".to_owned(),
            duration: Some(60.0),
            grid: Some(crate::store::GridOverride {
                grid_bpm: 128.0,
                anchor_seconds: 0.0,
                downbeat_phase: 0,
                updated_at: 0,
                key: Some(djcore::key::Key {
                    root: 9,
                    mode: djcore::key::KeyMode::Minor,
                }),
            }),
        };
        let incomplete = crate::playlist::store::PersistedTrack {
            id: 2,
            position: 1,
            track_hash: TrackHash("todo".to_owned()),
            title: "T2".to_owned(),
            artist: "A2".to_owned(),
            added_path: "/todo".to_owned(),
            duration: None,
            grid: None,
        };

        let (hashes, records) = hydrate_records_for_test(vec![complete, incomplete]);

        assert_eq!(hashes.len(), 2, "both hashes listed");
        assert_eq!(records.len(), 1, "only the complete track hydrated Ready");
        assert_eq!(records[0].hash.0, "done");
        assert!(records[0].analysis.is_ready());
    }

    // Given a loaded record with a ready analysis.
    // When re-analysis runs (record cleared, started/done events applied).
    // Then the record derives queued → analyzing → ready with no
    // row-addressed event anywhere in the chain.
    #[test]
    fn reanalyze_derives_rows_through_record_states() {
        let mut app = AutomixahUiApp::new(
            crate::playlist::queue::tests::fake_services(
                crate::playlist::queue::tests::output_fixture(),
            ),
            crate::bus::EventBus::without_repaint(),
        );
        let hash = TrackHash("reanalyze-me".to_owned());
        app.tracks.upsert(TrackRecord {
            hash: hash.clone(),
            tags: crate::tracks::TrackTags {
                title: "T".to_owned(),
                artist: "A".to_owned(),
                path: "/t".into(),
            },
            analysis: AnalysisState::Ready(crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 128.0,
                key: djcore::key::Key {
                    root: 9,
                    mode: djcore::key::KeyMode::Minor,
                },
                duration_seconds: 60.0,
            }),
        });

        // Step 1: the analysis data is GONE (reanalyze clears it).
        app.tracks.clear_analysis(&hash);
        assert!(
            matches!(
                app.tracks.get(&hash).map(|r| &r.analysis),
                Some(crate::tracks::AnalysisState::Queued)
            ),
            "row derives queued with no event sent"
        );

        // Steps 2–3: the analysis events land; the record follows.
        app.apply(Event::AnalysisStarted { hash: hash.clone() });
        assert!(
            matches!(
                app.tracks.get(&hash).map(|r| &r.analysis),
                Some(crate::tracks::AnalysisState::Analyzing)
            ),
            "row derives analyzing"
        );
        app.apply(Event::AnalysisDone {
            hash: hash.clone(),
            analysis: crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 140.0,
                key: djcore::key::Key {
                    root: 4,
                    mode: djcore::key::KeyMode::Major,
                },
                duration_seconds: 61.0,
            },
        });
        let Some(crate::tracks::AnalysisState::Ready(a)) =
            app.tracks.get(&hash).map(|r| &r.analysis)
        else {
            panic!("ready after done")
        };
        assert!((a.bpm - 140.0).abs() < f32::EPSILON, "fresh bpm derives");
    }

    // Given a track whose hash is already Ready in the database.
    // When the enqueue derivation runs for it.
    // Then no job is enqueued (hash dedup).
    #[test]
    fn enqueue_derivation_skips_ready_hashes() {
        let mut tracks = crate::tracks::Tracks::default();
        tracks.upsert(TrackRecord {
            hash: TrackHash("known".to_owned()),
            tags: crate::tracks::TrackTags {
                title: "T".to_owned(),
                artist: String::new(),
                path: "/known".into(),
            },
            analysis: AnalysisState::Ready(crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 128.0,
                key: djcore::key::Key {
                    root: 9,
                    mode: djcore::key::KeyMode::Minor,
                },
                duration_seconds: 60.0,
            }),
        });
        let mut pending = Vec::new();

        enqueue_if_needed(&mut tracks, &TrackHash("known".to_owned()), &mut pending);

        assert!(pending.is_empty(), "ready hash enqueues nothing");
    }

    // Given a hash with no record at all.
    // When the enqueue derivation runs for it.
    // Then the record is created queued and the job is withheld
    // until tags (with the path) land.
    #[test]
    fn enqueue_derivation_marks_unknown_hash_queued() {
        let mut tracks = crate::tracks::Tracks::default();
        let mut pending = Vec::new();

        enqueue_if_needed(&mut tracks, &TrackHash("fresh".to_owned()), &mut pending);

        assert!(pending.is_empty(), "no path known — job withheld");
        assert!(
            matches!(
                tracks
                    .get(&TrackHash("fresh".to_owned()))
                    .map(|r| &r.analysis),
                Some(crate::tracks::AnalysisState::Queued)
            ),
            "record created queued, awaiting tags"
        );
    }

    // Given a deck loaded for one track.
    // When another load outcome applies.
    // Then the prior deck (PCM, grid, engine) is replaced whole —
    // nothing of it survives the swap.
    #[test]
    fn loading_replaces_the_prior_deck_entirely() {
        let mut app = AutomixahUiApp::new(
            crate::playlist::queue::tests::fake_services(
                crate::playlist::queue::tests::output_fixture(),
            ),
            crate::bus::EventBus::without_repaint(),
        );
        let first = TrackHash("first".to_owned());
        let second = TrackHash("second".to_owned());

        let outcome = |hash: &TrackHash| crate::bus::LoadOutcome {
            hash: hash.clone(),
            path: format!("/{}.wav", hash.0).into(),
            analysis: crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 128.0,
                key: djcore::key::Key {
                    root: 9,
                    mode: djcore::key::KeyMode::Minor,
                },
                duration_seconds: 0.0,
            },
            audio: djcore::decoder::DecodeAudio {
                samples: Vec::new(),
                sample_rate: 44_100,
                channels: 2,
            },
            peaks: crate::audio::peaks::Peaks::build(&[], 44_100),
        };
        app.apply_load_done(outcome(&first));
        assert_eq!(app.deck.as_ref().expect("deck").hash.0, "first");

        app.apply_load_done(outcome(&second));

        let deck = app.deck.as_ref().expect("deck");
        assert_eq!(deck.hash.0, "second", "new deck");
    }

    fn app_with_pending_record(hash: &str) -> (AutomixahUiApp, TrackHash) {
        let mut app = AutomixahUiApp::new(
            crate::playlist::queue::tests::fake_services(
                crate::playlist::queue::tests::output_fixture(),
            ),
            crate::bus::EventBus::without_repaint(),
        );
        let hash = TrackHash(hash.to_owned());
        app.tracks.upsert(TrackRecord {
            hash: hash.clone(),
            tags: crate::tracks::TrackTags {
                title: "T".to_owned(),
                artist: "A".to_owned(),
                path: "/t".into(),
            },
            analysis: AnalysisState::Queued,
        });
        (app, hash)
    }

    // Given a deck loaded and another row still analyzing.
    // When the analyzing row is clicked.
    // Then the current deck drops and the hash arms (no load fires yet).
    #[test]
    fn clicking_a_pending_row_arms_the_deck_without_loading() {
        let (mut app, hash) = app_with_pending_record("pending");
        app.inject_deck_for_test(crate::bus::LoadOutcome {
            hash: TrackHash("loaded".to_owned()),
            path: "/loaded".into(),
            analysis: crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 128.0,
                key: djcore::key::Key {
                    root: 9,
                    mode: djcore::key::KeyMode::Minor,
                },
                duration_seconds: 0.0,
            },
            audio: djcore::decoder::DecodeAudio {
                samples: Vec::new(),
                sample_rate: 44_100,
                channels: 2,
            },
            peaks: crate::audio::peaks::Peaks::build(&[], 44_100),
        });

        app.load_row(&hash);

        assert!(app.deck.is_none(), "deck dropped");
        assert_eq!(app.pending_deck_load, Some(hash), "hash armed");
        assert!(!app.load_in_flight, "no load fired yet");
    }

    // Given an armed pending click whose analysis completes.
    // When the done event applies.
    // Then the load pipeline fires for the armed hash.
    #[test]
    fn analysis_done_for_armed_hash_fires_the_load() {
        let (mut app, hash) = app_with_pending_record("armed");
        app.load_row(&hash);
        assert_eq!(app.pending_deck_load, Some(hash.clone()));

        app.apply(Event::AnalysisDone {
            hash: hash.clone(),
            analysis: crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 140.0,
                key: djcore::key::Key {
                    root: 4,
                    mode: djcore::key::KeyMode::Major,
                },
                duration_seconds: 61.0,
            },
        });

        assert!(app.load_in_flight, "load fired for the armed hash");
        assert_eq!(app.pending_deck_load, None, "arming consumed");
    }

    // Given an armed click whose analysis fails.
    // When the failed event applies.
    // Then the arming disarms and nothing loads.
    #[test]
    fn analysis_failure_disarms_the_pending_load() {
        let (mut app, hash) = app_with_pending_record("doomed");
        app.load_row(&hash);

        app.apply(Event::AnalysisFailed {
            hash,
            message: "decode failed".to_owned(),
        });

        assert_eq!(app.pending_deck_load, None, "disarmed");
        assert!(!app.load_in_flight, "no load fired");
    }

    // Given a re-analyzed track completing while another is loaded.
    // When the bare done event applies with no arming.
    // Then nothing loads (a done event alone never starts a load).
    #[test]
    fn reanalyze_completion_never_loads_the_deck() {
        let (mut app, hash) = app_with_pending_record("reanalyze");
        app.tracks.set_analysis(
            &hash,
            AnalysisState::Ready(crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 128.0,
                key: djcore::key::Key {
                    root: 9,
                    mode: djcore::key::KeyMode::Minor,
                },
                duration_seconds: 60.0,
            }),
        );
        app.load_row(&hash);
        assert!(app.load_in_flight, "a track is loading");

        // A done event for some other re-analyzed hash arrives with no
        // arming in place: no load may fire from it.
        let other = TrackHash("other".to_owned());
        app.pending_deck_load = None;
        app.apply(Event::AnalysisDone {
            hash: other,
            analysis: crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 101.0,
                key: djcore::key::Key {
                    root: 3,
                    mode: djcore::key::KeyMode::Major,
                },
                duration_seconds: 50.0,
            },
        });

        assert!(
            app.pending_deck_load.is_none(),
            "a bare done event never arms or loads"
        );
    }
}
