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
use automixah_engine::mixdown::MixdownOutcome;
use automixah_engine::timeline::types::TrackHash;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// The automixah-ui application.
pub struct AutomixahUiApp {
    /// DI container (paths, grid store, async handle). Clone-cheap.
    services: Services,
    /// The frontend track database (hash → tags + analysis).
    tracks: crate::tracks::Tracks,
    /// Playlist section state (playlists + selected ordering).
    pub(crate) playlist_state: crate::playlist::PlaylistState,
    /// Library section state (roots, entries, scan lifecycle).
    pub(crate) library_state: crate::library::LibraryState,
    /// Library search box buffer (view-owned, like the render path).
    pub(crate) library_filter: String,
    /// Single-worker analysis queue (jobs in; bus events out).
    pub(crate) playlist_queue: crate::playlist::queue::AnalysisQueue,
    /// FIFO playlist reorder persistence queue.
    pub(crate) reorder_queue: crate::playlist::reorder::ReorderQueue,
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
    /// Absolute output path for the next mixdown (free-text + browse).
    pub(crate) render_out: String,
    /// Cancel flag for the in-flight mixdown; `None` when idle. Only
    /// a terminal render event may clear it — CancelRender only sets.
    pub(crate) render_cancel: Option<Arc<AtomicBool>>,
    /// Latest staged progress of the in-flight mixdown; `None` idle.
    pub(crate) render_stage: Option<crate::bus::RenderStage>,
    /// The UI event bus: every async outcome lands here; `update`
    /// drains it and applies events (the sole state mutation path).
    pub(crate) bus: crate::bus::EventBus,

    /// Latest optimistic reorder sequence per playlist.
    latest_reorder: HashMap<i64, u64>,
    /// Target BPM for rendering the mix.
    pub(crate) render_bpm: f32,
}

impl AutomixahUiApp {
    /// Builds the app around the assembled services.
    #[must_use]
    pub fn new(services: Services, bus: crate::bus::EventBus) -> Self {
        let playlist_queue =
            crate::playlist::queue::AnalysisQueue::spawn(services.clone(), bus.sender());
        let reorder_queue =
            crate::playlist::reorder::ReorderQueue::spawn(services.clone(), bus.sender());
        Self {
            services,
            tracks: crate::tracks::Tracks::default(),
            playlist_state: crate::playlist::PlaylistState::default(),
            library_state: crate::library::LibraryState::default(),
            library_filter: String::new(),
            playlist_queue,
            reorder_queue,
            deck: None,
            load_in_flight: false,
            pending_deck_load: None,
            status: "pick a track from the playlist to begin".to_owned(),
            render_out: String::new(),
            render_cancel: None,
            render_stage: None,
            bus,
            latest_reorder: HashMap::new(),
            render_bpm: 138.0,
        }
    }

    /// Spawns the startup loads: playlist list + library index (no scan).
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
        crate::library::scan::spawn_load(&self.services, self.bus.sender());
    }

    /// Spawns a contents fetch for the selected playlist: the event
    /// applier replaces the contents when it lands.
    fn spawn_contents_load(&self, playlist_id: i64) {
        let store = self.services.playlist_store.clone();
        let grid_store = self.services.grid_store.clone();
        let cue_store = self.services.cue_store.clone();
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
            let cues = load_cues(cue_store, &joined).await;
            let (hashes, records) = hydrate_records(joined, &cues);
            let _ = tx.send(Event::RowsLoaded {
                playlist_id,
                hashes,
                records,
            });
        });
    }
}

/// Loads each persisted track's cue points from the cue store.
async fn load_cues(
    cue_store: crate::store::CueStoreService,
    persisted: &[crate::playlist::store::PersistedTrack],
) -> std::collections::HashMap<TrackHash, automixah_engine::timeline::types::CuePoints> {
    let mut cues = std::collections::HashMap::with_capacity(persisted.len());
    for track in persisted {
        let cue_points = cue_store
            .get(&track.track_hash)
            .await
            .ok()
            .unwrap_or_default();
        cues.insert(track.track_hash.clone(), cue_points);
    }
    cues
}

/// Builds contents hashes + hydrated records from persisted tracks:
/// complete entries (grid + key + duration) carry a `Ready` analysis;
/// incomplete ones stay `Queued` (the enqueue derivation re-enqueues).
#[must_use]
fn hydrate_records(
    persisted: Vec<crate::playlist::store::PersistedTrack>,
    cues: &std::collections::HashMap<TrackHash, automixah_engine::timeline::types::CuePoints>,
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
                cues: cues.get(&track.track_hash).copied().unwrap_or_default(),
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
    let cues = std::collections::HashMap::new();
    hydrate_records(persisted, &cues)
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
    /// Flushes pending grid and cue saves from the deck; spawned tasks
    /// report back through the bus.
    fn flush_save_if_due(&mut self) {
        let Some(deck) = self.deck.as_mut() else {
            return;
        };
        if let Some((hash, grid)) = deck.pending_save.take() {
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
        if deck.cue_save_in_flight.is_none()
            && let Some((hash, cues)) = deck.pending_cue_save.take()
        {
            deck.cue_save_in_flight = Some((hash.clone(), cues));
            let store = self.services.cue_store.clone();
            let tx = self.bus.sender();
            self.services.runtime.handle().spawn(async move {
                let event = match store.put(&hash, &cues).await {
                    Ok(()) => Event::CuesSaved { hash, cues },
                    Err(report) => Event::CuesSaveFailed {
                        hash,
                        cues,
                        message: format!("{report:#}"),
                    },
                };
                let _ = tx.send(event);
            });
        }
    }

    /// Marks the deck's grid dirty; flushed on the next frame.
    fn schedule_save(&mut self) {
        if let Some(deck) = self.deck.as_mut() {
            deck.pending_save = Some((deck.hash.clone(), deck.edit_grid));
        }
    }

    /// Marks the current cue set for an off-thread complete replacement save.
    fn schedule_cue_save(&mut self) {
        if let Some(deck) = self.deck.as_mut() {
            deck.pending_cue_save = Some((deck.hash.clone(), deck.cues));
        }
    }

    /// Re-snaps every occupied cue after a grid edit and marks both data sets
    /// for persistence when positions changed.
    fn resnap_deck_cues_after_grid_change(&mut self) {
        let Some(deck) = self.deck.as_mut() else {
            return;
        };
        if crate::grid::resnap_cues(
            &mut deck.cues,
            deck.sample_rate,
            deck.source_frames,
            &deck.edit_grid,
        ) {
            deck.cues_dirty = true;
            self.schedule_cue_save();
        }
    }

    /// Drains the bus under the frame budget, applying each event.
    fn drain_bus(&mut self) {
        let mut pending_enqueue: Vec<(crate::playlist::queue::QueueJob, EnqueueTier)> = Vec::new();
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
                        enqueue_if_needed(
                            tracks,
                            hash,
                            EnqueueTier::Playlist,
                            &mut pending_enqueue,
                        );
                    }
                }
                Event::RowAdded { hash, .. } | Event::TagsResolved { hash, .. } => {
                    enqueue_if_needed(tracks, hash, EnqueueTier::Playlist, &mut pending_enqueue);
                }
                Event::LibraryLoaded {
                    roots,
                    entries,
                    analyzed,
                } => {
                    enqueue_library_backfill(
                        tracks,
                        roots,
                        entries,
                        analyzed,
                        &mut pending_enqueue,
                    );
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
        for (job, tier) in pending_enqueue {
            self.playlist_queue.enqueue(job, tier.priority());
        }
    }

    /// The single frontend mutation path: applies drained bus events to
    /// runtime state. Everything async arrives here.
    fn apply(&mut self, event: Event) {
        let apply_reorder = match &event {
            Event::RowsReordered {
                playlist_id,
                sequence,
                ..
            }
            | Event::ReorderFailed {
                playlist_id,
                sequence,
                ..
            }
            | Event::ReorderCommandFailed {
                playlist_id,
                sequence,
                ..
            } => {
                self.playlist_state.selected == Some(*playlist_id)
                    && self.latest_reorder.get(playlist_id) == Some(sequence)
            }
            _ => true,
        };
        // Playlist ordering is mutated only here; stale reorder completions
        // are deliberately ignored after a newer optimistic drop exists.
        if apply_reorder {
            self.playlist_state.apply(&event);
        }

        match event {
            Event::LoadStage(stage) => {
                self.status = match stage {
                    crate::track::LoadStage::Hashing => "hashing…".to_owned(),
                    crate::track::LoadStage::Decoding => "decoding…".to_owned(),
                    crate::track::LoadStage::Analyzing => "analyzing…".to_owned(),
                    crate::track::LoadStage::CacheHit => "cached analysis…".to_owned(),
                };
            }
            Event::LoadDone(ref boxed) => {
                self.load_in_flight = false;
                match boxed.as_ref() {
                    Ok(outcome) => self.apply_load_done(outcome.clone()),
                    Err(message) => self.status = format!("\u{26a0} load failed: {message}"),
                }
            }
            Event::AnalysisStarted { ref hash } => {
                self.tracks.set_analysis(hash, AnalysisState::Analyzing);
            }
            Event::AnalysisDone { ref hash, ref analysis } => {
                self.tracks
                    .set_analysis(hash, AnalysisState::Ready(analysis.clone()));
                // The armed click fires now: the worker just persisted
                // this analysis, so the load is a cache-hit (one pass).
                if self.pending_deck_load.as_ref() == Some(hash)
                    && let Some(path) = self.tracks.path_of(hash).cloned()
                {
                    self.pending_deck_load = None;
                    self.load_in_flight = true;
                    let tx = self.bus.sender();
                    crate::track::spawn_load(&self.services, tx, path);
                }
            }
            Event::AnalysisFailed { ref hash, ref message } => {
                self.tracks
                    .set_analysis(hash, AnalysisState::Failed(message.clone()));
                if self.pending_deck_load.as_ref() == Some(hash) {
                    self.pending_deck_load = None;
                    self.status = format!("\u{26a0} analysis failed: {message}");
                }
            }
            Event::TagsResolved { ref hash, ref tags } => {
                let mut record = self.tracks.get(hash).cloned().unwrap_or(TrackRecord {
                    hash: hash.clone(),
                    tags: tags.clone(),
                    analysis: AnalysisState::Queued,
                });
                record.tags = tags.clone();
                self.tracks.upsert(record);
            }
            Event::GridSaved { ref hash, ref grid } => {
                self.tracks.refresh_grid(hash, grid);
                self.status = format!("grid saved ({:.8})", hash.0);
            }
            Event::GridSaveFailed(ref message) => {
                self.status = format!("\u{26a0} save failed: {message}");
            }
            Event::CuesSaved { ref hash, ref cues } => {
                let is_current_snapshot = self
                    .deck
                    .as_ref()
                    .is_none_or(|deck| deck.hash != *hash || deck.cues == *cues);
                if let Some(deck) = self.deck.as_mut()
                    && deck.hash == *hash
                    && deck
                        .cue_save_in_flight
                        .as_ref()
                        .is_some_and(|(_, saved)| *saved == *cues)
                {
                    deck.cue_save_in_flight = None;
                }
                if is_current_snapshot {
                    if let Some(deck) = self.deck.as_mut()
                        && deck.hash == *hash
                        && deck.cues == *cues
                    {
                        deck.cues_dirty = false;
                    }
                    self.tracks.refresh_cues(hash, cues);
                    self.status = format!("cues saved ({:.8})", hash.0);
                }
                // A different cue snapshot is an older save completion. The
                // working deck remains authoritative until its newer save
                // reports success.
            }
            Event::CuesSaveFailed {
                ref hash,
                ref cues,
                ref message,
            } => {
                if let Some(deck) = self.deck.as_mut()
                    && deck.hash == *hash
                    && deck
                        .cue_save_in_flight
                        .as_ref()
                        .is_some_and(|(_, saved)| *saved == *cues)
                {
                    deck.cue_save_in_flight = None;
                }
                self.status = format!("\u{26a0} cue save failed: {message}");
            }
            Event::RowsLoaded { ref hashes, .. } => {
                // Retry semantics: hashes whose store hydration found no
                // analysis clear a terminal Failed state.
                self.tracks.retry_failed(hashes);
            }
            Event::ReorderFailed { ref message, .. } if apply_reorder => {
                self.status = format!("⚠ {message}");
            }
            Event::ReorderCommandFailed { ref message, .. } if apply_reorder => {
                self.status = format!("⚠ {message}");
            }
            Event::AddFailed { ref message } => {
                self.status = format!("\u{26a0} add failed: {message}");
            }
            Event::ImportCompleted {
                ref summary,
                imported,
                skipped,
                ref warning,
            } => {
                self.playlist_state.selected = Some(summary.id);
                self.playlist_state.contents = Contents::Loading;
                self.spawn_contents_load(summary.id);
                self.status = match warning {
                    Some(warning) => format!("imported {imported}, skipped {skipped} ({warning})"),
                    None => format!("imported {imported}, skipped {skipped}"),
                };
            }
            Event::ImportFailed { ref message } => {
                self.status = format!("\u{26a0} import failed: {message}");
            }
            Event::CommandFailed(ref message) => {
                self.status = format!("\u{26a0} {message}");
            }
            Event::RenderProgress { stage } => {
                self.render_stage = Some(stage);
            }
            Event::RenderDone { ref out } => {
                self.render_cancel = None;
                self.render_stage = None;
                self.status = format!("wrote {}", out.display());
            }
            Event::RenderCancelled => {
                self.render_cancel = None;
                self.render_stage = None;
                self.status = "render cancelled".to_owned();
            }
            Event::RenderFailed { ref message } => {
                self.render_cancel = None;
                self.render_stage = None;
                self.status = format!("\u{26a0} render failed: {message}");
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
            | Event::ReorderFailed { .. }
            | Event::ReorderCommandFailed { .. }
            | Event::DuplicateSkipped { .. }
            | Event::AddStarted { .. }
            | Event::ImportStarted
            | Event::ImportProgress { .. }
            // Library events are consumed entirely by the library
            // state's applier below.
            | Event::LibraryLoaded { .. }
            | Event::LibraryRootAdded(_)
            | Event::LibraryRootRemoved(_)
            | Event::LibraryScanStarted
            | Event::LibraryScanProgress { .. }
            | Event::LibraryScanDone { .. }
            | Event::LibraryScanFailed { .. } => {}
        }
        // Library state mutates only here, after the playlist applier.
        self.library_state.apply(&event);
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
        self.handle_library_actions(actions.library.clone());
        use crate::playlist::view::PanelAction;
        for action in actions.actions {
            match action {
                PanelAction::SelectPlaylist(id) => {
                    self.latest_reorder.remove(&id);
                    self.playlist_state.selected = Some(id);
                    self.playlist_state.contents = Contents::Loading;
                    self.spawn_contents_load(id);
                }
                PanelAction::NewPlaylist => self.create_playlist(),
                PanelAction::RenamePlaylist { id, name } => self.rename_playlist(id, name),
                PanelAction::DeletePlaylist(id) => self.delete_playlist(id),
                PanelAction::ImportPlaylist => self.import_playlist_dialog(),
                PanelAction::LoadRow(hash) => self.load_row(&hash),
                PanelAction::MoveRow {
                    from,
                    to,
                    insert_after,
                } => self.move_row_persist(&from, &to, insert_after),
                PanelAction::RemoveRow { hash } => self.remove_row_persist(&hash),
                PanelAction::BrowseRenderOut => self.browse_render_out(),
                PanelAction::Render => {
                    if self.can_render() {
                        self.spawn_render();
                    }
                }
                PanelAction::CancelRender => self.cancel_render(),
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

    /// Opens a single-file M3U picker and starts the sequential importer.
    fn import_playlist_dialog(&mut self) {
        if self.playlist_state.import.busy {
            return;
        }
        let Some(path) = rfd::FileDialog::new()
            .set_title("Import M3U playlist")
            .add_filter("M3U playlist", &["m3u"])
            .pick_file()
        else {
            return;
        };
        if !crate::playlist::m3u::is_m3u_path(&path) {
            self.bus.send(Event::ImportFailed {
                message: "selected file is not an .m3u playlist".to_owned(),
            });
            return;
        }
        self.bus.send(Event::ImportStarted);
        let services = self.services.clone();
        let tx = self.bus.sender();
        let handle = services.runtime.handle().clone();
        handle.spawn(async move {
            import_playlist_task(services, tx, path).await;
        });
    }

    /// Library-column intents (add root, rescan, remove root, add
    /// track from the index).
    fn handle_library_actions(&mut self, actions: crate::library::view::LibraryActions) {
        use crate::library::view::LibraryAction;
        for action in actions.actions {
            match action {
                LibraryAction::AddRoot => self.add_library_root_dialog(),
                LibraryAction::Rescan => {
                    if !self.library_state.scanning {
                        crate::library::scan::spawn_scan(&self.services, self.bus.sender());
                    }
                }
                LibraryAction::RemoveRoot { id } => self.remove_library_root(id),
                LibraryAction::AddTrack { hash } => self.spawn_add_library_track(hash),
            }
        }
    }

    /// Directory picker → store root → scan (which indexes it).
    fn add_library_root_dialog(&mut self) {
        let Some(path) = rfd::FileDialog::new()
            .set_title("Add library folder")
            .pick_folder()
        else {
            return;
        };
        let canonical = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let store = self.services.library_store.clone();
        let tx = self.bus.sender();
        let services = self.services.clone();
        let handle = services.runtime.handle().clone();
        handle.spawn(async move {
            match store.add_root(&canonical.display().to_string()).await {
                Ok(root) => {
                    let _ = tx.send(Event::LibraryRootAdded(root));
                    // Scan immediately: the new root indexes on arrival.
                    crate::library::scan::spawn_scan(&services, tx.clone());
                }
                Err(report) => {
                    let _ = tx.send(Event::CommandFailed(format!(
                        "add library root: {report:#}"
                    )));
                }
            }
        });
    }

    /// Root removal: prune the root's files, drop the root, report.
    fn remove_library_root(&mut self, root_id: i64) {
        let store = self.services.library_store.clone();
        let tx = self.bus.sender();
        let handle = self.services.runtime.handle().clone();
        handle.spawn(async move {
            let outcome = async {
                store.delete_files_for_root(root_id).await?;
                store.remove_root(root_id).await?;
                Ok::<(), error_stack::Report<crate::library::store::LibraryStoreError>>(())
            }
            .await;
            match outcome {
                Ok(()) => {
                    let _ = tx.send(Event::LibraryRootRemoved(root_id));
                }
                Err(report) => {
                    let _ = tx.send(Event::CommandFailed(format!(
                        "remove library root: {report:#}"
                    )));
                }
            }
        });
    }

    /// Adds a library entry to the selected playlist: every fact comes
    /// from the index (hash, tags, duration, path) — zero file reads.
    fn spawn_add_library_track(&mut self, hash: TrackHash) {
        let Some(playlist_id) = self.playlist_state.selected else {
            return;
        };
        let Some(entry) = self
            .library_state
            .entries
            .iter()
            .find(|entry| entry.hash == hash)
            .cloned()
        else {
            return;
        };
        // Derive the absolute path from the owning root.
        let Some(root) = self
            .library_state
            .roots
            .iter()
            .find(|root| root.id == entry.root_id)
        else {
            return;
        };
        let abs_path = root.path.join(&entry.rel_path);
        let services = self.services.clone();
        let tx = self.bus.sender();
        self.bus.send(Event::AddStarted { count: 1 });
        let handle = services.runtime.handle().clone();
        handle.spawn(async move {
            let duplicate = services
                .playlist_store
                .contains_hash(playlist_id, &hash)
                .await;
            match duplicate {
                Ok(false) => {
                    if let Err(report) = services
                        .playlist_store
                        .ensure_track(
                            playlist_id,
                            &hash,
                            &abs_path.display().to_string(),
                            &entry.title,
                            &entry.artist,
                            entry.duration,
                        )
                        .await
                    {
                        let _ = tx.send(Event::AddFailed {
                            message: format!("{report:#}"),
                        });
                        return;
                    }
                    // Tags first: the record (with its path) must exist
                    // before the row event makes the enqueue derivation
                    // run — otherwise the job has no path to decode.
                    let _ = tx.send(Event::TagsResolved {
                        hash: hash.clone(),
                        tags: crate::tracks::TrackTags {
                            title: entry.title.clone(),
                            artist: entry.artist.clone(),
                            path: abs_path.clone(),
                        },
                    });
                    let _ = tx.send(Event::RowAdded { playlist_id, hash });
                }
                Ok(true) => {
                    let _ = tx.send(Event::DuplicateSkipped {
                        playlist_id,
                        path: abs_path.display().to_string(),
                    });
                }
                Err(report) => {
                    let _ = tx.send(Event::AddFailed {
                        message: format!("{report:#}"),
                    });
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
        self.playlist_queue.enqueue(
            crate::playlist::queue::QueueJob {
                hash,
                path,
                force: true,
            },
            crate::playlist::queue::Priority::Force,
        );
    }

    /// Splices rows locally and queues exactly one serialized persistence
    /// request for the completed drop.
    fn move_row_persist(&mut self, from: &TrackHash, to: &TrackHash, insert_after: bool) {
        let Some(playlist_id) = self.playlist_state.selected else {
            return;
        };
        let Some(current) = self.playlist_state.selected_rows() else {
            return;
        };
        let Some(order) = crate::playlist::moved_order(current, from, to, insert_after) else {
            return;
        };
        let Ok(sequence) = self.reorder_queue.enqueue(playlist_id, order.clone()) else {
            self.status = "⚠ reorder worker unavailable".to_owned();
            return;
        };
        let _ = crate::playlist::set_order(&mut self.playlist_state, order);
        self.latest_reorder.insert(playlist_id, sequence);
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

/// How urgently a derived job wants the worker. Maps 1:1 onto the
/// queue's [`crate::playlist::queue::Priority`]; the distinct type keeps
/// derivation code from naming queue internals.
enum EnqueueTier {
    /// User-visible playlist flow (adds, tag hydration, loads).
    Playlist,
    /// Library-refresh backfill: pure prefetch behind all user work.
    Library,
}

impl EnqueueTier {
    fn priority(self) -> crate::playlist::queue::Priority {
        match self {
            Self::Playlist => crate::playlist::queue::Priority::Playlist,
            Self::Library => crate::playlist::queue::Priority::Library,
        }
    }
}

/// Enqueues an analysis job for `hash` when the database holds no
/// analysis knowledge (hash dedup: `Ready`/in-flight/`Failed` all
/// suppress).
fn enqueue_if_needed(
    tracks: &mut crate::tracks::Tracks,
    hash: &TrackHash,
    tier: EnqueueTier,
    pending: &mut Vec<(crate::playlist::queue::QueueJob, EnqueueTier)>,
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
        pending.push((
            crate::playlist::queue::QueueJob {
                hash: hash.clone(),
                path,
                force: false,
            },
            tier,
        ));
    }
}

/// Derives background analysis jobs for one library refresh: every
/// indexed hash without a stored analysis is enrolled once, at
/// [`EnqueueTier::Library`] priority behind all user work.
///
/// The `analyzed` set (grid + key stored — the fast-path population)
/// suppresses enrollment up front, so hashes with a persisted analysis
/// never even stage a no-op job. The library entry carries real display
/// facts, so a placeholder record seeded before enrollment renders
/// artist/title immediately instead of waiting for tags via some playlist.
/// Duplicate content across roots collapses to one job; first root wins.
fn enqueue_library_backfill(
    tracks: &mut crate::tracks::Tracks,
    roots: &[crate::library::store::LibraryRoot],
    entries: &[crate::library::store::LibraryEntry],
    analyzed: &std::collections::HashSet<TrackHash>,
    pending: &mut Vec<(crate::playlist::queue::QueueJob, EnqueueTier)>,
) {
    let paths = root_paths(roots);
    let mut enrolled = std::collections::HashSet::new();
    for entry in entries {
        // Already analyzed anywhere: no job exists at all.
        if analyzed.contains(&entry.hash) || !enrolled.insert(entry.hash.clone()) {
            continue;
        }
        seed_placeholder(tracks, entry, &paths);
        enqueue_if_needed(tracks, &entry.hash, EnqueueTier::Library, pending);
    }
}

/// Root id → canonical absolute path, for joining entry locations.
fn root_paths(roots: &[crate::library::store::LibraryRoot]) -> HashMap<i64, std::path::PathBuf> {
    roots
        .iter()
        .map(|root| (root.id, root.path.clone()))
        .collect()
}

/// Seeds the track database with what the index knows about an entry so
/// rows show artist/title/path before any analysis lands.
fn seed_placeholder(
    tracks: &mut crate::tracks::Tracks,
    entry: &crate::library::store::LibraryEntry,
    paths: &HashMap<i64, std::path::PathBuf>,
) {
    let Some(root_path) = paths.get(&entry.root_id) else {
        return;
    };
    let record = TrackRecord {
        hash: entry.hash.clone(),
        tags: crate::tracks::TrackTags {
            title: entry.title.clone(),
            artist: entry.artist.clone(),
            path: root_path.join(&entry.rel_path),
        },
        analysis: AnalysisState::Queued,
    };
    tracks.upsert(record);
}

/// Imports one M3U file after creating its uniquely named playlist.
///
/// File entries are processed sequentially because `ensure_track` appends at
/// the next position. Per-entry failures are counted and do not stop later
/// valid entries.
async fn import_playlist_task(
    services: Services,
    tx: std::sync::mpsc::Sender<Event>,
    m3u_path: std::path::PathBuf,
) {
    let base = match crate::playlist::m3u::filename_stem(&m3u_path) {
        Some(base) => base,
        None => {
            let _ = tx.send(Event::ImportFailed {
                message: "playlist filename must have a non-empty .m3u stem".to_owned(),
            });
            return;
        }
    };
    let playlists = match services.playlist_store.list_playlists().await {
        Ok(playlists) => playlists,
        Err(report) => {
            let _ = tx.send(Event::ImportFailed {
                message: format!("list playlists: {report:#}"),
            });
            return;
        }
    };
    let name = crate::playlist::m3u::lowest_unused_name(
        &base,
        playlists.iter().map(|playlist| playlist.name.as_str()),
    );
    let summary = match services.playlist_store.create_playlist(&name).await {
        Ok(summary) => summary,
        Err(report) => {
            let _ = tx.send(Event::ImportFailed {
                message: format!("create playlist {name}: {report:#}"),
            });
            return;
        }
    };
    let playlist_id = summary.id;
    let _ = tx.send(Event::PlaylistCreated(summary.clone()));

    let document = match std::fs::read_to_string(&m3u_path) {
        Ok(document) => document,
        Err(error) => {
            let _ = tx.send(Event::ImportCompleted {
                summary,
                imported: 0,
                skipped: 0,
                warning: Some(format!("read {}: {error}", m3u_path.display())),
            });
            return;
        }
    };
    let paths = crate::playlist::m3u::parse_entries(&document);
    let total = paths.len();
    let _ = tx.send(Event::ImportProgress {
        processed: 0,
        total,
        imported: 0,
        skipped: 0,
    });
    let registry = djcore::decoder::DecoderRegistry::with_symphonia();
    let supported = registry.supported_extensions();
    let mut hashes = std::collections::HashSet::new();
    let mut imported = 0;
    let mut skipped = 0;

    for path in paths {
        let result = import_entry(&services, playlist_id, &path, &supported, &mut hashes).await;
        match result {
            Ok(Some((hash, tags))) => {
                imported += 1;
                let _ = tx.send(Event::TagsResolved {
                    hash: hash.clone(),
                    tags,
                });
                let _ = tx.send(Event::RowAdded { playlist_id, hash });
            }
            Ok(None) | Err(_) => skipped += 1,
        }
        let processed = imported + skipped;
        let _ = tx.send(Event::ImportProgress {
            processed,
            total,
            imported,
            skipped,
        });
    }
    let _ = tx.send(Event::ImportCompleted {
        summary,
        imported,
        skipped,
        warning: None,
    });
}

/// Reads and persists one supported M3U entry.
async fn import_entry(
    services: &Services,
    playlist_id: i64,
    path: &std::path::Path,
    supported: &[String],
    hashes: &mut std::collections::HashSet<TrackHash>,
) -> Result<Option<(TrackHash, crate::tracks::TrackTags)>, String> {
    let extension = crate::track::identity::extension_of(path);
    if !supported.iter().any(|candidate| candidate == &extension) {
        return Ok(None);
    }
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    let hash = TrackHash(crate::track::identity::hex_sha256(&bytes));
    if hashes.contains(&hash)
        || services
            .playlist_store
            .contains_hash(playlist_id, &hash)
            .await
            .map_err(|report| format!("check {}: {report:#}", path.display()))?
    {
        return Ok(None);
    }
    let tags = crate::track::identity::resolve_tags(&bytes, path);
    let duration = crate::track::identity::probe_duration(&bytes, path);
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
        .map_err(|report| format!("insert {}: {report:#}", path.display()))?;
    hashes.insert(hash.clone());
    Ok(Some((hash, tags)))
}

/// Returns the new row's hash or `None` for a duplicate (skipped
/// silently — no row, no queue job).
impl eframe::App for AutomixahUiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // The bus drain is the frame's first act: render with confirmed
        // state only. Leftover events (over the 10 ms budget) land next
        // frame — the drain requests a repaint in that case.
        self.drain_bus();
        // Bottom panel first: it registers before CentralPanel claims
        // the remaining space.
        let actions = {
            // Snapshot derivations before the mutable borrow of the
            // path buffer.
            let running = self.render_cancel.is_some();
            let can_render = self.can_render();
            let stage = self.render_stage;
            let render = crate::playlist::view::RenderUiState {
                mix_path: &mut self.render_out,
                running,
                can_render,
                stage,
                bpm: &mut self.render_bpm,
            };
            crate::playlist::view::panel(
                ctx,
                &mut self.playlist_state,
                &self.tracks,
                &mut self.library_state,
                &mut self.library_filter,
                render,
            )
        };
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
                self.resnap_deck_cues_after_grid_change();
                self.schedule_save();
            }
            self.flush_save_if_due();
        }
        // Every frame: gestures mutate the scrub machine anywhere
        // (waveform drag, spacebar); the audio thread only sees what is
        // pushed here, so this must not be call-site dependent.
        if let Some(deck) = self.deck.as_mut() {
            deck.push_command();
        }

        // Keep the UI live while a deck is loaded (playhead ticking), a
        // load is in flight, any shown row is pending, a mixdown is
        // rendering (throttled progress), or a library scan is running
        // (its progress events arrive on the raw bus channel, which
        // schedules no repaints of its own).
        if self.deck.is_some()
            || self.load_in_flight
            || self.playlist_state.any_pending(&self.tracks)
            || self.render_cancel.is_some()
            || self.library_state.is_scanning()
        {
            ctx.request_repaint();
        }
    }
}

impl AutomixahUiApp {
    /// `true` when the render button may start a mixdown: idle, a
    /// playlist selected with at least two ready rows, and a
    /// non-empty output path. Derived — never stored.
    pub(crate) fn can_render(&self) -> bool {
        let Some(rows) = self.playlist_state.selected_rows() else {
            return false;
        };
        self.render_cancel.is_none()
            && !self.render_out.trim().is_empty()
            && rows.len() >= 2
            && rows.iter().all(|h| self.tracks.is_ready(h))
    }

    /// Snapshots the selected playlist into a mixdown job from the
    /// track database at click time; later grid edits cannot affect
    /// the returned job.
    pub(crate) fn build_mixdown_job(&self) -> Option<automixah_engine::mixdown::MixdownJob> {
        let rows = self.playlist_state.selected_rows()?;
        if rows.len() < 2 {
            return None;
        }
        let tracks = rows
            .iter()
            .map(|hash| {
                let record = self.tracks.get(hash)?;
                let crate::tracks::AnalysisState::Ready(analysis) = &record.analysis else {
                    return None;
                };
                let grid = crate::grid::EditableGrid::from_grid(&analysis.grid);
                Some(automixah_engine::mixdown::MixdownTrack {
                    hash: hash.clone(),
                    path: record.tags.path.clone(),
                    grid_bpm: grid.grid_bpm,
                    anchor_seconds: grid.anchor_seconds,
                    downbeat_phase: grid.downbeat_phase,
                    key: analysis.key.clone(),
                    duration: analysis.duration_seconds,
                    cues: analysis.cues,
                })
            })
            .collect::<Option<Vec<_>>>()?;
        let out = std::path::PathBuf::from(self.render_out.trim());

        Some(automixah_engine::mixdown::MixdownJob {
            tracks,
            out,
            bpm: self.render_bpm,
        })
    }

    /// Starts the mixdown on a blocking thread. Progress lands on
    /// the bus (throttled to ~10/s per stage class); the cancel flag
    /// is stored until a terminal render event clears it.
    fn spawn_render(&mut self) {
        let Some(job) = self.build_mixdown_job() else {
            return;
        };
        let cancel = Arc::new(AtomicBool::new(false));
        self.render_cancel = Some(Arc::clone(&cancel));
        self.render_stage = None;
        let tx = self.bus.sender();
        let out = job.out.clone();
        let handle = self.services.runtime.handle().clone();
        handle.spawn_blocking(move || {
            let mut last = ThrottleState::default();
            let mut progress = |stage: automixah_engine::mixdown::MixdownStage| {
                let bus_stage = to_bus_stage(stage);
                if last.should_send(bus_stage) {
                    let _ = tx.send(Event::RenderProgress { stage: bus_stage });
                }
            };
            let is_cancelled = || cancel.load(std::sync::atomic::Ordering::Relaxed);
            let outcome =
                automixah_engine::mixdown::run_mixdown(&job, &mut progress, &is_cancelled);
            let event = match outcome {
                MixdownOutcome::Done => Event::RenderDone { out },
                MixdownOutcome::Cancelled => Event::RenderCancelled,
                MixdownOutcome::Failed(message) => Event::RenderFailed { message },
            };
            let _ = tx.send(event);
        });
    }

    /// Cancels the in-flight mixdown. Only the terminal render event
    /// may clear `render_cancel` — the worker must observe the flag
    /// and report back before the app returns to idle.
    fn cancel_render(&mut self) {
        if let Some(flag) = &self.render_cancel {
            flag.store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Opens the system save dialog for the mixdown output path.
    fn browse_render_out(&mut self) {
        if let Some(path) = rfd::FileDialog::new()
            .set_title("Render mix to")
            .add_filter("WAV", &["wav"])
            .set_file_name("mix.wav")
            .save_file()
            && let Some(text) = path.to_str()
        {
            self.render_out = text.to_owned();
        }
    }
}

/// Progress-send throttle state: emit on stage-class change or
/// after the interval elapses.
#[derive(Default)]
struct ThrottleState {
    last_sent: Option<(std::time::Instant, crate::bus::RenderStage)>,
}

impl ThrottleState {
    fn should_send(&mut self, stage: crate::bus::RenderStage) -> bool {
        let now = std::time::Instant::now();
        if let Some((sent_at, last)) = self.last_sent
            && std::mem::discriminant(&last) == std::mem::discriminant(&stage)
            && now.duration_since(sent_at) < PROGRESS_INTERVAL
        {
            return false;
        }
        self.last_sent = Some((now, stage));
        true
    }
}

/// Minimum spacing between same-class progress events.
const PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

fn to_bus_stage(stage: automixah_engine::mixdown::MixdownStage) -> crate::bus::RenderStage {
    use crate::bus::RenderStage as Bus;
    use automixah_engine::mixdown::MixdownStage as Eng;
    match stage {
        Eng::Decoding { done, total } => Bus::Decoding { done, total },
        Eng::Stretching { done, total } => Bus::Stretching { done, total },
        Eng::Mixing { fraction } => Bus::Mixing { fraction },
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

    /// Seeds library state as if a `LibraryLoaded` had applied.
    pub fn seed_library_for_test(
        &mut self,
        roots: Vec<crate::library::store::LibraryRoot>,
        entries: Vec<crate::library::store::LibraryEntry>,
    ) {
        self.library_state.roots = roots;
        self.library_state.entries = entries;
    }

    /// Drives the library add-track path like a double-click would.
    pub fn add_library_track_for_test(&mut self, hash: TrackHash) {
        self.spawn_add_library_track(hash);
    }

    /// Selects a playlist by id without the contents load round-trip
    /// (tests that only exercise the add path).
    pub fn select_playlist_for_test(&mut self, id: i64) {
        self.playlist_state.selected = Some(id);
        self.playlist_state.contents = Contents::Loaded(Vec::new());
    }

    /// Blocks on the runtime until spawned tasks settle, then drains.
    pub fn bus_receiver_for_test(&mut self) -> &std::sync::mpsc::Receiver<Event> {
        self.bus.receiver_for_test()
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

    // Given a complete persisted track plus a stored cue map.
    // When hydrated with that cue map.
    // Then the Ready analysis carries the stored in-cue position.
    #[test]
    fn hydrate_records_carries_stored_cues_into_ready_analysis() {
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
        let mut cues = std::collections::HashMap::new();
        cues.insert(
            TrackHash("done".to_owned()),
            automixah_engine::timeline::types::CuePoints::with_in(2, 44_100 * 30),
        );

        let (_, records) = hydrate_records(vec![complete], &cues);

        assert_eq!(records.len(), 1, "complete track hydrated");
        let AnalysisState::Ready(analysis) = &records[0].analysis else {
            panic!("ready");
        };
        assert_eq!(
            analysis
                .cues
                .get(automixah_engine::timeline::types::CueKind::In, 2),
            Some(44_100 * 30),
            "stored cue hydrates into the record"
        );
    }

    #[tokio::test]
    async fn cue_hydration_reads_incomplete_tracks_without_grid_data() {
        // Given a playlist row with no grid yet and independently persisted cues.
        let hash = TrackHash("pending-cues".to_owned());
        let persisted = vec![crate::playlist::store::PersistedTrack {
            id: 1,
            position: 0,
            track_hash: hash.clone(),
            title: "T".to_owned(),
            artist: "A".to_owned(),
            added_path: "/pending.wav".to_owned(),
            duration: None,
            grid: None,
        }];
        let cues = automixah_engine::timeline::types::CuePoints::with_out(1, 900);
        let cue_store = crate::store::CueStoreService::new(std::sync::Arc::new(
            crate::store::in_memory::InMemoryCueStore::new(),
        ));
        cue_store.put(&hash, &cues).await.expect("seed cues");

        // When the playlist hydration reads cue data before analysis completes.
        let loaded = load_cues(cue_store, &persisted).await;

        // Then the pending row's source cues remain available for later work.
        assert_eq!(loaded.get(&hash), Some(&cues));
    }
    #[test]
    fn stale_cue_save_event_does_not_replace_newer_working_state() {
        // Given a loaded deck and track record with a newer cue edit.
        let mut app = AutomixahUiApp::new(
            crate::playlist::queue::tests::fake_services(
                crate::playlist::queue::tests::output_fixture(),
            ),
            crate::bus::EventBus::without_repaint(),
        );
        let hash = TrackHash("cue-race".to_owned());
        let old = automixah_engine::timeline::types::CuePoints::with_in(0, 100);
        let newer = automixah_engine::timeline::types::CuePoints::with_in(0, 300);
        app.tracks.upsert(TrackRecord {
            hash: hash.clone(),
            tags: crate::tracks::TrackTags {
                title: "T".to_owned(),
                artist: "A".to_owned(),
                path: "/cue-race.wav".into(),
            },
            analysis: AnalysisState::Ready(crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 128.0,
                key: djcore::key::Key {
                    root: 9,
                    mode: djcore::key::KeyMode::Minor,
                },
                duration_seconds: 1.0,
                cues: newer,
            }),
        });
        app.inject_deck_for_test(crate::bus::LoadOutcome {
            hash: hash.clone(),
            path: "/cue-race.wav".into(),
            analysis: crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 128.0,
                key: djcore::key::Key {
                    root: 9,
                    mode: djcore::key::KeyMode::Minor,
                },
                duration_seconds: 1.0,
                cues: newer,
            },
            audio: djcore::decoder::DecodeAudio {
                samples: vec![0.0; 4],
                sample_rate: 44_100,
                channels: 1,
            },
            peaks: crate::audio::peaks::Peaks::build_with_channels(&[0.0; 4], 44_100, 1),
        });

        // When the older in-flight save reports success.
        app.apply(Event::CuesSaved {
            hash: hash.clone(),
            cues: old,
        });

        // Then the newer working/session cue set remains authoritative.
        assert_eq!(app.deck.as_ref().expect("deck").cues, newer);
        let AnalysisState::Ready(analysis) = &app.tracks.get(&hash).expect("record").analysis
        else {
            panic!("ready record");
        };
        assert_eq!(analysis.cues, newer);
    }
    #[test]
    fn current_cue_save_event_clears_in_flight_state_and_refreshes_record() {
        // Given a ready track with one cue snapshot being persisted.
        let mut app = AutomixahUiApp::new(
            crate::playlist::queue::tests::fake_services(
                crate::playlist::queue::tests::output_fixture(),
            ),
            crate::bus::EventBus::without_repaint(),
        );
        let hash = TrackHash("cue-save".to_owned());
        let cues = automixah_engine::timeline::types::CuePoints::with_in(0, 400);
        app.tracks.upsert(TrackRecord {
            hash: hash.clone(),
            tags: crate::tracks::TrackTags {
                title: "T".to_owned(),
                artist: "A".to_owned(),
                path: "/cue-save.wav".into(),
            },
            analysis: AnalysisState::Ready(crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 128.0,
                key: djcore::key::Key {
                    root: 0,
                    mode: djcore::key::KeyMode::Major,
                },
                duration_seconds: 1.0,
                cues,
            }),
        });
        app.inject_deck_for_test(crate::bus::LoadOutcome {
            hash: hash.clone(),
            path: "/cue-save.wav".into(),
            analysis: crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 128.0,
                key: djcore::key::Key {
                    root: 0,
                    mode: djcore::key::KeyMode::Major,
                },
                duration_seconds: 1.0,
                cues,
            },
            audio: djcore::decoder::DecodeAudio {
                samples: vec![0.0; 4],
                sample_rate: 44_100,
                channels: 1,
            },
            peaks: crate::audio::peaks::Peaks::build_with_channels(&[0.0; 4], 44_100, 1),
        });
        let deck = app.deck.as_mut().expect("deck");
        deck.pending_cue_save = Some((hash.clone(), cues));
        deck.cue_save_in_flight = Some((hash.clone(), cues));
        deck.cues_dirty = true;

        // When the matching save success arrives.
        app.apply(Event::CuesSaved {
            hash: hash.clone(),
            cues,
        });

        // Then persistence state clears and the track record stays current.
        let deck = app.deck.as_ref().expect("deck");
        assert!(deck.cue_save_in_flight.is_none());
        assert!(!deck.cues_dirty);
        let AnalysisState::Ready(analysis) = &app.tracks.get(&hash).expect("record").analysis
        else {
            panic!("ready record");
        };
        assert_eq!(analysis.cues, cues);
    }

    #[test]
    fn failed_cue_save_keeps_working_cues_dirty() {
        // Given a cue snapshot whose asynchronous save is in flight.
        let mut app = AutomixahUiApp::new(
            crate::playlist::queue::tests::fake_services(
                crate::playlist::queue::tests::output_fixture(),
            ),
            crate::bus::EventBus::without_repaint(),
        );
        let hash = TrackHash("cue-fail".to_owned());
        let cues = automixah_engine::timeline::types::CuePoints::with_out(1, 500);
        app.inject_deck_for_test(crate::bus::LoadOutcome {
            hash: hash.clone(),
            path: "/cue-fail.wav".into(),
            analysis: crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 128.0,
                key: djcore::key::Key {
                    root: 0,
                    mode: djcore::key::KeyMode::Major,
                },
                duration_seconds: 1.0,
                cues,
            },
            audio: djcore::decoder::DecodeAudio {
                samples: vec![0.0; 4],
                sample_rate: 44_100,
                channels: 1,
            },
            peaks: crate::audio::peaks::Peaks::build_with_channels(&[0.0; 4], 44_100, 1),
        });
        let deck = app.deck.as_mut().expect("deck");
        deck.cue_save_in_flight = Some((hash.clone(), cues));
        deck.cues_dirty = true;

        // When the save fails.
        app.apply(Event::CuesSaveFailed {
            hash,
            cues,
            message: "write failed".to_owned(),
        });

        // Then working data remains dirty for a later retry.
        let deck = app.deck.as_ref().expect("deck");
        assert!(deck.cue_save_in_flight.is_none());
        assert!(deck.cues_dirty);
        assert!(app.status.contains("write failed"));
    }
    // Given a load outcome carrying persisted cues.
    // When the outcome is applied to the editor.
    // Then the loaded deck starts with the same working cue set.
    #[test]
    fn loading_initializes_deck_working_cues() {
        let mut app = AutomixahUiApp::new(
            crate::playlist::queue::tests::fake_services(
                crate::playlist::queue::tests::output_fixture(),
            ),
            crate::bus::EventBus::without_repaint(),
        );
        let cues = automixah_engine::timeline::types::CuePoints {
            ins: [Some(100), None, None, None],
            outs: [None, None, Some(300), None],
        };

        app.apply_load_done(crate::bus::LoadOutcome {
            hash: TrackHash("deck-cues".to_owned()),
            path: "/deck-cues.wav".into(),
            analysis: crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid::default(),
                bpm: 128.0,
                key: djcore::key::Key {
                    root: 9,
                    mode: djcore::key::KeyMode::Minor,
                },
                duration_seconds: 1.0,
                cues,
            },
            audio: djcore::decoder::DecodeAudio {
                samples: vec![0.0; 4],
                sample_rate: 44_100,
                channels: 1,
            },
            peaks: crate::audio::peaks::Peaks::build_with_channels(&[0.0; 4], 44_100, 1),
        });

        // Then the deck owns the loaded cue snapshot for editing.
        assert_eq!(app.deck.as_ref().expect("deck").cues, cues);
        assert!(!app.deck.as_ref().expect("deck").cues_dirty);
    }
    #[test]
    fn grid_change_resnaps_working_cues_and_queues_complete_save() {
        // Given a loaded deck with an off-beat cue and a changed grid.
        let mut app = AutomixahUiApp::new(
            crate::playlist::queue::tests::fake_services(
                crate::playlist::queue::tests::output_fixture(),
            ),
            crate::bus::EventBus::without_repaint(),
        );
        let hash = TrackHash("resnap".to_owned());
        let cues = automixah_engine::timeline::types::CuePoints::with_in(0, 10_000);
        app.inject_deck_for_test(crate::bus::LoadOutcome {
            hash: hash.clone(),
            path: "/resnap.wav".into(),
            analysis: crate::tracks::Analysis {
                grid: djcore::analyzer::BeatGrid {
                    grid_bpm: 120.0,
                    anchor_seconds: 0.0,
                    ..Default::default()
                },
                bpm: 120.0,
                key: djcore::key::Key {
                    root: 0,
                    mode: djcore::key::KeyMode::Major,
                },
                duration_seconds: 1.0,
                cues,
            },
            audio: djcore::decoder::DecodeAudio {
                samples: vec![0.0; 44_100],
                sample_rate: 44_100,
                channels: 1,
            },
            peaks: crate::audio::peaks::Peaks::build_with_channels(&vec![0.0; 44_100], 44_100, 1),
        });
        let deck = app.deck.as_mut().expect("deck");
        deck.edit_grid.anchor_seconds = 0.1;
        deck.grid_dirty = true;

        // When the changed grid is processed.
        app.resnap_deck_cues_after_grid_change();

        // Then the cue is re-snapped and a complete cue-set save is pending.
        let deck = app.deck.as_ref().expect("deck");
        assert_eq!(
            deck.cues
                .get(automixah_engine::timeline::types::CueKind::In, 0),
            Some(4_410)
        );
        assert!(deck.cues_dirty);
        assert_eq!(deck.pending_cue_save, Some((hash, deck.cues)));
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
                cues: automixah_engine::timeline::types::CuePoints::default(),
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
                cues: automixah_engine::timeline::types::CuePoints::default(),
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
                cues: automixah_engine::timeline::types::CuePoints::default(),
            }),
        });
        let mut pending = Vec::new();

        enqueue_if_needed(
            &mut tracks,
            &TrackHash("known".to_owned()),
            EnqueueTier::Playlist,
            &mut pending,
        );

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

        enqueue_if_needed(
            &mut tracks,
            &TrackHash("fresh".to_owned()),
            EnqueueTier::Playlist,
            &mut pending,
        );

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
                cues: automixah_engine::timeline::types::CuePoints::default(),
            },
            audio: djcore::decoder::DecodeAudio {
                samples: Vec::new(),
                sample_rate: 44_100,
                channels: 2,
            },
            peaks: crate::audio::peaks::Peaks::build_with_channels(&[], 44_100, 2),
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
                cues: automixah_engine::timeline::types::CuePoints::default(),
            },
            audio: djcore::decoder::DecodeAudio {
                samples: Vec::new(),
                sample_rate: 44_100,
                channels: 2,
            },
            peaks: crate::audio::peaks::Peaks::build_with_channels(&[], 44_100, 2),
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
                cues: automixah_engine::timeline::types::CuePoints::default(),
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
                cues: automixah_engine::timeline::types::CuePoints::default(),
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
                cues: automixah_engine::timeline::types::CuePoints::default(),
            },
        });

        assert!(
            app.pending_deck_load.is_none(),
            "a bare done event never arms or loads"
        );
    }
}

#[cfg(test)]
mod render_tests {
    use super::*;
    use crate::bus::RenderStage;
    use crate::tracks::Analysis;

    fn app_with_ready_playlist(rows: usize) -> AutomixahUiApp {
        let mut app = AutomixahUiApp::new(
            crate::playlist::queue::tests::fake_services(
                crate::playlist::queue::tests::output_fixture(),
            ),
            crate::bus::EventBus::without_repaint(),
        );
        let hashes: Vec<TrackHash> = (0..rows).map(|i| TrackHash(format!("r{i}"))).collect();
        for (i, hash) in hashes.iter().enumerate() {
            app.tracks.upsert(TrackRecord {
                hash: hash.clone(),
                tags: crate::tracks::TrackTags {
                    title: format!("T{i}"),
                    artist: String::new(),
                    path: format!("/t{i}.wav").into(),
                },
                analysis: AnalysisState::Ready(Analysis {
                    grid: crate::grid::EditableGrid {
                        grid_bpm: 128.0,
                        anchor_seconds: 0.25,
                        downbeat_phase: 2,
                    }
                    .project(),
                    bpm: 128.0,
                    key: djcore::key::Key {
                        root: 9,
                        mode: djcore::key::KeyMode::Minor,
                    },
                    duration_seconds: 60.0,
                    cues: automixah_engine::timeline::types::CuePoints::default(),
                }),
            });
        }
        // Seed one in-cue on r0 so render-time snapshotting carries it.
        app.tracks.refresh_cues(
            &hashes[0],
            &automixah_engine::timeline::types::CuePoints::with_in(0, 44_100 * 8),
        );
        app.playlist_state.selected = Some(7);
        app.playlist_state.contents = Contents::Loaded(hashes);
        app
    }

    // Given every render precondition met.
    // When deriving button enablement.
    // Then rendering is allowed.
    #[test]
    fn can_render_allows_fully_ready_playlist() {
        let mut app = app_with_ready_playlist(2);
        app.render_out = "  /out/mix.wav  ".to_owned();

        assert!(app.can_render());
    }

    // Given only one ready row.
    // When deriving button enablement.
    // Then rendering is disallowed (two-row minimum).
    #[test]
    fn can_render_requires_two_rows() {
        let mut app = app_with_ready_playlist(1);
        app.render_out = "/out/mix.wav".to_owned();

        assert!(!app.can_render());
    }

    // Given a playlist with a pending row.
    // When deriving button enablement.
    // Then rendering is disallowed.
    #[test]
    fn can_render_requires_all_rows_ready() {
        let mut app = app_with_ready_playlist(2);
        app.render_out = "/out/mix.wav".to_owned();
        app.tracks.clear_analysis(&TrackHash("r0".to_owned()));

        assert!(!app.can_render());
    }

    // Given a render already in flight.
    // When deriving button enablement.
    // Then rendering is disallowed.
    #[test]
    fn can_render_blocks_while_rendering() {
        let mut app = app_with_ready_playlist(2);
        app.render_out = "/out/mix.wav".to_owned();
        app.render_cancel = Some(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(
            false,
        )));

        assert!(!app.can_render());
    }

    // Given a ready two-row playlist and a whitespace path.
    // When deriving button enablement.
    // Then rendering is disallowed.
    #[test]
    fn can_render_requires_nonempty_trimmed_path() {
        let mut app = app_with_ready_playlist(2);
        app.render_out = "   ".to_owned();

        assert!(!app.can_render());
    }

    // Given a ready two-row playlist with no selection.
    // When deriving button enablement.
    // Then rendering is disallowed.
    #[test]
    fn can_render_requires_a_selected_playlist() {
        let mut app = app_with_ready_playlist(2);
        app.render_out = "/out/mix.wav".to_owned();
        app.playlist_state.selected = None;
        app.playlist_state.contents = Contents::None;

        assert!(!app.can_render());
    }

    // Given a ready two-row playlist.
    // When snapshotting the job.
    // Then it carries ordered tracks with the stored canonical
    // triple, key, duration, and the trimmed output path.
    #[test]
    fn build_mixdown_job_snapshots_metadata_in_row_order() {
        let mut app = app_with_ready_playlist(2);
        app.render_out = " /out/mix.wav ".to_owned();

        let job = app.build_mixdown_job().expect("job");
        let snapshotted = job.tracks[0].cues;

        // When the live record changes after the job is built.
        app.tracks.refresh_cues(
            &TrackHash("r0".to_owned()),
            &automixah_engine::timeline::types::CuePoints::with_in(0, 44_100 * 16),
        );

        // Then the in-flight job retains its click-time cue snapshot.
        assert_eq!(job.tracks[0].cues, snapshotted);
        assert_eq!(
            job.tracks[0]
                .cues
                .get(automixah_engine::timeline::types::CueKind::In, 0),
            Some(44_100 * 8),
            "later session edits cannot mutate the job"
        );

        assert_eq!(job.out, std::path::PathBuf::from("/out/mix.wav"));
        assert_eq!(job.tracks.len(), 2);
        assert_eq!(job.tracks[0].hash.0, "r0");
        assert_eq!(job.tracks[1].hash.0, "r1");
        // r0's seeded in-cue snapshots into the job.
        assert_eq!(
            job.tracks[0]
                .cues
                .get(automixah_engine::timeline::types::CueKind::In, 0),
            Some(44_100 * 8),
            "job carries the click-time in-cue snapshot"
        );
        for track in &job.tracks {
            assert_eq!(track.grid_bpm, 128.0);
            assert_eq!(track.anchor_seconds, 0.25);
            assert_eq!(track.downbeat_phase, 2);
            assert_eq!(track.duration, 60.0);
            assert_eq!(track.key.root, 9);
            assert!(track.path.to_string_lossy().contains(".wav"));
        }
    }

    // Given a render in flight with staged progress.
    // When a progress event applies.
    // Then the stage displays.
    #[test]
    fn render_progress_event_updates_display_state() {
        let mut app = app_with_ready_playlist(2);
        app.render_cancel = Some(Arc::new(AtomicBool::new(false)));

        app.apply(Event::RenderProgress {
            stage: RenderStage::Mixing { fraction: 0.5 },
        });

        assert_eq!(
            app.render_stage,
            Some(RenderStage::Mixing { fraction: 0.5 })
        );
    }

    // Given a render in flight.
    // When the terminal done event applies.
    // Then the app returns to idle and the status reports the path.
    #[test]
    fn render_done_event_clears_in_flight_and_reports_success() {
        let mut app = app_with_ready_playlist(2);
        app.render_cancel = Some(Arc::new(AtomicBool::new(false)));
        app.render_stage = Some(RenderStage::Mixing { fraction: 0.5 });

        app.apply(Event::RenderDone {
            out: "/out/mix.wav".into(),
        });

        assert!(app.render_cancel.is_none());
        assert!(app.render_stage.is_none());
        assert!(app.status.contains("/out/mix.wav"));
    }

    // Given a render in flight.
    // When the terminal cancelled event applies.
    // Then the app returns to idle.
    #[test]
    fn render_cancelled_event_restores_idle_state() {
        let mut app = app_with_ready_playlist(2);
        app.render_cancel = Some(Arc::new(AtomicBool::new(false)));
        app.render_stage = Some(RenderStage::Decoding { done: 1, total: 2 });

        app.apply(Event::RenderCancelled);

        assert!(app.render_cancel.is_none());
        assert!(app.render_stage.is_none());
    }

    // Given a render in flight.
    // When the terminal failed event applies.
    // Then the app returns to idle with the message shown.
    #[test]
    fn render_failed_event_restores_idle_state() {
        let mut app = app_with_ready_playlist(2);
        app.render_cancel = Some(Arc::new(AtomicBool::new(false)));

        app.apply(Event::RenderFailed {
            message: "boom".to_owned(),
        });

        assert!(app.render_cancel.is_none());
        assert!(app.status.contains("boom"));
    }

    // Given a fresh throttle and a stage.
    // When asked twice in quick succession.
    // Then only the first sends and a class change always sends.
    #[test]
    fn throttle_suppresses_same_class_within_interval() {
        let mut throttle = ThrottleState::default();
        let first = throttle.should_send(RenderStage::Decoding { done: 0, total: 2 });
        let repeat = throttle.should_send(RenderStage::Decoding { done: 1, total: 2 });
        let class_change = throttle.should_send(RenderStage::Mixing { fraction: 0.0 });

        assert!(first, "first event sends");
        assert!(!repeat, "same class inside the interval is suppressed");
        assert!(class_change, "stage-class change sends immediately");
    }

    fn lib_root(id: i64, path: &str) -> crate::library::store::LibraryRoot {
        crate::library::store::LibraryRoot {
            id,
            path: std::path::PathBuf::from(path),
        }
    }

    fn lib_entry(
        root_id: i64,
        rel: &str,
        title: &str,
        hash: &str,
    ) -> crate::library::store::LibraryEntry {
        crate::library::store::LibraryEntry {
            root_id,
            rel_path: std::path::PathBuf::from(rel),
            hash: TrackHash(hash.to_owned()),
            title: title.to_owned(),
            artist: "Artist".to_owned(),
            duration: Some(61.0),
            mtime_secs: 0,
            size_bytes: 0,
        }
    }

    // Given a refresh whose index holds one analyzed hash and one
    // un-analyzed hash (both under a known root).
    // When the backfill derivation runs.
    // Then only the un-analyzed hash is enrolled, with the absolute
    // path joined from its root.
    #[test]
    fn refresh_derivation_enrolls_only_missing_hashes() {
        let mut tracks = crate::tracks::Tracks::default();
        let mut pending = Vec::new();
        let roots = vec![lib_root(1, "/music")];
        let entries = vec![
            lib_entry(1, "a.flac", "Analyzed", "analyzed-hash"),
            lib_entry(1, "b.flac", "Missing", "missing-hash"),
        ];
        let analyzed = [TrackHash("analyzed-hash".to_owned())].into();

        enqueue_library_backfill(&mut tracks, &roots, &entries, &analyzed, &mut pending);

        assert_eq!(pending.len(), 1, "only the un-analyzed hash enrolls");
        assert_eq!(pending[0].0.hash.0, "missing-hash");
        assert_eq!(
            pending[0].0.path,
            std::path::PathBuf::from("/music/b.flac"),
            "absolute path joined from root"
        );
        assert!(!pending[0].0.force, "backfill never forces");
    }

    // Given an index with the same content at two locations.
    // When the backfill derivation runs.
    // Then one job is enqueued for the first occurrence only.
    #[test]
    fn duplicate_hash_across_roots_enrolls_once() {
        let mut tracks = crate::tracks::Tracks::default();
        let mut pending = Vec::new();
        let roots = vec![lib_root(1, "/music-one"), lib_root(2, "/music-two")];
        let entries = vec![
            lib_entry(1, "a.flac", "T", "dup"),
            lib_entry(2, "b.flac", "T", "dup"),
        ];
        let analyzed = std::collections::HashSet::new();

        enqueue_library_backfill(&mut tracks, &roots, &entries, &analyzed, &mut pending);

        assert_eq!(pending.len(), 1, "same content enrolls once");
        assert_eq!(
            pending[0].0.path,
            std::path::PathBuf::from("/music-one/a.flac"),
            "first root wins"
        );
    }

    // Given a refresh over unknown hashes.
    // When the derivation seeds placeholder records.
    // Then each record carries the entry's artist/title/path so rows
    // render real display facts before analysis completes.
    #[test]
    fn refresh_seeds_tags_for_placeholder_records() {
        let mut tracks = crate::tracks::Tracks::default();
        let mut pending = Vec::new();
        let roots = vec![lib_root(1, "/music")];
        let entries = vec![lib_entry(1, "deep/track.flac", "Nightfall", "seed-me")];
        let analyzed = std::collections::HashSet::new();

        enqueue_library_backfill(&mut tracks, &roots, &entries, &analyzed, &mut pending);

        let record = tracks
            .get(&TrackHash("seed-me".to_owned()))
            .expect("seeded");
        assert_eq!(record.tags.title, "Nightfall");
        assert_eq!(record.tags.artist, "Artist");
        assert_eq!(
            record.tags.path,
            std::path::PathBuf::from("/music/deep/track.flac")
        );
        assert!(matches!(record.analysis, AnalysisState::Queued));
    }

    // Given a hash that previously failed analysis in this session.
    // When a later library refresh runs the derivation again.
    // Then it is not re-enrolled (no retry storm).
    #[test]
    fn failed_state_suppresses_reenrollment_on_next_refresh() {
        let mut tracks = crate::tracks::Tracks::default();
        tracks.set_analysis(
            &TrackHash("doomed".to_owned()),
            AnalysisState::Failed("corrupt".to_owned()),
        );
        let mut pending = Vec::new();
        let roots = vec![lib_root(1, "/music")];
        let entries = vec![lib_entry(1, "c.flac", "Doomed", "doomed")];
        let analyzed = std::collections::HashSet::new();

        enqueue_library_backfill(&mut tracks, &roots, &entries, &analyzed, &mut pending);

        assert!(pending.is_empty(), "failed hash never re-enrolls");
    }

    // Given a populated index (one analyzed hash pre-stored, one not)
    // and real WAV files behind the entries.
    // When a `LibraryLoaded` flows through drain_bus with the analysis
    // worker running and completion events are drained back.
    // Then only the un-analyzed hash is enqueued at Library priority,
    // its record ends Ready, and events never address rows — hashes
    // throughout.
    // (Plain sync test: `fake_services` owns the runtime; nesting a
    // #[tokio::test] runtime would deadlock block_on.)
    #[test]
    fn backfill_end_to_end_with_fake_analyzer() {
        // Given: fake services + real files for decodable content.
        let services = crate::playlist::queue::tests::fake_services(
            crate::playlist::queue::tests::output_fixture(),
        );
        let dir = tempfile::tempdir().expect("temp");
        let bytes_a = crate::playlist::queue::tests::wav_bytes(1.0);
        let mut bytes_b = bytes_a.clone();
        let last = bytes_b.len() - 1;
        bytes_b[last] = 7; // distinct content → distinct hash
        std::fs::write(dir.path().join("a.wav"), &bytes_a).expect("write");
        std::fs::write(dir.path().join("b.wav"), &bytes_b).expect("write");
        let hash_a = TrackHash(crate::track::identity::hex_sha256(&bytes_a));
        let hash_b = TrackHash(crate::track::identity::hex_sha256(&bytes_b));

        // And the store already holds an analysis for hash A.
        services
            .runtime
            .block_on(async {
                services
                    .grid_store
                    .put(
                        &hash_a,
                        &crate::store::GridOverride {
                            grid_bpm: 140.0,
                            anchor_seconds: 0.0,
                            downbeat_phase: 0,
                            updated_at: 1,
                            key: Some(djcore::key::Key {
                                root: 0,
                                mode: djcore::key::KeyMode::Major,
                            }),
                        },
                    )
                    .await
            })
            .expect("seed analyzed grid");

        // And an app wired to that container.
        let mut app =
            AutomixahUiApp::new(services.clone(), crate::bus::EventBus::without_repaint());

        // When the refresh event arrives (startup hydration shape).
        app.bus.send(Event::LibraryLoaded {
            roots: vec![lib_root(1, dir.path().to_str().expect("utf8"))],
            entries: vec![
                lib_entry(1, "a.wav", "Analyzed", &hash_a.0),
                lib_entry(1, "b.wav", "Backfill", &hash_b.0),
            ],
            analyzed: [hash_a.clone()].into(),
        });

        // Drain derives the jobs and flushes them to the worker.
        app.drain_bus();
        assert!(
            matches!(
                app.tracks.get(&hash_b).map(|r| &r.analysis),
                Some(AnalysisState::Queued | AnalysisState::Analyzing)
            ),
            "un-analyzed hash enrolls"
        );

        // The analyzed hash must have enrolled nothing (no record from
        // this derivation beyond what derivation seeds — here neither:
        // it was filtered before seeding).
        assert!(app.tracks.get(&hash_a).is_none(), "analyzed hash skipped");

        // The worker reports by hash; repeated drains play the UI frame
        // role until the record derives Ready.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !matches!(
            app.tracks.get(&hash_b).map(|r| &r.analysis),
            Some(AnalysisState::Ready(_))
        ) {
            assert!(
                std::time::Instant::now() < deadline,
                "backfill never completed"
            );
            std::thread::sleep(std::time::Duration::from_millis(10));
            app.drain_bus();
        }
        assert!(matches!(
            app.tracks.get(&hash_b).map(|r| &r.analysis),
            Some(AnalysisState::Ready(_))
        ));

        // And the fresh analysis persists (grid + key on hash B).
        let stored = services
            .runtime
            .block_on(async { services.grid_store.get(&hash_b).await })
            .expect("lookup")
            .expect("backfilled grid persisted");
        assert!(stored.key.is_some());
    }
}
