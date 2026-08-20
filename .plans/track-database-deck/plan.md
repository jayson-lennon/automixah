# Track Database + Deck + Derived Playlist State

## Problem

Clicking re-analyze runs the editor pipeline (`spawn_load` in `track.rs`), which speaks editor-dialect events (`LoadStage`/`LoadDone`) addressed to no track identity, while playlist rows hold imperative per-row status (`PlaylistRow.status`) mutated only by queue-worker events (`RowAnalyzing`/`RowReady`/`RowFailed`) addressed by internal store rowids. Two event dialects, no linkage → deleting analysis data changes nothing the playlist renders.

Track knowledge is additionally smeared across the frontend (session `AnalysisCache`, copies in row fields, `LoadedTrack`), the editor has two parallel load pipelines (async `spawn_load` + production-dead sync `load`), hash/tag/extension helpers are triplicated across three files, the editor bypasses the injected analyzer, `add_pending` has a live spinner-never-clears bug (`RowAdded` never decrements), and `app.pcm`/`last_frame_time`/`QueueJob.playlist_id` are dead state.

## Solution

Invert to a single source of truth with derived projections:

- **`Tracks`** — one map, content hash → `TrackRecord { hash, tags, analysis }`, totally concrete (zero `Option` fields). Mutated only in the event-drain path. Absence of an analysis entry *is* "analysis gone."
- **Hash-addressed track events** (`TagsResolved`, `AnalysisStarted`, `AnalysisDone`, `AnalysisFailed`) spoken by all producers (queue worker, editor load pipeline, add-task). Playlist events reduce to ordering only; store rowids never address events and never reach the frontend.
- **Playlist contents = ordered hash list** (`Vec<TrackHash>`); all row display state (glyph/spinner/dim/red, BPM/key/duration, clickability) derives at render time from the record.
- **`Deck`** — the sole home of loaded-track media and playback state. `deck: Option<Deck>` is the app's one deck-lifecycle Option. Constructed atomically; dropped on load/re-analyze.
- **Conforming re-analyze**: take deck → clear the record's analysis (rows everywhere flip instantly) → reload in `Reanalyze` mode, which deletes the persisted grid *before* analysis and skips the store fast path.

---

## Dialectical Outcomes (Why)

- **Derivation over imperative pokes.** The root disease: rows cached display status and events poked it. The user's principle — "delete the analysis data and the playlist updates automatically because we are in immediate mode" — requires display state to be a pure function of shared state. Rows degrade to ordered identity references; rendering reads the track database.
- **Stable identity = content hash.** The hash is already the store key, the playlist reference, and the dedup key. Rowids are internal structure ("other tasks won't know about the row index"). Consequence: `RowId` disappears from the frontend entirely; the existing Record entry about database-minted row ids must be amended (see Record Updates).
- **Rejected: routing re-analyze through the analysis queue.** Zero new event wiring, but the track would be decoded/analyzed twice (worker for metadata + editor reload for PCM) — analysis is the CPU-heavy operation the single-worker queue exists to serialize.
- **Rejected: refetch playlist contents on completion.** Whole-panel flash (clear → spinner), a store-write→fetch ordering dependency, and no `Analyzing` row state during the work.
- **Rejected: stamping a `row_id` onto the loaded track.** Interim proposal; superseded by the hash-identity principle (one analysis event serves every playlist row referencing the hash).
- **Rejected: `Option<TrackMedia>` inside `TrackRecord`.** "A footgun waiting to explode" — an Option on a field every consumer must remember to check, plus PCM-per-track is a memory bomb (the queue drops PCM by design). The Option moves to exactly one place, `deck: Option<Deck>`, where it means "is a track loaded".
- **`add_pending: bool` → `adds_in_flight: usize`.** A count of genuinely outstanding async tasks (not derivable), incremented by `AddStarted.count`, decremented by *every* terminal outcome. Fixes the live bug where successful adds leave the spinner spinning forever. New `AddFailed` event so add-task failures don't get confused with `CommandFailed` from unrelated CRUD.
- **Sync `load()` deleted.** Production-dead duplicate of the async pipeline, kept alive only by its own tests. Its two behavioral tests (override-by-content-hash, rename survival) port to the async pipeline.
- **Editor pipeline routes through `services.analyzer`.** Today `track.rs::analyze()` constructs `StratumAnalyzer` directly, bypassing injection; the queue worker honors it. After this change no production path constructs an analyzer directly and the whole app is fake-analyzer testable.
- **Helper consolidation into `track::identity`.** SHA-256-hex, tag resolution (probe → filename fallback), and extension extraction are currently triplicated (`queue.rs`, `track.rs`, `app.rs`).
- **"Media loaded" event from earlier drafts: superseded.** Media lives in the Deck, built directly at `LoadDone` apply time; no event needed.
- **`Analysis.grid` is the effective (stored) grid.** Producers write what they just persisted (detected or store value); `GridSaved` refreshes the record so manual grid edits sync playlist BPM immediately (today they lag until playlist re-selection).

## Relevant Files (Where)

All paths relative to repo root `/mnt/zed/repos/automixah/re-analyze`:

| File | Action |
|---|---|
| `crates/automixah-ui/src/tracks.rs` | **New** — `TrackTags`, `Analysis`, `AnalysisState`, `TrackRecord`, `Tracks` + merge policy + unit tests |
| `crates/automixah-ui/src/deck.rs` | **New** — `Deck` struct (media, working grid, engine, scrub, view, gesture state) |
| `crates/automixah-ui/src/track.rs` | Rewrite — bus-based load pipeline, `LoadMode`, `LoadOutcome`, `identity` submodule; delete sync `load()`, `apply_stored_override`, `LoadedTrack`, `GridSource` |
| `crates/automixah-ui/src/analysis.rs` | **Delete** — `AnalysisCache` dissolves into `Tracks` |
| `crates/automixah-ui/src/bus.rs` | Rework `Event` enum + `Debug` impls |
| `crates/automixah-ui/src/playlist/mod.rs` | Rework `PlaylistState`/`Contents`/appliers; delete `RowStatus`, `PlaylistRow`, `RowId` usage |
| `crates/automixah-ui/src/playlist/queue.rs` | Rework `QueueJob` → `{hash, path}`; worker emits hash-addressed events; delete `TrackMeta`, `RowId`, `library_hit`'s tag reads |
| `crates/automixah-ui/src/playlist/view.rs` | Derivation-based rendering; hash-based `PanelAction`s |
| `crates/automixah-ui/src/app.rs` | App field rework (`tracks`, `deck`, `load_in_flight`), apply matrix, derivation, re-analyze; delete `poll_loading`, `pcm`, `last_frame_time` |
| `crates/automixah-ui/src/lib.rs` | Module declarations (`tracks`, `deck`; remove `analysis`) |
| `crates/automixah-ui/src/services.rs` | Unchanged (DI container already correct) |
| `crates/automixah-ui/src/main.rs` | Unchanged |
| `.agents/RECORD.md` | End of implementation only (Record Updates section) |

## Key Code Context (What)

### Current types being replaced

`PlaylistRow` and `RowStatus` (playlist/mod.rs) — the disease: display facts cached per row, poked by row-addressed events:

```rust
pub enum RowStatus { Queued, Analyzing, Ready, Failed(String) }

pub struct PlaylistRow {
    pub row_id: RowId,          // playlist_tracks.rowid
    pub position: i64,
    pub path: PathBuf,
    pub hash: Option<TrackHash>,
    pub title: String, pub artist: String,
    pub bpm: Option<f32>, pub key: Option<Key>, pub duration: Option<f32>,
    pub status: RowStatus,
}
```

Current appliers (`set_status`, `apply_ready`) and the view's glyph match in `paint_row_content`:

```rust
let (glyph, color) = match &row.status {
    RowStatus::Ready => (" ", strong_color),
    RowStatus::Queued => ("🕓", weak_color),
    RowStatus::Analyzing => ("⭕", weak_color),
    RowStatus::Failed(_) => ("!", Color32::RED),
};
```

Current event dialect (bus.rs) — row-addressed analysis events to be replaced:

```rust
RowAnalyzing { row_id: RowId },
RowReady { row_id: RowId, meta: TrackMeta },
RowFailed { row_id: RowId, message: String },
```

Current app fields to be absorbed/deleted (app.rs):

```rust
track: Option<crate::track::LoadedTrack>,   // → Deck
peaks: Option<Peaks>,                        // → Deck
view: WaveformView,                          // → Deck
edit_grid: EditableGrid,                     // → Deck
scrub: ScrubMachine,                         // → Deck
engine: Option<OutputEngine>,                // → Deck (stays Option inside)
drag_mode: DragMode, drag_last_x, drag_view_frame, cursor_time,
position_updated, position_at_update,        // → Deck
analysis: AnalysisCache,                     // → Tracks
pcm: Option<Arc<Vec<f32>>>,                  // dead — delete
last_frame_time: Option<Instant>,            // dead — delete
loading: Option<Receiver<LoadEvent>>,        // → load_in_flight: bool (bus-only reporting)
```

Current `reanalyze_current` (app.rs) — the fire-and-forget delete race:

```rust
fn reanalyze_current(&mut self) {
    let Some(track) = self.track.take() else { return };
    self.analysis.invalidate(&track.hash);
    let hash = track.hash.clone();
    let store = self.services.grid_store.clone();
    self.services.runtime.handle().spawn(async move {
        let _ = store.delete(&hash).await;      // fire-and-forget: races the reload's store read
    });
    self.peaks = None; self.engine = None;
    self.loading = Some(crate::track::spawn_load(&self.services, track.path.clone(), None));
}
```

Current enqueue derivation in `drain_bus` — keyed on row status:

```rust
Event::RowsLoaded { playlist_id, rows } => {
    for row in rows {
        if row.status == RowStatus::Queued && let Some(hash) = row.hash.clone() { /* enqueue */ }
    }
}
```

Services (services.rs) — the injected analyzer the editor pipeline currently bypasses:

```rust
pub struct Services {
    pub paths: AppPaths,
    pub grid_store: GridStoreService,
    pub playlist_store: PlaylistStoreService,
    pub analyzer: std::sync::Arc<dyn djcore::analyzer::AudioAnalyzer>,  // must be used by ALL analysis
    pub runtime: std::sync::Arc<tokio::runtime::Runtime>,
}
```

Store traits consumed (no changes to them): `GridStore::{get, put, delete}`, `PlaylistStore::{tracks_for, ensure_track, contains_hash, update_track_meta, track_duration, reorder(playlist_id, &[TrackHash]), remove_track(playlist_id, position), …}`. Note `reorder` already takes hashes; `remove_track` is position-addressed (the local splice provides the index).

### Target types

```rust
// tracks.rs
pub struct TrackTags {           // store-minted facts
    pub title: String,
    pub artist: String,
    pub path: PathBuf,           // add-time source path
}

pub struct Analysis {            // the one analysis package
    pub grid: BeatGrid,          // effective (stored) grid
    pub bpm: f32,
    pub key: Key,
    pub duration_seconds: f32,
}

pub enum AnalysisState {
    Queued,                      // job enqueued (inserted at enqueue time)
    Analyzing,
    Ready(Analysis),
    Failed(String),
}

pub struct TrackRecord {         // concrete — zero Option fields
    pub hash: TrackHash,
    pub tags: TrackTags,
    pub analysis: AnalysisState,
}

pub struct Tracks { by_hash: HashMap<TrackHash, TrackRecord> }
```

```rust
// deck.rs
pub struct Deck {
    pub hash: TrackHash,
    pub path: PathBuf,                    // re-load source for re-analyze
    pub pcm: std::sync::Arc<Vec<f32>>,
    pub peaks: Peaks,
    pub edit_grid: EditableGrid,          // working copy; saves go to the store
    pub pending_save: Option<EditableGrid>,
    pub engine: Option<OutputEngine>,     // None = cpal unavailable; audio methods no-op
    pub scrub: ScrubMachine,
    pub view: WaveformView,
    pub drag_mode: DragMode,              // gesture state moves in here
    pub drag_last_x: Option<f32>,
    pub drag_view_frame: Option<f32>,
    pub cursor_time: Option<f32>,
    pub position_updated: Option<Instant>,
    pub position_at_update: f64,
}
```

App shell after:

```rust
pub struct AutomixahUiApp {
    services: Services,
    bus: EventBus,
    playlist_queue: AnalysisQueue,
    tracks: Tracks,                        // THE track database
    playlist_state: PlaylistState,         // ordering only
    deck: Option<Deck>,                    // the one lifecycle Option
    load_in_flight: bool,
    status: String,
}
```

Playlist state after:

```rust
pub struct PlaylistState {
    pub playlists: Vec<PlaylistSummary>,
    pub selected: Option<i64>,
    pub contents: Contents,                // Loaded(Vec<TrackHash>)
    pub adds_in_flight: usize,
    pub rename: RenameEditor,
}
```

## Implementation Algorithm (How)

### Event dialect (final)

Track events (mutate records):
- `TagsResolved { hash, tags }` — add-task resolved tags for a hash.
- `AnalysisStarted { hash }` — analysis actually running (not sent on store fast path).
- `AnalysisDone { hash, analysis }` — analysis known (freshly detected or store fast path).
- `AnalysisFailed { hash, message }`.

Playlist events (mutate ordering only): `PlaylistsLoaded`, `PlaylistCreated`, `PlaylistRenamed`, `PlaylistDeleted`, `RowsLoaded { playlist_id, hashes, records }`, `RowsLoadFailed`, `RowAdded { playlist_id, hash }`, `RowRemoved { playlist_id, hash }`, `RowsReordered { playlist_id, hashes }`, `DuplicateSkipped`, `AddStarted { count }`, `AddFailed { message }` (add-task terminal error — distinct from `CommandFailed` so the in-flight count is never corrupted by unrelated CRUD failures), `CommandFailed`.

Editor events: `LoadStage(LoadStage)`, `LoadDone(Box<Result<LoadOutcome, String>>)` where `LoadOutcome { hash, path, analysis: Analysis, audio: DecodeAudio, peaks: Peaks }`, `GridSaved { hash, grid: EditableGrid }` (carries the grid so the record refreshes), `GridSaveFailed(String)`.

### Apply matrix (the complete mutation surface)

| Event | Mutation |
|---|---|
| `LoadStage(s)` | `status` text (load_in_flight stays true) |
| `LoadDone(Ok(o))` | `tracks.set_analysis(hash, Ready(o.analysis))`; `deck = Some(Deck::new(o…))`; `load_in_flight = false`; status |
| `LoadDone(Err(m))` | `load_in_flight = false`; `status = "⚠ load failed: m"` |
| `AnalysisStarted{hash}` | analysis → `Analyzing` |
| `AnalysisDone{hash,a}` | analysis → `Ready(a)` |
| `AnalysisFailed{hash,m}` | analysis → `Failed(m)` |
| `TagsResolved{hash,t}` | upsert tags |
| `GridSaved{hash,grid}` | `tracks.refresh_grid(hash, grid)`; status |
| `GridSaveFailed(m)` | status |
| `RowsLoaded{pid,hashes,records}` | if selected: upsert records (merge policy below), clear `Failed` for store-incomplete hashes (retry semantics), `contents = Loaded(hashes)` |
| `RowAdded{pid,hash}` | push hash; `adds_in_flight -= 1` |
| `RowRemoved{pid,hash}` | `contents.retain(≠ hash)` (idempotent) |
| `RowsReordered{pid,hashes}` | reorder contents to match |
| `AddStarted{count}` | `adds_in_flight += count` |
| `DuplicateSkipped` | `adds_in_flight -= 1` |
| `AddFailed{m}` | `adds_in_flight -= 1`; status ⚠ |
| `CommandFailed(m)` | status ⚠ (never touches the count) |
| playlist list events | unchanged from today |

### Record merge policy (`Tracks::upsert`)

```
entry = map.entry(hash).or_insert(TrackRecord { hash, tags: incoming.tags, analysis: incoming.analysis })
entry.tags = incoming.tags                                  // store/add-task truth wins for tags
if entry.analysis is absent-state { entry.analysis = incoming.analysis }  // in-session/in-flight truth wins
```
Plus the contents-load retry rule: for each hash in the loaded playlist whose hydrated record had *no* analysis and whose current entry is `Failed` → clear to absent (preserves today's retry-on-reselect for incomplete rows).

### Enqueue derivation (drain_bus, after applying events)

For every hash arriving via `RowsLoaded.hashes` or `RowAdded.hash`: if `tracks` has **no analysis entry** for it → insert `Queued` and enqueue `QueueJob { hash, path: record.tags.path }`. This is the dedup: `Ready`/`Queued`/`Analyzing`/`Failed` entries all suppress a job. (Map writes happen in exactly two places: this derivation inserts `Queued`; `apply` handles lifecycle events.)

### Queue worker (`run_job` rewrite)

1. Store fast path (grid store row with key AND playlist-store duration known): emit `AnalysisDone { hash, analysis from store }` — no `AnalysisStarted`, no decode, no file read. This serves cross-session adds of already-analyzed hashes.
2. Otherwise: emit `AnalysisStarted { hash }` → read file → decode → `services.analyzer.analyze(mono, rate)` → persist `GridOverride` (grid + key) + `update_track_meta(duration)` → emit `AnalysisDone`. PCM dropped at scope end (unchanged).
3. Any failure → `AnalysisFailed { hash, message }`.

### Editor load pipeline (`track.rs` rewrite)

One function, one task, bus-only reporting (the mpsc channel and `poll_loading` disappear; this matches every other async task in the app):

```
spawn_load(services, tx: Sender<Event>, path, mode: LoadMode) -> spawns task
  LoadMode::Normal:    store fast path allowed (grid_store.get; CacheHit stage; AnalysisDone from store row incl. key)
  LoadMode::Reanalyze: await grid_store.delete(hash) FIRST (ordering fixes the race), then always analyze fresh
  Stages via Event::LoadStage exactly as today (Hashing → Decoding → Analyzing | CacheHit)
  Analysis via services.analyzer (injection honored)
  Success: persist auto grid when freshly analyzed (as today), build peaks, send AnalysisDone + LoadDone(Ok(LoadOutcome))
  Failure: send AnalysisFailed + LoadDone(Err(msg))
```

`load_in_flight` is set `true` at spawn and `false` on `LoadDone` apply (disables the re-analyze button, shows the spinner, drives repaint).

### Re-analyze flow (the conforming version of the user's three steps)

```
1. deck = self.deck.take()                     // old PCM/grid/engine gone immediately; waveform placeholder shows
2. tracks.clear_analysis(hash)                 // rows in EVERY playlist derive "needs analysis" next frame — no event
3. load_in_flight = true; spawn_load(..., path, LoadMode::Reanalyze)
   // delete lands before analysis inside the task; AnalysisStarted → rows derive Analyzing; AnalysisDone → Ready
```

### Deck construction (atomic, at `LoadDone` apply)

`Deck::new(hash, path, audio, peaks, edit_grid)` builds pcm, `OutputEngine::start(...)` (→ `Option`, audio-fail = status note + `None`, grid editing still works), fresh `ScrubMachine::new(1.0)`, default `WaveformView`, cleared gesture fields. No half-built deck states exist.

### View derivation

`panel(ctx, &mut PlaylistState, &Tracks) -> PanelActions` — one hop, both inputs. Per visible row: `tracks.get(hash)` → paint glyph/metadata/interactivity from `AnalysisState` (match arms identical to today's, input changed; `Queued` and absent render identically — clock glyph — distinguishing them is a paint-time choice). BPM/key/duration columns read `Ready(a)` fields, `--`/`---` placeholders otherwise. Clickable/draggable iff `Ready`. `prev_key` harmonic walk reads records. Repaint driver `any_pending` scans `contents` hashes against the map. `PanelAction`s become hash-based: `LoadRow(TrackHash)`, `MoveRow { from, to }` (hashes), `RemoveRow { hash }`.

### Add-task flow

hash (identity) → tags (identity: probe → filename fallback) → `AddStarted` already sent by the dialog → dup check (`contains_hash`) → `ensure_track` → send `TagsResolved { hash, tags }` then `RowAdded { playlist_id, hash }`. Errors → `AddFailed`.

### Contents load (hydration)

Fetch `tracks_for` → join grids from grid store (as today) → build `TrackRecord`s (complete = grid+key+duration → `Ready(Analysis)`; else absent analysis) → send `RowsLoaded { playlist_id, hashes, records }`.

## Phases

Phases 2 and 3 are one compile-green checkpoint (the Event enum, producers, appliers, and view must land together); implement as a unit, review separately. Every phase ends with `just check` + `just test` + `just lint` green.

1. **Track database** — Create `tracks.rs` with `TrackTags`/`Analysis`/`AnalysisState`/`TrackRecord`/`Tracks`, the merge policy, and unit tests. Add `tracks: Tracks` to the app (populated by hydration, not yet consumed). Dissolve `analysis.rs` (`AnalysisCache`) into it.
2. **Event dialect** — Rework `Event` (hash-addressed track events, `AddFailed`, `GridSaved` carrying the grid, `RowsLoaded` carrying records); rewrite the queue worker (`QueueJob { hash, path }`, fast path → `AnalysisDone`, delete `TrackMeta`/`RowId`/`playlist_id`); rewrite the editor pipeline (bus-only, `LoadMode`, `services.analyzer`, `LoadOutcome`); delete sync `load()` and port its two behavioral tests; replace `poll_loading`/mpsc with `load_in_flight`.
3. **Playlist projection** — `Contents::Loaded(Vec<TrackHash>)`; appliers ordering-only; move/remove/reorder by hash; `adds_in_flight` counter with correct decrement-on-every-terminal; view derives all display from `Tracks`; hash-based `PanelAction`s; enqueue derivation with hash dedup; `rows_from_persisted` becomes record hydration.
4. **Deck extraction** — Create `deck.rs`; absorb media, `edit_grid`, `pending_save`, `engine`, `scrub`, `view`, and gesture/extrapolation fields; `deck: Option<Deck>` on the app; atomic `Deck::new`; rework `update()` (waveform under one `if let Some(deck)`, Space-key and command pushes guarded, save flush from the deck).
5. **Re-analyze + cleanup** — Conforming re-analyze (deck take → clear analysis → `LoadMode::Reanalyze`); consolidate `track::identity` (hash/tags/extension) and switch all call sites; delete dead state (`app.pcm`, `last_frame_time`).
6. **Tests & record** — Port/update every affected test (list under Test Strategies); full `just check`/`just test`/`just lint`/`just fmt`; Record Updates at end of implementation.

## Acceptance Criteria

- All track facts live in `Tracks`; playlist contents are ordered hash lists with no per-row data fields; media/playback state exists only inside `Deck`; the sole lifecycle Option is `deck: Option<Deck>`.
- Re-analyze: rows referencing the hash (in any playlist) derive queued → analyzing → ready with fresh BPM/key/duration; old deck dropped immediately; no playlist re-selection.
- Persisted-grid deletion is ordered before reload; analysis always actually re-runs (no stale fast-path).
- Adding a track whose hash is already `Ready` enqueues no job.
- PCM exists only inside the loaded deck; loading/re-analyzing drops the prior deck entirely.
- Add-spinner clears after a fully successful add batch (no terminal outcome left un-decremented).
- No production path constructs `StratumAnalyzer` directly; no duplicated hash/tag/extension helpers outside `track::identity`.
- No sync duplicate of the load pipeline.

## Test Cases

| # | Given | When | Then |
|---|---|---|---|
| 1 | Ready rows in two playlists sharing a hash | Re-analyze clicked | Both rows derive queued → analyzing → ready |
| 2 | Analysis entry dropped from a record | Any row rendered | Derives "needs analysis" with no row-addressed event |
| 3 | Contents load with store-known tracks | Load event applies | Records hydrated `Ready`; incomplete hashes enqueued |
| 4 | Hash already `Ready` | Track added to another playlist | No job enqueued; row derives ready immediately |
| 5 | Persisted grid exists | Re-analyze runs | Delete lands before lookup; `AnalysisStarted` observed, no fast-path |
| 6 | Load completes | Apply runs | Record holds `Ready(Analysis)`; `deck` is `Some` with media+grid+engine |
| 7 | Bad file during re-analyze | Failure event | Row derives `Failed`; editor surfaces the error |
| 8 | Deck loaded, then another track loaded / re-analyzed | Apply runs | Old deck gone — no stale PCM, grid, or engine survives |
| 8b | No deck | Render | Waveform placeholder; no interaction state leaks |
| 9 | A batch of adds, all succeeding | Last `RowAdded` applies | `adds_in_flight` reaches 0; spinner cleared |
| 10 | Editor pipeline with `FakeAnalyzer` injected | Load runs | Fake analyzer is called (injection honored) |
| 11 | Manual override stored for a hash (ported sync-test) | Async load runs | Override wins by content hash across rename |
| 12 | Legacy incomplete row | Contents load | Derives queued and re-enqueues (existing behavior preserved) |

## Edge Cases & Gotchas

1. **The `add_pending` bug**: `CommandFailed` is also emitted by playlist CRUD — it must never decrement `adds_in_flight`. That is why add-task failures get their own `AddFailed` event.
2. **Delete/reload race**: the delete and the fresh analysis must happen inside one task (`LoadMode::Reanalyze`) — ordering by construction, not by timing.
3. **Bus drain budget**: `LoadDone` apply builds a Deck including a cpal `OutputEngine::start` — may exceed the 10ms drain budget. Acceptable: leftovers drain next frame; the bus never drops events.
4. **Duplicate hashes**: `contains_hash` dedups within a playlist; the same hash across playlists is legal and is exactly what hash-addressed events serve with one analysis.
5. **Hydration merge**: in-session/in-flight analysis always wins over store reads; tags always take the incoming (store/add-task) value; store-incomplete + current `Failed` → cleared (retry on reselect, preserving today's behavior).
6. **Fast-path `AnalysisDone`**: store path has no analyzer run — key and grid come from the stored `GridOverride` (it carries both); duration comes from the playlist store. No `AnalysisStarted` is sent (mirrors today's "no analyzing stage on library hit").
7. **Duration display**: `AnalysisDone` from the editor pipeline carries duration from decoded audio, so rows show duration even when the add-time container probe failed (free fix; no store write needed for display).
8. **`remove_track` is position-addressed** in the store: keep the local-splice-first pattern — the splice index *is* the position passed to the store.
9. **Gesture state reset**: moving drag/extrapolation fields into `Deck` resets them on every deck swap — a mid-drag load ends the drag cleanly (strictly better than today).
10. **Pipeline tests**: the mpsc-channel tests die with the channel — port them to the `EventBus::without_repaint` + `receiver_for_test` pattern already used by queue tests.
11. **`FakeAnalyzer` injection** is assertable via `call_count()` (existing djcore test helper) for both the worker and the editor pipeline.
12. **Session fast path**: the pipeline consults the grid store (single point read); the map no longer feeds the pipeline an input — one code path, and the map stays the display source of truth.
13. **Clippy**: warnings are errors (`just lint`); f32/f64 casts carry `#[expect(clippy::cast_possible_truncation, reason = …)]` per house style; BDD comment structure and standalone-sentence test names per AGENTS.md §4.
14. **Store grid refresh on manual save**: `GridSaved` refreshes the record's grid so the playlist BPM matches manual edits immediately — an intentional small improvement over today's lag-until-reselect.

## Anti-Goals (Out of Scope)

- No persistence schema changes; no new store-trait methods; no migrations.
- No changes outside `automixah-ui` (djcore/engine/schema/CLI untouched beyond consumption).
- No variable-tempo support; no seeking (permanently out of scope per Record).
- No multi-deck; no PCM retention for playlist tracks.
- No `self.status` line restructuring (transient message surface; churn without payoff).
- No `Services`/`main.rs` startup restructuring.
- No automatic retry of `Failed` analysis beyond the existing reselect semantics.
- No visual redesign of rows (same glyphs/layout; only the source of the data changes).

## Navigation Anchors

- `AutomixahUiApp::update` (app.rs) — frame flow: `drain_bus` → panel → top bar → side panel → central panel.
- `AutomixahUiApp::apply` + `drain_bus` (app.rs) — the single mutation path and the enqueue derivation.
- `PlaylistState::apply` (playlist/mod.rs) — ordering appliers.
- `run_job` / `worker_loop` (playlist/queue.rs) — the analysis worker.
- `spawn_load` (track.rs) — the editor pipeline (rewritten).
- `reanalyze_current` → `reanalyze` (app.rs) — the conforming flow.
- `spawn_contents_load` / `join_grids` / `rows_from_persisted` (app.rs) — hydration (reworked).
- `rows` / `row_ui` / `paint_row_content` (playlist/view.rs) — derivation-based rendering.
- `panel` (playlist/view.rs) — signature gains `&Tracks`.

## Dependency Mappings

No new external dependencies. Existing: `egui`/`eframe`, `tokio` (Arc<Runtime>), `parking_lot`, `sha2`, `rfd`, `error_stack`/`wherror`, `djcore` (analyzer/decoder/key), `automixah-engine` (`timeline::types::TrackHash`). New internal modules: `tracks`, `deck`, `track::identity`; removed: `analysis`.

## Test Strategies

- **tracks.rs (new unit tests)**: merge policy (in-session analysis wins, tags overwritten), absent → derivation, `refresh_grid`, clear-analysis. Ports of the three `AnalysisCache` tests.
- **queue.rs (update)**: switch all six existing tests to bus-receiver draining and hash assertions; fast-path test asserts no `AnalysisStarted` + analyzer `call_count() == 0`; incomplete-row retry test preserved; drop `playlist_id` from fixtures.
- **playlist/mod.rs (update)**: appliers on hash lists (replaces rowid tests); reorder/remove by hash; `adds_in_flight` reaches 0 after all-successful batch and after mixed success/failure; `AddFailed` decrements.
- **track.rs (update + port)**: stages-in-order via bus; store fast path skips analysis; `LoadMode::Reanalyze` deletes-then-analyzes (no `CacheHit`, `AnalysisStarted` observed, analyzer actually called — FakeAnalyzer); missing-file failure; port `load_prefers_manual_override_by_content_hash` semantics onto the async pipeline (rename survival); `hash_file_is_stable_hex` moves to `identity`.
- **app-level (test hooks / `__test-hooks`)**: re-analyze flow end-to-end with fake services (deck taken, record cleared, events observed); hydration test replaces `rows_from_persisted_for_test`; deck swap drops prior media; save-path hooks retooled for `Deck`.
- **view.rs (update)**: `analysis_view`-style derivation purity tests (glyph/interactivity mapping); existing color/insertion tests unchanged.
- **Full gate**: `just check`, `just test`, `just lint`, `just fmt` — all green before Record Updates.

## Record Updates

Applied at end of implementation, only if the implementation matches (divergences surface to the user instead):

- Add: `(ui) Frontend track knowledge lives in a single track database (content hash → tags, analysis state); playlist rows are ordered hash references and all row display state derives from the database at render time.`
- Add: `(ui) Analysis lifecycle events address tracks by content hash; row ids are internal to playlist ordering and never address events.`
- Add: `(ui) The editor holds a single optional Deck (media, working grid, engine, scrub, view) for the loaded track; decoded PCM/peaks exist only inside the deck and are dropped on load or re-analyze.`
- Amend: `(ui) Playlist analysis runs on a single-worker FIFO queue that decodes, analyzes, persists, and drops PCM; jobs are deduplicated by content hash.`
- Amend: `(ui) Track loading runs off the UI thread (hash → decode → analyze) with progress stages surfaced in the UI; the re-analyze button drops the record's analysis, deletes the stored grid before reloading, and playlist rows reflect the re-analysis automatically.`
- Amend (spec-refined during specification: rowids no longer reach the frontend at all): `(ui) Playlist contents load on selection: the view clears and shows a spinner until the load event replaces them; playlist rows carry database-minted ids.` → `(ui) Playlist contents load on selection: the view clears and shows a spinner until the load event replaces them; contents are ordered content-hash lists and store rowids never reach the frontend.`
