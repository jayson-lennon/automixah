# Event Bus + Sync Redesign, Audio Fixes, Gap Fixes

## Problem

automixah-ui's frontend state has three concurrent writers — panel handlers mutating `PlaylistState` directly, hydration tasks replacing rows wholesale (`set_rows`), and the analysis worker addressing rows by locally-minted ids — plus fire-and-forget DB writes (`let _ = store...await`). This produced three verified sync bugs (B1: stale read races its own write; B2: row-id collisions across hydration; B3: paused audio callback clobbers UI seeks), invisible failures (G1/G4), and the zoom-dependent drag-pan lag. Hydration itself is the root architectural flaw: a retained-mode "resync the widget model" pass that an immediate-mode frontend never needed.

## Solution

A single **UI event bus** (std mpsc + egui repaint scheduling). Every spawned task/thread — editor load pipeline, grid saves, playlist CRUD, add-track, playlist-content fetch, the analysis worker, reorder/remove — finishes its work and sends one `Event` on a cloneable sender. **`apply(Event)` is the only code that mutates event-derived frontend state.** Waiting is explicit in the model via loading enums matched at render time (`Contents`, `EditorLoad`, `add_pending`). Hydration machinery is deleted: playlist names load once at startup; a playlist's contents fetch on selection (clear → spinner → one `RowsLoaded` event replaces them); every mutation is a delta event, never a re-read. Playlist rows carry **database-minted ids** (`playlist_tracks.rowid`). The bus schedules repaints on send, debounced to 50ms; each frame drains events under a 10ms budget and renders with whatever it has, continuing next frame. Separately, the waveform drag-pan regression and the stopped-state seek bug are fixed in the audio/view layer.

---

## Dialectical Outcomes (Why)

1. **Event bus over shared memory (`Arc<RwLock<PlaylistState>>`).** The channels never failed — mpsc delivered every event; the events were *addressed to wrong ids* (B2) and *raced unordered mutations* (B1). Channels preserve single-consumer ownership, give queueing for free, and keep locks out of the frame loop; shared state would recreate the multi-writer race profile unless single-writer discipline were added anyway. User-directed: "frontend calls whatever it needs to do, background tasks fire an event into a channel to update the frontend state."
2. **No hydration — load-on-demand instead.** Hydration = retained-mode resync; caused B1 (read raced write) and B2 (replacement re-minted ids from 0). Decision 1B/2B: the playlist *list* loads in one event at startup; *contents* fetch per selection with an explicit loading indicator. User: "we aren't doing web dev here — it's totally fine and expected that we display small loading indicators."
3. **DB-minted row ids over local counters/UUIDs.** One minting site (SQLite `rowid` returned from insert), stable across fetches, collision-proof by construction. Local counter re-issue was B2's mechanism.
4. **No optimistic UI.** State appears when its event confirms; failures surface as events. Rollback machinery is what we're deleting, not adding.
5. **Pan fix: view follows the pointer, audio follows the view.** The lag existed because the *view* tracked the audio thread's position, and scrub speed is clamped to ±8× (`ScrubMachine::command`). At high zoom-out the pointer requests more than 8 source-seconds/wall-second, so the waveform capped out while the cursor raced. Audio stays clamped (vinyl behavior is a *consequence*, not the driver); during drag the view accumulates raw pointer deltas unclamped, and on release a `seek` snaps audio to the pointer position.
6. **B3 fix ordering.** In the paused branch, position was written from the stale scrub position *before* the `seek` consumption below it; the callback clobbered every UI seek until playback resumed. Consume seeks first.
7. **Debounce via `request_repaint_after` earliest-wins semantics.** egui merges repaint requests (earliest deadline wins), so a send-storm coalesces into ~50ms windows for free; the explicit deadline check avoids redundant scheduling.

## Relevant Files (Where)

New:
- `crates/automixah-ui/src/bus.rs` — `Event` enum, `EventBus`, drain/debounce logic.

Modified:
- `crates/automixah-ui/src/app.rs` — `apply()` replaces the four `poll_*` methods; `Contents`/`EditorLoad`/`add_pending` state; panel actions become spawns; drag-view accumulation; bus construction.
- `crates/automixah-ui/src/track.rs` — `spawn_load` sends `Event::LoadStage`/`Event::LoadDone` on a bus sender instead of a private channel.
- `crates/automixah-ui/src/playlist/mod.rs` — hydration deleted (`spawn_hydrate`, `HydrateEvent`, `hydrate_rows`, `HydratedRows`, `set_rows`, `next_row_id`); add-track task; contents-fetch task; row-event application.
- `crates/automixah-ui/src/playlist/queue.rs` — `RowId(u64)` → `RowId(i64)` (DB rowid); worker sends bus `Event`s; uses `ensure_track`.
- `crates/automixah-ui/src/playlist/view.rs` — render matches `Contents`/`add_pending` (spinner states); rename `TextEdit`; drag insertion-line preview.
- `crates/automixah-ui/src/playlist/store/mod.rs`, `store/sqlite.rs`, `store/in_memory.rs` — `insert_track` returns rowid; new `ensure_track`, `contains_hash`, `track_duration`; `tracks_for`/`PersistedTrack` carry `id` (`pt.rowid`).
- `crates/automixah-ui/src/audio/output.rs` — paused branch consumes seeks before writing position (B3).
- `crates/automixah-ui/src/audio/scrub_state.rs` — unchanged (clamp stays); possibly a `seek_to` helper if extracted.

Unchanged: `view/waveform.rs` (pinning/clamping already support unclamped drag-follow), engine, DSP, CLI, schema.

## Key Code Context (What)

Current update-loop entry (to be reshaped — `app.rs:533`):
```rust
fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
    self.poll_loading();
    self.poll_playlist_events();
    let actions = crate::playlist::view::panel(ctx, &mut self.playlist_state);
    self.handle_panel_actions(actions);
    ...
}
```

The buggy id minting being deleted (`playlist/mod.rs:85`):
```rust
pub fn next_row_id(&mut self) -> RowId {
    self.next_row_id += 1;
    RowId(self.next_row_id)
}
```

The B3 bug (`audio/output.rs` callback, paused branch — position written before seek consumption):
```rust
if !cmd.playing {
    out.fill(0.0);
    *cb_playhead.position.write() = scrub.position();  // clobbers UI seek…
    *cb_playhead.speed.write() = 0.0;
    return;
}
if let Some(frame) = cb_playhead.seek.write().take() {  // …never reached while paused
    scrub = ScrubCore::new(channels, frame);
}
```

The pan-lag clamp (`audio/scrub_state.rs`, `command()`):
```rust
ScrubState::Dragging { .. } => ScrubCommand {
    speed: self.smoothed_drag.clamp(-8.0, 8.0) * self.unit_speed,
    playing: true,
},
```
…while the view follows the audio position (`app.rs` `follow = self.engine.as_ref().map(|e| …position…)`) — that coupling is what must break during drag.

Playhead shared state (`audio/output.rs:17`):
```rust
pub struct Playhead {
    pub position: RwLock<f32>,        // current read position, source frames
    pub seek: RwLock<Option<f32>>,    // UI sets Some(frame); audio consumes it
    pub speed: RwLock<f32>,           // source frames/sec at last callback (UI extrapolates)
}
```

Store trait today (`playlist/store/mod.rs`) — signatures that change:
```rust
async fn insert_track(&self, playlist_id: i64, hash: &TrackHash, path: &str,
    title: &str, artist: &str, duration: Option<f64>) -> Result<(), Report<PlaylistStoreError>>;
async fn tracks_for(&self, id: i64) -> Result<Vec<PersistedTrack>, Report<PlaylistStoreError>>;
```

Worker job loop signature (`playlist/queue.rs:111`):
```rust
while let Ok(job) = job_rx.recv() { run_job(services, &job, &event_tx); }
```

`QueueJob { row_id: RowId, playlist_id: i64, path: PathBuf }`; `PlaylistRow { row_id, position, path, hash: Option<TrackHash>, title, artist, bpm: Option<f32>, key: Option<Key>, duration: Option<f32>, status: RowStatus }`; `TrackMeta { hash, bpm, key, duration_seconds, title, artist }`.

## Implementation Algorithm (How)

### 1. The bus (`bus.rs`)

```rust
pub enum Event {
    // Editor load pipeline (hash → decode → analyze → peaks)
    LoadStage(LoadStage),
    LoadDone(Box<Result<(LoadedTrack, Peaks), String>>),
    // Grid saves (debounced flush outcomes)
    GridSaved(String), GridSaveFailed(String),
    // Playlist list (one load at startup; deltas after)
    PlaylistsLoaded(Vec<PlaylistSummary>),
    PlaylistCreated(PlaylistSummary),
    PlaylistRenamed { id: i64, name: String },
    PlaylistDeleted(i64),
    // Playlist contents (fetch on selection)
    RowsLoaded { playlist_id: i64, rows: Vec<PlaylistRow> },
    RowsLoadFailed { playlist_id: i64, message: String },
    // Row deltas
    RowAdded { playlist_id: i64, row: PlaylistRow },        // row.row_id = DB rowid
    RowRemoved { playlist_id: i64, row_id: RowId },
    RowsReordered { playlist_id: i64, row_ids: Vec<RowId> },
    DuplicateSkipped { playlist_id: i64, path: PathBuf },
    AddStarted { playlist_id: i64 },                         // spinner while adds pending
    // Analysis queue (addressed by DB rowid)
    RowAnalyzing { row_id: RowId },
    RowReady { row_id: RowId, meta: TrackMeta },
    RowFailed { row_id: RowId, message: String },
    CommandFailed(String),                                   // former `let _ =` outcomes
}

pub struct EventBus {
    tx: std::sync::mpsc::Sender<Event>,
    rx: std::sync::mpsc::Receiver<Event>,          // owned by the UI thread only
    repaint: Option<egui::Context>,                // None in tests
    deadline: parking_lot::Mutex<Option<Instant>>, // debounce window anchor
}
```

- `send(&self, event)`: `tx.send(event)`; then debounce — pure helper decides:
  ```rust
  fn coalesced_deadline(pending: Option<Instant>, now: Instant) -> Option<Duration>
  // Some(50ms) when no deadline or the pending one has passed; None while a
  // window is already open (egui's earliest-wins merge keeps the first).
  ```
  When `Some(d)`: store `now + 50ms` in `deadline`, `repaint.request_repaint_after(d)`.
- `drain(&self, budget: Duration, mut apply: impl FnMut(Event))`: clear `deadline`; loop `rx.try_recv()` → `apply(event)`; check `Instant::now() >= start + budget` after each apply → on budget break call `repaint.request_repaint()` (immediate) so the next frame continues grabbing. No event is ever dropped — the receiver is never cleared.
- `sender(&self)` clones `tx` for spawned tasks. Sender is `Send`; receiver stays on the UI thread (`mpsc::Receiver` is `!Sync` — fine for egui's single-threaded `update`).

### 2. Loading enums + `apply()`

State model in `AutomixahUiApp`:
```rust
enum Contents { None, Loading, Loaded(Vec<PlaylistRow>), Failed(String) }
enum EditorLoad { Idle, Loading(LoadStage), Failed(String) }  // Loaded ≡ track.is_some()
```
plus `add_pending: u32`. `poll_loading`, `poll_playlist_events`, `poll_save_outcomes`, and the hydrate receiver all die; `update()` begins with `self.bus.drain(Duration::from_millis(10), |e| self.apply(e))` (split borrow via a free fn or `apply(&mut PlaylistUiState…)` as needed).

`apply` dispatch table:
- `LoadStage(s)` → `editor = Loading(s)`; status line text.
- `LoadDone(Ok((track, peaks)))` → set track/peaks, `start_engine`, `editor` idle. `LoadDone(Err(msg))` → `editor = Failed(msg)`.
- `GridSaved/Failed` → status line (replaces `SaveOutcome` channel).
- `PlaylistsLoaded(list)` → replace playlist list (startup only).
- `PlaylistCreated(p)` → append to list; **select it** and set `contents = Loading` + spawn its fetch (contents are empty, event is instant anyway).
- `PlaylistRenamed/Deleted` → update/remove in list; on delete, clear selection → `contents = None`.
- `RowsLoaded { playlist_id, rows }` → if `playlist_id == selected`: `contents = Loaded(rows)`; **then derive re-enqueue**: rows with missing grid/key/duration → send `QueueJob { row_id: row.row_id, playlist_id, path }` to the worker. Else drop (stale).
- `RowsLoadFailed` → `contents = Failed(msg)` if still selected.
- `RowAdded { row, .. }` → if selected: push row; `add_pending -= 1`.
- `AddStarted` → `add_pending += 1`.
- `RowRemoved` / `RowsReordered` → splice `Loaded(rows)`; renumber positions locally.
- `DuplicateSkipped` → `add_pending -= 1`; status line "skipped duplicate: <file>".
- `RowAnalyzing/RowReady/RowFailed` → find row by `row_id` in `Loaded(rows)` (drop silently if absent — row was removed).
- `CommandFailed(msg)` → status line.

Row-scoped events carry `playlist_id` and are **dropped when ≠ selected** (contents are refetched on switch, so no stale writes). Playlist-scoped events (Created/Renamed/Deleted) always apply to the list.

### 3. Flows

**Startup** (`AutomixahUiApp::new`, using `cc.egui_ctx`): construct `EventBus`; spawn one task: `list_playlists()` → `send(PlaylistsLoaded(list))`. Selected = first; if any, `contents = Loading` + spawn fetch.

**Select playlist** (panel action): handler sets `selected = Some(id)` and `contents = Loading` directly (selection is view-local input state, like drag/zoom — see Anti-Goals), spawns fetch task: `tracks_for(id)` → join grids (grid store per hash, as `join_grids` does today) → build `PlaylistRow`s with `row_id = persisted.id` (DB rowid), status `Ready` only when grid+key+duration complete → `send(RowsLoaded { .. })`. `hydrate_rows`/`set_rows`/`next_row_id` deleted.

**Create playlist**: spawn `create_playlist("Playlist N")` → `send(PlaylistCreated(summary))`. No re-read anywhere — B1 dead by construction.

**Add tracks** (`Add…` → multi-select dialog): for each picked path spawn add-task: `send(AddStarted)` → read file + hash → probe tags (`filename_fallback` fallback) → `contains_hash(playlist_id, hash)?` → yes: `send(DuplicateSkipped)`; no: `insert_track(...)` (now returns rowid) → `send(RowAdded { row: PlaylistRow { row_id: RowId(rowid), status: Queued, .. } })` → library-hit check (grid+key present): hit → `send(RowReady { row_id, meta })`; miss → enqueue `QueueJob { row_id, playlist_id, path }`. A UNIQUE violation from `insert_track` maps to `DuplicateSkipped` (belt & braces). UI appends the row only when `RowAdded` lands — B2 dead: no local minting, events address DB ids.

**Worker** (`queue.rs`): single thread unchanged (`while let Ok(job) = job_rx.recv()`); emits `RowAnalyzing`/`RowReady`/`RowFailed` on the bus sender. Uses `ensure_track` (below) instead of `insert_track` — re-enqueued rows (failed last session, pre-v2 legacy without key) no longer die on the UNIQUE constraint; they proceed to library-hit → decode/analyze → persist grid+key → `update_track_meta(hash, Some(duration))` → `RowReady`. PCM still dropped at job end.

**Reorder/remove**: handler splices local rows (drag UX) and spawns `reorder(playlist_id, hashes)` / `remove_track(playlist_id, position)`; outcomes → `CommandFailed` on error (no more `let _ =`).

**Grid saves**: `flush_save_if_due` unchanged; the spawned save sends `GridSaved/Failed`.

**Editor load (click ready row)**: `spawn_load` now takes a `Sender<Event>`; stages/done become `LoadStage`/`LoadDone`.

### 4. Audio fixes

**Pan (zoom-dependent lag)**: add `drag_view_frame: Option<f32>` to app state. On `drag_started` (Scrub mode): `drag_view_frame = Some(current followed frame)`. Each dragged frame: `drag_view_frame -= drag_dx * seconds_per_pixel * sample_rate` (sign matches the audio's `-drag_dx` convention: waveform tracks the pointer; `drag_dx` is pointer px delta from `pointer_drag_delta`). The `follow` passed to `waveform::show` becomes `Some(drag_view_frame)` during drag — raw accumulation, **unclamped** (existing `clamp_pan` skip-while-dragging already permits this). Audio keeps receiving the smoothed, ±8-clamped scrub speed (vinyl feel unchanged). On `drag_stopped`: write `seek = Some(drag_view_frame)` and `position = drag_view_frame` so audio snaps to the pointer and following resumes without a jump.

**B3 (stopped seek)**: reorder the paused branch — consume the seek *before* writing position:
```rust
if !cmd.playing {
    out.fill(0.0);
    if let Some(frame) = cb_playhead.seek.write().take() {
        scrub = ScrubCore::new(channels, frame);
    }
    *cb_playhead.position.write() = scrub.position();
    *cb_playhead.speed.write() = 0.0;
    return;
}
```
Extract a testable helper (e.g. `paused_update(&Playhead, &mut ScrubCore, channels) -> f32`) so the ordering is unit-testable without a cpal stream.

### 5. Store changes (G1/G2 plumbing + rowids)

- `insert_track` → `Result<i64, …>` returning the new `playlist_tracks.rowid` (`INSERT … RETURNING rowid` or `last_insert_rowid` in the tx).
- New `ensure_track(&self, playlist_id, hash, path, title, artist, duration) -> Result<i64, …>`: tags upsert (as today) + `INSERT INTO playlist_tracks … ON CONFLICT(playlist_id, track_hash) DO NOTHING` + rowid via `RETURNING`, falling back to `SELECT rowid FROM playlist_tracks WHERE playlist_id = ? AND track_hash = ?` when the conflict suppressed the insert. Idempotent — used by the worker.
- New `contains_hash(&self, playlist_id, &TrackHash) -> Result<bool, …>` (add-path duplicate pre-check).
- New `track_duration(&self, &TrackHash) -> Result<Option<f64>, …>` (library-hit duration).
- `tracks_for` selects `pt.rowid AS id`; `PersistedTrack` gains `id: i64`.
- `RowId(u64)` → `RowId(i64)` throughout (`queue.rs`, rows, events).
- In-memory backend mirrors all of the above.
- G2: `tags_for` returns `Option<f32>` duration (probe `None` → insert `None`, never `Some(0.0)`); `library_hit` builds `TrackMeta` with `track_duration(hash)` so hit rows show their stored duration instead of `---`.

### 6. View changes (G3/G5 + loading states)

- Content column matches `Contents`: `None` → "select a playlist"; `Loading` → centered spinner; `Failed(msg)` → error text. Add button shows a spinner while `add_pending > 0`.
- G3 rename: right-click menu on a playlist gets an inline `TextEdit` (view-local editing state: id + text buffer); submit (non-empty) → spawn `rename_playlist` → `PlaylistRenamed` updates the list. Store round-trip test added (sqlite + in-memory).
- G5: while a row drag is active, paint a 1px insertion line at the boundary under the pointer; compute the index from pointer y over the painted row rects; extract the index math as a pure fn for testing.

## Anti-Goals (Out of Scope)

- No optimistic UI with rollback; no multi-writer shared state; no crossbeam/tokio-broadcast swap (std mpsc stays).
- No multi-playlist contents cache (`BTreeMap` of all rows) — contents fetch on selection (decision 2B).
- No changes to the single-worker FIFO analysis model, DSP, engine, CLI, or schema version.
- View-local **input** state (selection, drag gesture, zoom, cursor latch, rename text buffer, `add_pending` echo via `AddStarted` aside) is set directly by handlers — only *async-derived* state flows through `apply()`. Routing raw clicks through the bus would add latency to no correctness benefit.
- No rendering/mixing from the playlist (separate follow-up); no m3u export/import; no drag-from-file-manager.
- PCM never rides an `Event` beyond the existing boxed `LoadDone` payload; events carry `String` messages (Reports are not `Send`) as today.

## Edge Cases & Gotchas

1. **Stale loads**: `RowsLoaded`/`RowsLoadFailed` arriving for a no-longer-selected playlist must be dropped (guard on `playlist_id`), or a slow fetch overwrites the newly selected playlist's contents.
2. **Row-scoped events for other playlists**: dropped (contents refetched on switch). Playlist-scoped events always apply.
3. **Drain starvation**: if the 10ms budget breaks with events remaining, `drain` must `request_repaint()` immediately — otherwise leftovers wait for the next input-driven frame.
4. **Debounce reset**: `drain` clears the pending deadline at entry (a frame is running; new sends open a fresh window). Deadline state lives behind a small lock (`parking_lot::Mutex`) because `send` is called from worker threads.
5. **B3 ordering**: seek consumption must precede the paused-branch position write; extract and test the helper so a future refactor can't silently reorder it.
6. **Drag sign/units**: audio uses `-drag_dx * seconds_per_pixel`; the view accumulation must use the same sign and convert to frames (`* sample_rate`) or the waveform drifts against the cursor.
7. **Drag-end snap**: without `seek = Some(drag_view_frame)` on release, the view jumps back to the lagging audio position (the original bug resurfacing for one frame).
8. **`ensure_track` RETURNING**: SQLite's `INSERT … ON CONFLICT DO NOTHING RETURNING` yields *no row* on conflict — fall back to a `SELECT rowid`; don't assume `RETURNING` always fires.
9. **UNIQUE violation on add** still possible (same file picked twice in one dialog, hashes computed before either insert lands): map to `DuplicateSkipped`, not `CommandFailed`.
10. **`RowId` type change** (`u64`→`i64`) touches queue tests using literal ids; `remove_track` is position-based — derive the position from the row's *current* slot at spawn time (single UI-thread writer makes this safe).
11. **`mpsc::Receiver` is `!Sync`**: the bus receiver must live on the UI thread; only `Sender` clones cross into tasks.
12. **`apply()` may dispatch** (re-enqueue jobs on `RowsLoaded`) but must never block; sends to the unbounded worker channel are non-blocking.
13. **Editor top bar**: spinner + re-analyze enablement move from `loading.is_some()` to `EditorLoad` matching.

## Navigation Anchors

- `AutomixahUiApp::update` (`app.rs:533`) — drain+apply entry; panel wiring.
- `AutomixahUiApp::new` — bus construction, startup `PlaylistsLoaded` task.
- `spawn_load` (`track.rs:95`) — sender plumbed in.
- `worker_loop` (`playlist/queue.rs:111`) + `run_job` — `ensure_track`, bus events.
- `playlist::view::panel` (`playlist/view.rs:58`) — `Contents`/`add_pending` rendering, rename, insertion line.
- `insert_track` (`playlist/store/sqlite.rs:233`) — rowid return, `ensure_track`, `contains_hash`, `track_duration`.
- Output callback (`audio/output.rs:209-235`) — paused-branch fix.
- `follow` closure (`app.rs:611-630`) — drag-view override.

## Dependency Mappings

- External: none new. Uses std `mpsc`, existing `parking_lot`, `egui` (`Context::request_repaint_after`).
- Internal: `bus.rs` depends on `track::LoadStage`, `playlist::{PlaylistSummary, PlaylistRow}`, `playlist::queue::{RowId, TrackMeta}`, `track::LoadedTrack`, `audio::peaks::Peaks`. All existing types; no new schema migrations (v3 tables suffice — `rowid` is implicit).

## Test Strategies

House rules: BDD Given/When/Then comments, one behavior per test, `rstest` only for same-property inputs. New tests:

| # | Test | Where | Verifies |
|---|------|-------|----------|
| 1 | `coalesced_deadline_opens_one_window_per_burst` | bus.rs | N sends inside 50ms → single deadline; after expiry a new one opens |
| 2 | `drain_respects_time_budget_and_keeps_rest` | bus.rs | slow-apply flood → returns ≤ budget, receiver non-empty, repaint requested |
| 3 | `select_then_load_transitions_contents` | app/state | `Contents` None→Loading→Loaded via `apply(RowsLoaded)` |
| 4 | `rows_loaded_carries_db_rowids` | store + app | `tracks_for` rows carry `pt.rowid`; add-events use the same id space |
| 5 | `create_playlist_appends_exactly_once` | app/state | B1 regression: one event → one list entry |
| 6 | `add_track_leaves_other_rows_untouched` | app/state | B2 regression: `RowReady` mutates only its rowid's row |
| 7 | `stale_rows_loaded_is_dropped` | app/state | event for unselected playlist → contents unchanged |
| 8 | `duplicate_add_emits_skip_not_failure` | queue/store | G4: `contains_hash` → `DuplicateSkipped`, no Failed row |
| 9 | `rename_playlist_roundtrips` | store (sqlite + in-memory, rstest) | G3 |
| 10 | `reenqueued_incomplete_row_reaches_ready` | queue | G1 regression: persisted-without-grid row → `ensure_track` → Ready |
| 11 | `library_hit_reports_stored_duration` | queue/store | G2: hit `TrackMeta.duration_seconds` from `tracks` table; probe-None inserts NULL |
| 12 | `drag_view_advances_unclamped_beyond_scrub_max` | app state | 1000px at max zoom → frame delta = px × spp × rate (> 8×-speed distance) |
| 13 | `paused_seek_updates_position` | audio/output helper | B3: seek consumed in paused branch before position write |
| 14 | `rows_loaded_derives_reenqueue_jobs` | app/state | incomplete rows → jobs for exactly those rowids |
| 15 | `insertion_index_from_pointer_y` | playlist/view | G5 pure index math |

Update existing tests: queue tests (`RowId` literals i64; `insert_track` rowid return), track tests (drain via bus sender), delete `startup_hydration_restores_playlists_and_rows` (superseded by 3/4/14), delete `set_rows`-dependent view tests.

**Manual smoke** (documented in the final summary): create playlist (appears once, first click), switch playlists (spinner → rows), add duplicate file (skip message, no error row), add unanalyzed file (other rows untouched, new row queued → spinner → values), restart (rows restored, incomplete rows re-analyze to Ready), click-seek while stopped (playhead moves immediately), drag-pan zoomed fully out (waveform tracks cursor 1:1), rename playlist.

## Phases

1. **Audio fixes** — drag-view accumulation + follow override + drag-end seek snap; paused-branch reorder with extracted helper; tests 12/13.
2. **Event bus + loading enums** — `bus.rs` (`Event`, `EventBus`, debounce, drain); `Contents`/`EditorLoad`/`add_pending`; `apply()` replaces `poll_*`; tests 1/2.
3. **Spawn-site conversion, hydration deletion** — store rowid changes; contents fetch on selection; add-track task; CRUD/reorder/remove/load/saves via events; worker on bus; stale guards; re-enqueue derivation; delete `spawn_hydrate`/`HydrateEvent`/`hydrate_rows`/`set_rows`/`next_row_id`; tests 3-7, 14 + updated existing.
4. **Gap fixes** — G1 `ensure_track`, G2 duration plumbing, G3 rename UI + round-trip, G4 duplicate skip, G5 insertion line; tests 8-11, 15.
5. **Verify** — full suite (`just check`/`just test`/`just lint`), manual smoke, Record Updates.

## Acceptance Criteria

- Every async outcome arrives as an `Event`; `apply()` is the sole state mutator (audited); debounce ≤50ms; drain ≤10ms/frame with leftovers picked up next frame; no event dropped.
- Selecting a playlist clears contents → spinner → replaced by one event; switching back and forth never shows stale rows or re-analysis (B1/B2 dead by construction: no resync, no local id minting).
- Adding a track disturbs no other row; the new row reaches Ready via its own rowid.
- Every in-flight task shows a loading indicator (playlist contents, editor load, Add busy, per-row queued/spinner states).
- Click-seek while stopped paints immediately (B3); waveform tracks the cursor 1:1 at all zooms while dragging.
- G1–G5 fixed; `just check && just test && just lint` green.

## Record Updates (applied at end of implementation)

- `(ui) All async and threaded work reports back through a single UI event bus; frontend state is mutated only when applying events.`
- `(ui) UI repaints are scheduled by event-bus sends with a 50 ms debounce; each frame drains events under a 10 ms budget before rendering.`
- `(ui) Playlist contents load on selection: the view clears and shows a spinner until the load event replaces them; playlist rows carry database-minted ids.`
- Amend: `(ui) Track loading enters through the playlist: clicking a ready row loads the track into the grid editor; the Open button is removed.` *(ready-row wording)*
