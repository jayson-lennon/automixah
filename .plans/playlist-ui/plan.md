# Playlist UI for automixah-ui — Implementation Specification

## Problem

automixah-ui loads one track at a time via an `Open…` button; there is no way to build an ordered track list. Key/duration analysis results are discarded after loading (only the grid persists), and container tags are never read — so there is nothing to show per-track anyway.

## Solution

A playlist section as a **bottom panel** spanning the window width: left column lists playlists, right column shows the selected playlist's tracks (`Artist – Title · BPM · Camelot key · duration`), key colored by harmonic distance to the **previous row** (blue→red gradient ported from harmonic-playlist). Adding tracks (multi-select file dialog) hashes + tag-probes each file; a stored grid+key hydrates the row instantly, otherwise the track enters a **single-worker FIFO analysis queue** (queued icon → spinner → ready) that decodes, analyzes, persists grid+key/tags, and drops PCM. Clicking a Ready row loads it into the existing grid editor via the existing pipeline; dragging reorders; right-click removes. The `Open…` button is deleted.

Rendering/mixing from the UI playlist is explicitly out of scope for this task (follow-up).

## Dialectical Outcomes (Why)

Decisions from the Socratic dialogue, with rejected alternatives:

1. **Row click = select + load into editor (A).** The playlist replaces `Open…` as the only track entry point. *Rejected:* load-as-separate-action (extra step, no benefit). *Confirmed:* rendering+mixing from the UI is out of scope until the playlist works.
2. **Key persists as new columns on `beat_grids` via migration v2 (A).** One row per content hash; key is deterministic from content. *Rejected:* separate `track_meta`-style key store (two stores to keep consistent per hash); not persisting key (defeats "attempt to load metadata" across sessions).
3. **Color coding = harmonic distance to the previous row (A).** DJ-adjacency view; drag-reorder instantly recolors, which fits drag-reordering. *Rejected:* single fixed reference key; static per-key colors (no relational meaning).
4. **Playlists persist to SQLite; two-column layout below all existing UI (B).** Left column = all playlists, right column = selected playlist's content. *Rejected:* session-only playlist (user chose persistence); m3u export only.
5. **Analysis queue = single-worker FIFO (A).** Analysis is very CPU-heavy; one job at a time keeps the host responsive. Queued-icon → spinner → ready naturally reads as queue position. *Rejected:* bounded parallelism (jank, complexity).
6. **Tags via symphonia probe, filename fallback, persisted in DB (A).** Matches harmonic-playlist rows. *Rejected:* filename-only v1.
7. **Remove tracks via right-click context menu (A).** *Rejected:* always-visible `×` button (row width); no removal at all.
8. **Playlist analysis is metadata-only: decode → analyze → persist → drop PCM (A).** PCM loads only when a row is selected into the editor (existing pipeline, cache-hit). *Rejected:* caching PCM for all rows (memory explodes on real libraries).

Migrations: automixah **already has** a jinn-style migration runner in `crates/automixah-schema/src/lib.rs` (`_migrations` tracking table, `BEGIN IMMEDIATE` transaction, v1 = `create_beat_grids`; explicitly modeled on jinn's session-schema crate). "Use the jinn pattern" therefore means: bump `LATEST_VERSION`, extend `apply_migration_chain`, add `migrate_v2`/`migrate_v3` functions in the same file — no new machinery.

Baked-in minor decisions (approved with the plan):
- Duplicate content-hash within one playlist is rejected at add time; the same track in *different* playlists is allowed.
- Playlist management: "＋ new" button; right-click rename/delete on the playlist list.
- Startup hydration re-runs lookups for every playlist row; rows missing grid or key are re-enqueued.
- `open_pick` in `track.rs` is deleted along with the button.

## Relevant Files (Where)

**Modified:**
- `crates/automixah-schema/src/lib.rs` — migrations v2 + v3, `LATEST_VERSION = 3`, new schema tests.
- `crates/automixah-ui/src/store/mod.rs` — `GridOverride` gains `key: Option<Key>`.
- `crates/automixah-ui/src/store/sqlite.rs` — read/write key columns; COALESCE upsert.
- `crates/automixah-ui/src/store/in_memory.rs` — mirror the key field.
- `crates/automixah-ui/src/services.rs` — `playlist_store` + `analyzer` fields.
- `crates/automixah-ui/src/track.rs` — `spawn_load` persists key; delete `open_pick`.
- `crates/automixah-ui/src/app.rs` — playlist panel wiring, poll queue/store events, remove `Open…`.
- `crates/automixah-ui/src/main.rs` — assemble new services.
- `crates/djcore/src/decoder/mod.rs` — re-export `TrackTags`/probe from `meta.rs`.
- `AGENTS.md` — extend the data-flow diagram with one line (`playlist library`).

**New:**
- `crates/djcore/src/decoder/meta.rs` — tag probe (`TrackTags`, `probe_metadata`).
- `crates/automixah-ui/src/playlist/mod.rs` — feature module: row model, states, hydration.
- `crates/automixah-ui/src/playlist/queue.rs` — `AnalysisQueue` worker.
- `crates/automixah-ui/src/playlist/store/mod.rs` — `PlaylistStore` trait + `PlaylistStoreService`.
- `crates/automixah-ui/src/playlist/store/sqlite.rs` — SQLite backend.
- `crates/automixah-ui/src/playlist/store/in_memory.rs` — in-memory backend (tests).
- `crates/automixah-ui/src/playlist/view.rs` — egui panel rendering, row layout, color gradient.

## Key Code Context (What)

### `djcore::key::Key` (crates/djcore/src/key.rs) — exists, use as-is

```rust
pub struct Key {
    /// Root note index: 0=C, 1=C#/Db, ..., 11=B.
    pub root: u8,
    /// Whether the key is major or minor.
    pub mode: KeyMode,
}
pub enum KeyMode { Major, Minor }

impl Key {
    pub fn format_with(&self, format: KeyFormat) -> String;   // KeyFormat::Camelot → "8A"
    pub fn harmonic_distance(&self, other: &Key) -> f32;      // 0.0..=1.0, Camelot-wheel based
}
```

### `djcore::analyzer` (crates/djcore/src/analyzer.rs) — `AnalyzerOutput` fields we now consume

```rust
pub struct AnalyzerOutput {
    pub bpm: f32,
    pub key: Key,
    pub duration_seconds: f32,
    pub beat_grid: BeatGrid,
    pub bpm_confidence: f32,
    pub key_confidence: f32,
    pub grid_stability: f32,
}
pub trait AudioAnalyzer: Send + Sync {
    fn name(&self) -> &'static str;
    fn analyze(&self, samples: &[f32], sample_rate: u32) -> Result<AnalyzerOutput, Report<AnalyzerError>>;
}
/// Test double — already exists:
pub struct FakeAnalyzer { /* with_output(AnalyzerOutput), call_count() */ }
```

### `GridOverride` (crates/automixah-ui/src/store/mod.rs) — extend

```rust
pub struct GridOverride {
    pub grid_bpm: f32,
    pub anchor_seconds: f32,
    pub downbeat_phase: u8,
    pub updated_at: i64,
    // NEW:
    pub key: Option<Key>,   // None = unknown / "leave unchanged" on upsert
}
```

Current SQLite upsert (pattern to keep, adding COALESCE):

```rust
"INSERT INTO beat_grids (track_hash, grid_bpm, anchor_seconds, downbeat_phase, updated_at) \
 VALUES (?, ?, ?, ?, ?) \
 ON CONFLICT(track_hash) DO UPDATE SET \
 grid_bpm = excluded.grid_bpm, \
 anchor_seconds = excluded.anchor_seconds, \
 downbeat_phase = excluded.downbeat_phase, \
 updated_at = excluded.updated_at"
```

New upsert adds `key_root`/`key_mode` columns with `key_root = COALESCE(excluded.key_root, beat_grids.key_root)` (same for `key_mode`) so a manual grid edit (which passes `key: None`) never clobbles a stored key.

### Migration runner (crates/automixah-schema/src/lib.rs) — extend in place

```rust
const LATEST_VERSION: i32 = 1;               // → 3

fn apply_migration_chain(conn: &mut rusqlite::Connection, current: i32)
    -> Result<(), Report<SchemaMigrationError>>
{
    if current < 1 { migrate_v1(conn)?; record_version(conn, 1, "create_beat_grids")?; }
    // NEW:
    // if current < 2 { migrate_v2(conn)?; record_version(conn, 2, "add_key_columns")?; }
    // if current < 3 { migrate_v3(conn)?; record_version(conn, 3, "create_playlist_tables")?; }
    Ok(())
}
```

v1 table being extended:

```sql
CREATE TABLE beat_grids (
  track_hash TEXT PRIMARY KEY,
  grid_bpm REAL NOT NULL,
  anchor_seconds REAL NOT NULL,
  downbeat_phase INTEGER NOT NULL CHECK (downbeat_phase BETWEEN 0 AND 3),
  updated_at INTEGER NOT NULL
)
```

### `Services` (crates/automixah-ui/src/services.rs) — extend

```rust
#[derive(Clone)]
pub struct Services {
    pub paths: AppPaths,
    pub grid_store: GridStoreService,
    pub analysis: crate::analysis::AnalysisCache,
    pub runtime: std::sync::Arc<tokio::runtime::Runtime>,   // Arc, NOT Handle (recorded bug)
    // NEW:
    pub playlist_store: crate::playlist::store::PlaylistStoreService,
    pub analyzer: std::sync::Arc<dyn djcore::analyzer::AudioAnalyzer>,
}
```

The queue itself is **app-owned session state** (like `engine`/`loading`), not a `Services` field — it needs the egui-facing event channel.

### `spawn_load` (crates/automixah-ui/src/track.rs) — key persistence point

The `None => { send_stage(Analyzing); ... }` branch already persists the auto grid via `grid_store.put(...)`; extend the constructed `GridOverride` to carry `key: Some(out.key)`. Keep the rest of the pipeline untouched. Delete `open_pick` entirely.

### egui panel structure (crates/automixah-ui/src/app.rs `update`)

Current order: `top("top")` → `right("grid_controls")` → central. New order: `top("top")` → **`bottom("playlist")`** (shown before right/central so it spans full width) → `right` → central.

### harmonic-playlist color gradient (port verbatim constants)

```rust
const STOPS: [(f32, (u8, u8, u8)); 6] = [
    (0.0, (70, 130, 230)),  // blue    — harmonically identical
    (0.2, (0, 190, 220)),   // cyan
    (0.4, (40, 200, 100)),  // green
    (0.6, (240, 200, 40)),  // yellow
    (0.8, (245, 140, 30)),  // orange
    (1.0, (220, 60, 50)),   // red     — opposite side of the wheel
];
// clamp distance to [0,1]; find bracketing stop pair; lerp each channel; round.
```

Row text format (harmonic-playlist parity): BPM right-aligned width 3 rounded to integer, `" "` , Camelot key width 3, `" "`, `mm:ss`. Missing → `---` / `--:--`.

## Implementation Algorithm (How)

### Schema v2 — key columns

```sql
ALTER TABLE beat_grids ADD COLUMN key_root INTEGER;  -- 0..=11, NULL = unknown
ALTER TABLE beat_grids ADD COLUMN key_mode INTEGER;  -- 0 = Major, 1 = Minor, NULL = unknown
```

No CHECK constraint possible on ALTER; validate in Rust (`key_mode` must be 0/1, `key_root` 0..=11) when reading/writing. Legacy rows have NULL key → treated as "not yet key-analyzed" and re-enqueued once by hydration.

### Schema v3 — playlists

```sql
CREATE TABLE tracks (
  track_hash       TEXT PRIMARY KEY,
  title            TEXT NOT NULL,
  artist           TEXT NOT NULL,
  duration_seconds REAL,                -- NULL until analyzed
  updated_at       INTEGER NOT NULL
);
CREATE TABLE playlists (
  id         INTEGER PRIMARY KEY AUTOINCREMENT,
  name       TEXT NOT NULL UNIQUE,
  created_at INTEGER NOT NULL
);
CREATE TABLE playlist_tracks (
  playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
  position    INTEGER NOT NULL,
  track_hash  TEXT NOT NULL REFERENCES tracks(track_hash),
  added_path  TEXT NOT NULL,
  PRIMARY KEY (playlist_id, position),
  UNIQUE (playlist_id, track_hash)      -- duplicate rejection at DB level
);
```

FK enforcement: enforce ordering in store logic (`tracks` row inserted before `playlist_tracks` row; `delete_playlist` deletes `playlist_tracks` then `playlists` explicitly). Do not rely on the `foreign_keys` pragma being enabled by the pool.

### `PlaylistStore` trait

```rust
#[async_trait]
pub trait PlaylistStore: Send + Sync {
    async fn list_playlists(&self) -> Result<Vec<PlaylistSummary>, Report<PlaylistStoreError>>;
    async fn create_playlist(&self, name: &str) -> Result<PlaylistSummary, Report<PlaylistStoreError>>;
    async fn rename_playlist(&self, id: i64, name: &str) -> Result<(), Report<PlaylistStoreError>>;
    async fn delete_playlist(&self, id: i64) -> Result<(), Report<PlaylistStoreError>>;
    /// Joined view: playlist rows + track tags + stored grid/key (LEFT JOINs).
    async fn tracks_for(&self, id: i64) -> Result<Vec<PersistedTrack>, Report<PlaylistStoreError>>;
    async fn upsert_track_meta(&self, hash: &TrackHash, title: &str, artist: &str, duration: Option<f32>)
        -> Result<(), Report<PlaylistStoreError>>;
    async fn insert_track(&self, playlist_id: i64, position: i64, hash: &TrackHash, path: &str)
        -> Result<(), Report<PlaylistStoreError>>;
    async fn remove_track(&self, playlist_id: i64, position: i64) -> Result<(), Report<PlaylistStoreError>>;
    /// Reorder = delete all rows for the playlist, re-insert in new order, in one transaction.
    async fn reorder(&self, playlist_id: i64, ordered: &[(i64 /*position*/, TrackHash, String /*path*/)])
        -> Result<(), Report<PlaylistStoreError>>;
    fn name(&self) -> &'static str;
}
// + PlaylistStoreService wrapper (Arc<dyn PlaylistStore>), PlaylistStoreError colocated.
```

`PersistedTrack { position, track_hash, added_path, title, artist, duration: Option<f32>, grid: Option<GridOverride> }` — `grid` carries key when present.

### djcore tag probe (`decoder/meta.rs`)

```rust
pub struct TrackTags { pub title: Option<String>, pub artist: Option<String>, pub duration_seconds: Option<f32> }
pub fn probe_metadata(bytes: &[u8], extension: &str) -> Result<TrackTags, Report<DecodeError>>;
```

Use symphonia's probe with `MetadataOptions::default()` (same as `decode_bytes` does), read the format's metadata revisions (`format.metadata()` current + revised) for standard tags (`title`, `artist` vendor fields). Duration from the default track's `codec_params` (frames/time_base) — approximate is acceptable. **Do not decode packets.**

Filename fallback (applies in the queue, not in djcore): split the file stem on `" - "` once (first occurrence) → `(artist, title)`; no separator → title = stem, artist = `""`. Render "Artist – Title" skipping the artist when empty.

### `AnalysisQueue` (playlist/queue.rs)

Single dedicated worker thread (plain `std::thread`, lives until the sender side drops at app exit). Constructed with `Services` clone; communicates via std mpsc:

```rust
pub struct QueueJob { pub row_id: u64, pub playlist_id: i64, pub position: i64, pub path: PathBuf }
pub enum QueueEvent {
    Analyzing { row_id: u64 },
    Ready     { row_id: u64, meta: TrackMeta },   // bpm, key, duration, title, artist
    Failed    { row_id: u64, message: String },   // Report rendered to String (Reports are !Send)
}
pub struct AnalysisQueue { tx: Sender<QueueJob>, pub events: Receiver<QueueEvent> }
```

Worker loop (FIFO, one job at a time; store calls via `services.runtime.handle().block_on`, mirroring `spawn_load`):

1. `recv()` a job (blocks; thread parks when idle).
2. Hash the file (`hash_file` logic — reuse/extract from `track.rs`). Missing/unreadable file → `Failed`, row not persisted beyond `playlist_tracks` (see step 3 note), continue.
3. Tag probe (`probe_metadata`) + filename fallback → `(title, artist)`. Persist `tracks` row (duration NULL) + `playlist_tracks` row (hash, position, added_path) — **persist even on later failure**, so the playlist reflects what the user added; a failed row re-queues on next startup (hydration sees no grid).
4. Library lookup: grid store `get(hash)` with grid **and** key present AND `tracks.duration` known → send `Ready` with meta from DB. Done (no decode).
5. Otherwise send `Analyzing`, then decode (`DecoderRegistry::with_symphonia()`, constructed inside the thread — never shared across threads), analyze via injected `Arc<dyn AudioAnalyzer>` on the mono downmix, persist grid+key (`grid_store.put` with `key: Some`), update `tracks` row (duration), send `Ready`.
6. **Drop PCM** — the decoded `DecodeAudio` is a local binding; nothing retains it. (Test asserts memory stays flat.)

UI-side row state machine:

```
RowStatus::Queued    --(worker picks job)-->       RowStatus::Analyzing
RowStatus::Analyzing --Ready-->                    RowStatus::Ready(meta)
RowStatus::Queued|Analyzing --Failed-->            RowStatus::Failed(message)
RowStatus::Failed    --(app restart: hydration)--> RowStatus::Queued
```

Icons: `Queued` → clock/hourglass glyph or `≡`; `Analyzing` → `ui.spinner()`; `Failed` → `⚠` + tooltip with message; `Ready` → no icon.

### UI model + hydration (playlist/mod.rs)

```rust
pub struct PlaylistRow {
    pub row_id: u64,               // session-local, generated by a counter
    pub position: i64,
    pub path: PathBuf,             // added_path
    pub hash: Option<TrackHash>,   // filled when known (library hit / analysis)
    pub title: String, pub artist: String,
    pub bpm: Option<f32>, pub key: Option<Key>, pub duration: Option<f32>,
    pub status: RowStatus,
}
pub struct PlaylistState {
    pub playlists: Vec<PlaylistSummary>,
    pub selected: Option<i64>,
    pub rows: Vec<PlaylistRow>,    // selected playlist's rows, in order
    pub next_row_id: u64,
    pub drag: Option<DragState>,   // reorder gesture
}
```

Startup (in `AutomixahUiApp::new` or first frame, off-thread with an event back — follow the `flush_save_if_due` spawn pattern): `list_playlists` → for the selected (last-used or first) playlist `tracks_for` → per row: grid+key+duration present → `Ready`; else push `QueueJob` (fresh `row_id`) → `Queued`.

### Panel rendering (playlist/view.rs)

`egui::TopBottomPanel::bottom("playlist").resizable(true).default_height(220.0)`, shown **before** the right panel and central panel in `update()` so it spans the full window width. Inside:

- `egui::SidePanel::left("playlist_lists")`: playlist names, one per row; selected highlighted; "＋ new" button at the bottom; right-click a name → context menu (Rename…, Delete). Selecting a playlist triggers `tracks_for` hydration (spawn + event).
- Remaining area = content column. Header row: `Add…` button + (optional) track count. `Add…` opens `rfd::FileDialog::pick_files()` (multi-select, same audio filter as today). Each picked path: duplicate check against current rows' hashes — paths whose hash is already in this playlist are skipped (hash known only after the worker computes it for new files; for already-known hashes compare directly, for new files compare by canonical path as a fast pre-check and by hash when the `Ready`/`insert_track` event lands — a UNIQUE-violation error from `insert_track` surfaces as a skipped duplicate). New rows appended with `position = max+1…`, status `Queued`, jobs pushed.
- Track rows (one `egui::Response` per row rect): `[drag handle] Artist – Title ... 138  8A  05:32 [status icon]`. BPM/Key/Duration right-aligned fixed widths; name truncated with ellipsis to the remaining width.
- **Color**: `key` present on this row *and* the previous row → `harmonic_color(this.key.harmonic_distance(prev.key))` applied to the key text (`RichText::color`); otherwise default text color. First row is never colored.
- **Drag reorder**: `drag_started_by(Primary)` on a Ready row (drag disabled for Queued/Analyzing rows — their DB row may not exist yet, so a persisted position rewrite would race) → record source index; while dragging, compute the insertion index from pointer y over row rects and paint an insertion line; on `drag_stopped` → `Vec::splice` the rows, assign consecutive positions, persist via `reorder()` (spawn). Colors recompute automatically next frame.
- **Remove**: right-click row → context menu → Remove. Removes from UI immediately; `remove_track(playlist_id, position)` spawned; positions rewritten via `reorder`. Removing a Queued/Analyzing row is allowed — the worker still finishes (library warm-up) and its `row_id`-addressed event is dropped because the row no longer exists.
- **Click a Ready row** → identical to the old `Open…` flow: clear `track`/`peaks`/`engine`, `spawn_load(path)`, editor pipeline unchanged.
- While any row is `Queued`/`Analyzing`, `ctx.request_repaint()` each frame (spinner animation + prompt event polling).

### App wiring changes (app.rs)

- Remove the `Open…` button, the `rfd` dialog block, and `track::open_pick`; keep the `re-analyze` button and status line.
- Add `playlist: PlaylistState`, `queue: AnalysisQueue` (constructed in `AutomixahUiApp::new` from `services`), and a store-event receiver (outcomes of playlist CRUD/reorder spawns — status-line messages).
- Per frame, before rendering: drain `queue.events` (update rows by `row_id`), drain store outcomes.
- Empty states: no playlists → hint + "＋ new creates your first playlist"; empty playlist → "Add… to add tracks".

## Anti-Goals (Out of Scope)

- **No rendering/mixing from the UI playlist** — follow-up task after this lands.
- No BPM/key editing from the playlist (grid editing stays in the waveform editor).
- No variable-tempo support (global assumption, per record).
- No m3u export, no playlist import, no drag-and-drop *from the file manager* (only in-app reordering; the Add button + file browser is the entry).
- No multi-playlist drag (moving tracks between playlists).
- No cancellation of in-flight analysis jobs.
- No seek/playback changes; the scrub/waveform editor is untouched apart from load entry point.
- No CLI changes.

## Edge Cases & Gotchas

- **egui panel order matters**: the bottom panel must be shown before the right/central panels in `update()` or it will not span the full width.
- **`Arc<Runtime>`, never a bare `Handle`** in anything long-lived — a dropped runtime turns spawns into silent no-opes (recorded shipped bug). The queue thread holds a `Services` clone (which owns the `Arc`).
- **`Report` is not `Send`** — render to `String` before crossing thread boundaries (existing `LoadEvent::Done(Box<Result<..., String>>)` pattern).
- **Legacy rows** (grid stored pre-v2, key NULL) re-analyze exactly once via hydration to backfill the key; after that they hit the library fast path. This is a one-time cost, consistent with the amended `(db)` record entry.
- **Manual grid edits must not clobber keys** — COALESCE in the upsert; `flush_save_if_due` constructs `GridOverride` with `key: None`.
- **Queued/Analyzing rows cannot be reordered** (persisted-position race); they *can* be removed (worker event for a dead `row_id` is ignored).
- **Duplicates**: rejected within one playlist (DB UNIQUE + pre-check); allowed across playlists (shared `beat_grids`/`tracks` rows).
- **PCM memory**: the worker's decoded audio must not escape the loop. Rows carry only metadata.
- **Clicking non-Ready rows does nothing** (avoid double-analysis racing the queue).
- **`key_mode`/`key_root` have no CHECK constraints** (ALTER TABLE limitation) — validate in Rust on read/write.
- **FK pragmas may not be enabled** by the daow pool — enforce referential ordering in store code; `delete_playlist` cascades manually.
- **SQLite REAL is f64** — keep the existing `#[expect(clippy::cast_precision_loss, ...)]` cast pattern at conversion sites.
- **Duration unknown pre-analysis** → render `--:--`; BPM/key unknown → `---`.
- **Failed rows persist** in `playlist_tracks` (so the playlist reflects user intent) and re-queue on next startup; missing/unsupported files show `⚠` with the message and don't panic.
- **Empty playlist names rejected**; duplicate playlist names surface the UNIQUE error in the status line.
- **`rfd` multi-select** uses `pick_files()` (plural), same audio extension filter built from `DecoderRegistry::supported_extensions()`.

## Navigation Anchors

- `automixah_schema::run_migrations` / `apply_migration_chain` / `migrate_v1` — where v2/v3 slot in.
- `store::GridOverride` + `SqliteGridStore::put` — key column read/write + COALESCE upsert.
- `AutomixahUiApp::update` — panel order, event draining, `Open…` removal.
- `track::spawn_load` — analysis persist site (add `key: Some(...)`).
- `djcore` `SymphoniaDecoder::decode_bytes` — probe_metadata mirrors its prologue (probe + metadata options), minus packet decoding.
- `Services` struct — new fields; `main.rs` block-scoped assembly.

## Dependency Mappings

**External: none.** symphonia (djcore dep), rfd, egui (spinner, context menus, resizable panels), tokio, daow, parking_lot are already dependencies of the touched crates.

**Internal:**
- `automixah-ui` → `djcore::key::{Key, KeyMode, KeyFormat}` (already a dependency; first UI-side use).
- `automixah-ui` → `djcore::analyzer::{AudioAnalyzer, AnalyzerOutput, FakeAnalyzer}` (analyzer injection).
- `automixah-schema` unchanged consumers; `run_migrations` still called by `SqliteGridStore::open_or_create` (playlist store must open through the same DB/pool — see below).

**Shared database**: `PlaylistStore`'s SQLite backend must use the *same* `library.sqlite` (the `daow` pool can be shared or a second pool opened on the same path; prefer passing the existing pool in from `main.rs` so migrations run once).

## Test Strategies

Per phase; all follow house rules (BDD Given/When/Then comments, one behavior per test, `rstest` for same-property-many-inputs).

**Phase 1 (schema & stores):**

| Test | Verifies |
|---|---|
| `migrations_apply_v2_v3_idempotently` | fresh DB and a v1-only DB both migrate to v3; second run no-ops; `_migrations` has 3 rows |
| `v2_key_columns_nullable_on_legacy_rows` | a v1-era row survives migration, reads back with `key: None` |
| `grid_upsert_preserves_stored_key` | `put` with `key: None` after a keyed row keeps the key; `put` with `Some` overwrites |
| `playlist_store_crud_and_ordering` | create/rename/delete; insert/`tracks_for` order; `reorder` rewrite (sqlite + in-memory backends, `rstest`-parameterized) |
| `playlist_rejects_duplicate_hash` | second `insert_track` with the same hash in one playlist errors |

**Phase 2 (queue & metadata):**

| Test | Verifies |
|---|---|
| `tag_probe_prefers_tags_falls_back_to_filename` | probe returns container tags; fallback split produces artist/title |
| `queue_transitions_queued_analyzing_ready` | with `FakeAnalyzer`: events arrive in order; grid+key+tags persisted (in-memory stores) |
| `add_with_library_hit_skips_queue` | pre-seeded grid+key+duration → `Ready` without `Analyzing`, `FakeAnalyzer::call_count() == 0` |
| `queue_worker_drops_pcm` | after N analyzed jobs, process RSS / allocation footprint stays flat (rough bound acceptable) |
| `startup_hydration_reads_playlists_and_meta` | persisted playlist + analyzed tracks → rows hydrate `Ready`; missing-key row enqueues |
| `missing_file_job_reports_failed` | nonexistent path → `Failed` event, no panic |

**Phase 3 (UI logic — pure parts only, no egui harness):**

| Test | Verifies |
|---|---|
| `harmonic_color_endpoints` | distance 0 → blue, 1 → red, midpoints match stops |
| `row_color_uses_previous_row_key` | color logic: first row uncolored; missing keys uncolored |
| `reorder_splice_reassigns_positions` | the splice+renumber function used by the drag handler |

**Phase 4:** `just check`, `just test`, `just lint` green; module `//!` docs for all new files.

**Manual smoke** (documented in the final summary, not automated): add harmonic-playlist's `tracks/` files, observe queued→spinner→values, drag reorder recolors, restart restores, click loads editor.

## Acceptance Criteria

- Playlist section renders below all existing UI; `Open…` gone; adding via button opens a file browser (multi-select).
- Row shows title/artist (tags or filename), BPM, Camelot key, duration when known; `--`/`---` when not.
- Key text color = harmonic distance to previous row (first row uncolored).
- Unknown tracks show queued icon, then spinner while analyzing, then values — one at a time, UI thread never blocks.
- Reordering/removing persists (restart restores order); clicking a row loads it into the grid editor; library hits never enqueue.
- Grid edits preserve stored keys; `just check && just test && just lint` green.

## Phases

### Phase 1 — Schema & Stores
Migrations v2/v3 in `automixah-schema`; `GridOverride.key` with COALESCE-preserving upsert (sqlite + in-memory); `PlaylistStore` trait + service + sqlite + in-memory backends; `Services` gains `playlist_store` + `analyzer` (Arc<dyn AudioAnalyzer>); `main.rs` assembly shares the library DB; Phase-1 tests.

### Phase 2 — Queue & Metadata
djcore `decoder/meta.rs` tag probe; `AnalysisQueue` single-worker FIFO with events (hash → probe/persist → library-hit fast path → decode+analyze+persist → drop PCM); `spawn_load` persists key; startup + playlist-switch hydration; Phase-2 tests (FakeAnalyzer, in-memory stores).

### Phase 3 — Playlist UI
Full-width resizable bottom panel; playlist list column (select, ＋ new, rename/delete); content column with Add… (multi-select rfd), rows (artist–title, BPM, colored Camelot key, duration, status icons); drag-reorder (Ready rows) with persisted positions; right-click remove; row click loads the editor via `spawn_load`; remove the `Open…` button and `open_pick`; empty-state hints; per-frame event draining + repaint-while-busy; pure-logic UI tests.

### Phase 4 — Tests & Docs
Full workspace suite green (`just check`/`just test`/`just lint`); module docs for all new files; AGENTS.md data-flow diagram gains the playlist-library line; manual smoke pass summary.

### Phase 5 — Verification
Walk each acceptance criterion against the running implementation; apply Record Updates (below) or surface divergence.

## Record Updates (apply at end of implementation)

- `(db) Track analysis persists BPM, key, and the beat grid per content hash; manual grid edits preserve the stored key.` *(amends the existing grid-library entry's scope)*
- `(db) Playlists persist to the library database as ordered content-hash references with add-time paths; track tags (artist/title/duration) persist keyed by content hash.`
- `(ui) automixah-ui's playlist section (bottom panel) lists playlists and their tracks; rows show BPM, Camelot key colored by harmonic distance to the previous row, and duration.`
- `(ui) Track loading enters through the playlist: clicking a row loads the track into the grid editor; the Open button is removed.`
- `(ui) Playlist analysis runs on a single-worker FIFO queue that decodes, analyzes, persists, and drops PCM; rows show queued/analyzing/ready state.`
