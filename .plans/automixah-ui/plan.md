# Spec: automixah-ui — Manual Beat-Grid Alignment Tool

## Problem

Auto-detected beat grids are continuous but not *aligned* to the tracks' actual beats; mixes land a few tens of milliseconds off. The user wants to hand-align grids the way one would in Mixxx, and audition audio by scrubbing. This UI also plants the seed for the future automation-editing tool. The CLI is explicitly out of scope for this task and will be remade after the UI exists.

## Solution

A new binary crate `automixah-ui` (egui/eframe):

- Load a track (file dialog) → decode with `djcore` → render a Mixxx-style waveform (3-band Bessel-4 filters at 600/4000 Hz, max-of-|abs| peaks, RGB blend) → overlay the constant grid (`grid_bpm` + `anchor_seconds` + downbeat phase; `beats/downbeats/bars` are projections) → edit BPM/anchor/downbeat (sliders, nudges, "mark downbeat at cursor") → audition via vinyl-style scrub audio (cpal, varispeed with pitch follow; space = 1× playback; drag = drag-velocity speed; release restores prior state) → persist overrides to a SQLite library (`~/.local/share/automixah/library.sqlite`, keyed by the existing content `TrackHash`) behind a store trait with SQLite + in-memory implementations, following jinn's architecture patterns (`daow`/`#[dao]`, schema-leaf migration with `_migrations` versioning, `wherror`+`error_stack`, `parking_lot`, `dirs`, `Services` DI container, `rfd` file dialog).

## Dialectical Outcomes (Why)

1. **Build the grid tool vs. offload to Mixxx analysis.** Mixxx-imported grids would be well-aligned, but the user's workflow requires *manual* alignment anyway and an egui tool is eventually needed for automation authoring. A dedicated tool wins: single source of truth (the same `BeatGrid` model the engine consumes), no Mixxx DB/protobuf parsing dependency, and the investment compounds into the automation editor.
2. **Mixxx-vs-library waveform.** Existing Rust waveform crates are static-image renderers (bake a PNG) or GPU oscilloscope effects; none render scrollable/zoomable 3-band waveforms inside egui. Peak extraction is a single max-reduce pass; painting is `Painter` rects. Hand-roll, porting Mixxx's filter + peak scheme. Mixxx's own storage is u8 quartets at 441 Hz visual rate (~160 KB per track) — proven sufficient.
3. **3-band RGB vs. monochrome.** User chose Mixxx parity. Band coloring makes kicks vs. hats visually distinct, which is precisely what manual alignment needs.
4. **Scrub semantics.** Confirmed vinyl emulation: cursor movement maps to playback velocity (slow → slow, at-BPM → at-BPM), pitch follows speed (varispeed, no WSOLA), release restores prior state. Space toggles 1× play/pause. No metronome ("that's not a thing for DJing").
5. **Persistence.** SQLite (user decision), keyed by content `TrackHash` (survives renames/moves). Store trait (`GridStore`) with `SqliteGridStore` + `InMemoryGridStore` so tests run without a DB. CLI consumption is deferred to the CLI remake; this task only *writes* the library.
6. **Services container.** Per user instruction and jinn `AGENTS.md`: a `Services` struct in `automixah-ui` bundles the app's backends (`grid_store`, `library_path`/paths), constructed once in a block-scoped assembly at startup; every field is either `Arc<T>` data or a trait-backed service wrapper. The eframe `App` holds `Services` (clone-cheap) and all runtime state in a separate `UiState`.
7. **Anti-goal: no engine changes.** The UI consumes `djcore`/`automixah-engine` types read-only; the canonical grid model already exists there.

## Relevant Files (Where)

```
crates/automixah-ui/                  # new binary crate
├── Cargo.toml
└── src/
    ├── main.rs                       # eframe entry; Services assembly block; run_native
    ├── app.rs                        # AutomixahUiApp (eframe App impl): UiState + Services
    ├── services.rs                   # Services container + builder (jinn pattern)
    ├── audio/
    │   ├── mod.rs
    │   ├── bands.rs                  # Bessel-4 low/band/high filters (Mixxx port) + tests
    │   ├── peaks.rs                  # 441 Hz visual-rate peak extraction (u8 quartets) + tests
    │   └── scrub.rs                  # varispeed reader + cpal output thread + tests
    ├── store/
    │   ├── mod.rs                    # GridStore trait + GridStoreError
    │   ├── in_memory.rs              # InMemoryGridStore
    │   ├── sqlite.rs                 # SqliteGridStore (daow Pool + #[dao])
    │   ├── migrations.rs             # async migration runner (wraps schema crate)
    │   └── schema/                   # automixah-schema leaf crate *inside* the UI crate's
    │       ├── lib.rs                #   modules? NO — separate crate: crates/automixah-schema
    │       └── ...
    └── view/
        ├── mod.rs
        ├── waveform.rs               # egui custom widget: zoom/scroll/3-band render + drag
        ├── grid.rs                   # grid overlay painting + edit controls panel
        └── panels.rs                 # top bar (open/save/status), BPM input, nudges

crates/automixah-schema/              # new leaf crate (jinn session-schema pattern)
├── Cargo.toml                        # rusqlite only; no deps on automixah-*
└── src/lib.rs                        # run_migrations(&mut Connection), beat_grids DDL, tests

.plans/automixah-ui/plan.md           # this spec
.agents/RECORD.md                     # amended at end of implementation (Record Updates)
```

Existing files depended on (read-only unless noted):

- `crates/djcore/src/analyzer.rs` — `BeatGrid`, `AnalyzerOutput` (types to mirror/consume; **no edits**)
- `crates/djcore/src/decoder/mod.rs` — `DecoderRegistry::decode(bytes, ext) -> DecodeAudio` (**no edits**)
- `crates/djcore/src/lib.rs` — `AudioAnalyzer`/`StratumAnalyzer` for the initial auto grid (**no edits**)
- `crates/automixah-engine/src/timeline/types.rs` — `TrackHash` newtype (the store key) (**no edits**)
- `/mnt/zed/repos/third-party/mixxx/src/analyzer/analyzerwaveform.cpp` — filter constants & stride scheme (reference)
- `/mnt/zed/repos/third-party/mixxx/src/engine/filters/enginefilterbessel4.cpp` — Bessel-4 coefficients (reference)
- `/mnt/zed/repos/jinn/workspace/crates/jinn-domain/src/feat/session/session_store/sqlite.rs` + `migrator.rs`, `crates/jinn-session-schema/` — daow/migration patterns (reference)
- `/mnt/zed/repos/jinn/workspace/AGENTS.md` — Services container pattern (reference)

## Key Code Context (What)

**Canonical grid model consumed by the UI (djcore, read-only):**

```rust
// crates/djcore/src/analyzer.rs
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct BeatGrid {
    pub grid_bpm: f32,        // 0.0 = no grid (unconfident fallback)
    pub anchor_seconds: f32,  // downbeat in [0, bar); beats = anchor + k·60/grid_bpm
    pub downbeats: Vec<f32>,
    pub beats: Vec<f32>,
    pub bars: Vec<f32>,
}
```

**Decode entry point (djcore, read-only):**

```rust
// crates/djcore/src/decoder/mod.rs
impl DecoderRegistry {
    pub fn decode(&self, bytes: &[u8], extension: &str) -> Result<DecodeAudio, Report<DecodeError>>;
    pub fn supported_extensions(&self) -> Vec<String>;
}
```
`DecodeAudio` carries interleaved stereo f32 PCM + sample rate + duration (verify exact field names on read; `decoder/mod.rs`).

**Store key (engine, read-only):**

```rust
// crates/automixah-engine/src/timeline/types.rs
pub struct TrackHash(pub ...);  // content hash; clone-cheap (inspect exact inner type on read)
```

**Mixxx waveform scheme to port (reference snippets):**

```cpp
// analyzerwaveform.cpp — filters & peak scheme
constexpr double kLowMidFreqHz  = 600.0;
constexpr double kMidHighFreqHz = 4000.0;
constexpr int mainWaveformSampleRate = 441;  // visual samples per second

// Bessel-4: EngineFilterBessel4Low(sr, 600), EngineFilterBessel4Band(sr, 600, 4000),
//            EngineFilterBessel4High(sr, 4000); settle for silence before streaming.

// Per audio sample (stereo): c = fabs(sample); per stride (audioVisualRatio ≈ 100 samples):
//   storeIfGreater(&stride.max[band][ch], c) → quantize → u8 {low, mid, high, all}
//   where all = max(|L|,|R|) unfiltered. Data length = ceil(frames / audioVisualRatio).
```

**GridStore trait shape (new; jinn service-trait style):**

```rust
// crates/automixah-ui/src/store/mod.rs
#[derive(Debug, wherror::Error)]
#[error("beat grid store error")]
pub struct GridStoreError;

pub struct GridOverride {           // stored row; feeds BeatGrid reconstruction
    pub grid_bpm: f32,
    pub anchor_seconds: f32,
    pub downbeat_phase: u8,         // 0..=3 — beat-in-bar of the anchor
    pub updated_at: i64,            // unix seconds
}

#[async_trait::async_trait]
pub trait GridStore: Send + Sync {
    fn get(&self, hash: &TrackHash) -> Result<Option<GridOverride>, Report<GridStoreError>>;
    fn put(&self, hash: &TrackHash, grid: &GridOverride) -> Result<(), Report<GridStoreError>>;
    fn name(&self) -> &'static str;
}
```
Wrapper (cheap-clone, holds `Arc<dyn GridStore>`): `GridStoreService`. Same shape as jinn's `SessionStoreService`.

**Schema-leaf crate (jinn pattern; single source of truth for DDL):**

```rust
// crates/automixah-schema/src/lib.rs
pub struct SchemaMigrationError;
pub fn run_migrations(conn: &mut rusqlite::Connection) -> Result<(), Report<SchemaMigrationError>>;
// v1: CREATE TABLE beat_grids (
//   track_hash TEXT PRIMARY KEY,
//   grid_bpm REAL NOT NULL, anchor_seconds REAL NOT NULL,
//   downbeat_phase INTEGER NOT NULL, updated_at INTEGER NOT NULL);
// records into _migrations(version, name); idempotent.
```

**Services container (jinn AGENTS.md pattern):**

```rust
// crates/automixah-ui/src/services.rs
#[derive(Debug, Clone)]
pub struct Services {
    pub paths: AppPaths,             // Arc<data>-style: XDG dirs resolved once (dirs::data_dir)
    pub grid_store: GridStoreService,// trait-backed wrapper over Arc<dyn GridStore>
    pub handle: tokio::runtime::Handle, // async backend for store calls
}
```
Assembled once in `main.rs` inside a block expression; `App` stores `Services` and mutable `UiState` separately. No `Services::test` needed initially — tests construct stores directly; add a test builder when a second consumer appears.

## Implementation Algorithm (How)

### Services assembly (main.rs)

```
block:
  paths   = AppPaths::resolve()                     // XDG data dir via dirs
  db_path = paths.data_dir.join("library.sqlite")
  pool    = daow::Pool::connect(db_path)            // PoolConfig::default (4 conns)
  run automixah_schema::run_migrations via pool.with_conn  (async migration runner)
  store   = SqliteGridStore::new(pool)
  services = Services { paths, grid_store: GridStoreService::new(Arc::new(store)), handle }
  eframe::run_native(app closure capturing services)
```

### Track load flow (app.rs)

```
on File → Open:
  path = rfd::FileDialog::pick_file(filters = DecoderRegistry::supported_extensions)
  bytes = fs::read(path)
  decode via DecoderRegistry::decode(bytes, ext) → DecodeAudio { pcm, sample_rate, .. }
  hash  = TrackHash::from_bytes(&bytes)            // use djcore/engine existing hashing path
  analysis = StratumAnalyzer::analyze(mono downmix) // auto grid for starting point
  override = block_on(services.grid_store.get(&hash))
  grid = override.map(reconstruct).unwrap_or(analysis.beat_grid)
  peaks = audio::peaks::build(&pcm, sample_rate)    // u8 quartets @ 441 Hz
  UiState { track: LoadedTrack { path, hash, pcm, sample_rate, duration, peaks, grid } }
```
Hashing must match the engine's existing content-hash path (inspect `djcore`/engine hashing on read; reuse, don't reinvent).

### Waveform pipeline (audio/bands.rs, audio/peaks.rs)

1. Three Bessel-4 stateful filters run over interleaved stereo PCM in one pass (streamed in chunks; call `assumeSettled()`-equivalent warmup = pre-roll of zeros so ramps don't dirty the head).
2. Stride loop at `audio_visual_ratio = sample_rate / 441.0` (≈100): per stride keep running max of |L|,|R| per band + unfiltered all; at stride end quantize `min(255, round(x * 255 / 1.0))` into `u8` quartet `(low, mid, high, all)`.
3. Output `Peaks { data: Vec<u8>, visual_rate: 441, stride_frames: usize }`.

### Widget render (view/waveform.rs)

- `Response` from `Sense::click_and_drag()`; canvas = `ui.allocate_rect`.
- Zoom slider sets `px_per_visual_sample` in [min: full-track-in-width … max: ~1 px per 4 visual samples ≈ 9 ms/px]. Scroll: wheel alters zoom around cursor; shift+wheel/drag pans when zoomed in.
- For each pixel column, aggregate the visible stride range to max-of-u8 per band; draw 3 stacked translucent rects (`Color32::from_rgb`) scaled to half-height around center line; downbeat/beat grid lines painted after waveform (thin; beats lighter, downbeats heavier + colored).
- Cursor: fixed vertical line at playhead position; dragging waveform moves `cursor_pos` and reports `dx` to scrub controller.

### Grid edit model (view/grid.rs)

- `EditableGrid { grid_bpm: f32, anchor_seconds: f32, downbeat_phase: u8 }` — canonical subset; `(re)project()` regenerates beats/downbeats/bars arrays exactly like djcore's projection (shared helper or local copy; keep djcore read-only unless a re-export is trivially available).
- BPM: `DragValue` (step 0.01, range 60–200).
- Anchor slider: range [0, beat) with wrap; nudge buttons ±1/±10/±100 ms.
- "Set downbeat at cursor": `anchor = cursor_seconds mod bar` (respecting new BPM), `downbeat_phase` stays; "Shift grid so nearest beat hits cursor": recompute anchor so `cursor ≡ anchor (mod beat)`.
- Every mutation → `project()` → overlay repaints immediately (egui immediate mode).

### Scrub audio (audio/scrub.rs) — state machine

```
States: Paused | Playing (speed 1.0) | Scrubbing (speed = f(drag velocity))
Events:
  Space        → toggle Paused ↔ Playing (cursor starts at playhead)
  drag-start   → remember prior state (Paused/Playing), enter Scrubbing
  drag-move    → speed = clamp(drag_dx_per_frame * px→seconds, -8..8) (vinyl: pitch follows)
  drag-release → restore prior state (Playing resumes from release point; Paused = silence)
```

- cpal output thread (own stream on default device; fallback: error banner, UI still functional). Output callback pulls from shared `Arc<ScrubState>` (`parking_lot::RwLock<ScrubCore>`) holding `pcm`, `sample_rate`, `pos_frames: f64`, `speed: f64`, `mode`.
- Varispeed reader: cubic Hermite interpolation over interleaved stereo PCM at `pos + speed * (device_frames / device_rate * track_rate)`; when `speed ≈ 0`, emit silence but keep tracking cursor.
- Device-rate conversion folded into the same interpolation step (track sample position advances by `speed * track_frames_per_device_frame`).
- Playhead ↔ waveform cursor share `pos_frames` (f64, clamp to [0, len]); UI updates during `update()` via read of `ScrubCore`.
- Gain: −6 dB headroom (deck-style), soft clip at ±1.

### Persistence flow (store/)

- Save button + auto-save on grid mutation (debounced 500 ms): `services.handle.block_on(store.put(hash, GridOverride::from(&editable)))` spawned via `handle.spawn`.
- Load on open (above). `updated_at = now`.
- `SqliteGridStore` uses `daow` `Pool`; single statements via `pool.execute`/`pool.query_*` under `#[dao]` generated methods (follow jinn sqlite.rs shape); `InMemoryGridStore` = `parking_lot::Mutex<HashMap<TrackHash, GridOverride>>`.

## Anti-Goals (Out of Scope)

- **No CLI changes** — the mix CLI keeps re-analyzing; consuming the manual-grid library is a future CLI remake.
- **No engine/djcore contract changes** — read-only consumption.
- **No metronome** — scrub audio only, per user decision.
- **No WSOLA/pitch-preserve scrub** — vinyl varispeed only.
- **No cue points / loops / hotcues** — grid editing only (cue import from Mixxx was floated but explicitly deferred).
- **No stems, no video, no library browsing UI** — single-track open; the DB stores grids, not a browsable library UI.
- **No key/BPM re-analysis edits** — BPM field is grid BPM only.
- **No undo/redo history beyond simple re-edit** (out of scope for v1).
- **No Windows/macOS path special-casing beyond `dirs` defaults.**

## Edge Cases & Gotchas

1. **Bessel filter warm-up**: streaming filters ramp from zero; Mixxx calls `assumeSettled()`. Port the settle-by-silence trick (process a short zero pre-roll) or copy Mixxx's steady-state initialization, else the waveform head renders wrong.
2. **Stride boundary**: last stride is partial; include it (Mixxx includes partial trailing stride via `m_currentStride` advance only on `>=` stride boundary — match: advance only when stride count reached, but still flush the final partial max).
3. **f32→u8 quantization**: Mixxx saturates at 1.0 (`min(255, …)`); PCM from decoders can exceed ±1.0 — clamp before quantize.
4. **Varispeed at boundaries**: clamp `pos` at both ends and zero speed there (don't wrap); at `pos=0` with negative speed → silence.
5. **Drag-velocity spikes**: wheel/trackpad flings produce huge dx; clamp speed to ±8× and smooth with a short one-pole (avoid zipper noise from discontinuous speed changes; also crossfade 64 frames on any speed step >0.05 to prevent clicks).
6. **cpal device rate ≠ 44.1k**: always resample in the interpolation step (never assume device rate); pick device's default config, prefer f32.
7. **async-in-egui**: never `block_on` inside `update()`; spawn store IO onto the tokio `handle` from the `Services` and update status via atomics/`RwLock` (one-frame-late UI is fine).
8. **Hash mismatch**: if the store's hash keying doesn't match djcore's analysis hash, renames break; reuse the exact existing hash path (verify on read — engine hashes file bytes; confirm `TrackHash` construction site and reuse it).
9. **Very long zoom-out**: aggregate to max-u8 per pixel (don't average — transients vanish); cap aggregate scan per frame (skip levels/mipmap not needed at v1, but guard ≥1 visual sample per pixel).
10. **Downbeat phase editing wrap**: anchor nudges past a bar boundary must wrap, and `downbeat_phase` changes re-anchor `anchor_seconds` into `[0, bar)` — keep invariant `0 ≤ anchor < bar`.
11. **Migration idempotency**: `_migrations` table must be checked before DDL; second run must no-op (jinn pattern).
12. **SqliteGridStore pool**: 4 connections; store IO is tiny — never hold a transaction across UI frames.

## Navigation Anchors

- `crates/automixah-ui/src/main.rs` — entry; `Services` assembly; eframe bootstrap.
- `crates/automixah-ui/src/app.rs` — `AutomixahUiApp::update` — top-level frame flow (panels, waveform, scrub wiring).
- `crates/automixah-ui/src/audio/scrub.rs` — `ScrubCore`/`ScrubState` + cpal callback — playback state machine.
- `crates/automixah-ui/src/view/waveform.rs` — custom widget `WaveformView` — render + input entry.
- `crates/automixah-ui/src/store/sqlite.rs` — `SqliteGridStore` — persistence.
- `crates/automixah-schema/src/lib.rs` — `run_migrations` — DDL source of truth.
- Read-only anchors: `crates/djcore/src/analyzer.rs` (`BeatGrid`), `crates/djcore/src/decoder/mod.rs` (`DecoderRegistry::decode`).

## Dependency Mappings

New external (workspace `Cargo.toml` entries):

- `eframe` (egui desktop; latest stable) — UI framework
- `rfd` — native file dialog
- `cpal` — audio output
- `daow` (0.1.0 — match jinn's pinned version) — async rusqlite Pool + `#[dao]`
- `rusqlite` — used directly by `automixah-schema` (bundled SQLite feature so no system dep)
- `dirs` — XDG data dir resolution
- `parking_lot` — locks
- `tokio` — runtime handle (rt only) for store IO
- `async-trait` — GridStore trait
- `wherror`, `error-stack` — errors (already in workspace via stratum-dsp/djcore usage; verify versions)

Existing internal:

- `djcore` (decoder + analyzer; read-only)
- `automixah-engine` (`TrackHash`; read-only)

New internal:

- `crates/automixah-schema` (leaf; rusqlite only)

## Test Strategies

| # | Test | Type | How |
|---|---|---|---|
| T1 | Bessel band split: white noise → per-band RMS/FFT | Unit (`audio/bands.rs`) | Feed 1 s white noise; assert low-band energy concentrated <600 Hz, high >4000 Hz (coarse FFT or per-band RMS ratios) |
| T2 | Peak extraction: synthetic PCM (impulses, clipping >1.0) | Unit (`audio/peaks.rs`) | Stride count = ceil(frames/ratio); impulse lands in correct quartet slot; clamp at 255 |
| T3 | Widget at zoom extremes + grid projection after edits | Unit (headless egui `kittest`-style or pure functions) | Prefer testing pure projection math (`project()`) — widget smoke via `eframe` skipped in CI; assert beats = anchor + k·60/bpm, downbeats every 4th from phase, wrap invariants |
| T4 | Grid edits: BPM/anchor/phase/nudges | Unit (`view/grid.rs`) | `EditableGrid` transitions; invariant 0 ≤ anchor < bar; "set downbeat at cursor" math |
| T5 | Varispeed: sine at 0.5×/1×/2× | Unit (`audio/scrub.rs`) | Render N frames offline from `ScrubCore` at each speed; assert dominant frequency scales ×speed; no NaN/discontinuity > small tol |
| T6 | Scrub semantics: pause→drag→release | Unit (`audio/scrub.rs`) | State machine transitions table-driven; release restores prior state; speed clamps ±8 |
| T7 | Device-rate fold: 44.1k track → 48k device | Unit | Render tone through 48k callback; frequency preserved within 0.5% |
| T8 | Migration idempotency | Unit (`automixah-schema`) | Fresh in-mem DB → v1 applied, `_migrations` row; run twice → no error, no duplicate |
| T9 | Store round-trip SQLite + InMemory | Integration (`store/`) | Save → load → equal; rename path (same bytes) → same hit; both stores pass identical suite |
| T10 | Manual: reference OGG → align → reopen | Manual | Your eyes/ears: grid locks to transients; scrub feels vinyl-like; reopen restores grid |

## Acceptance Criteria

- Any supported track opens and renders a zoomable (overview → ~9 ms/px), scrollable 3-band waveform.
- BPM/anchor/downbeat edits re-project the grid live; manual alignment visually locks to transients on the reference OGGs.
- Audio: space toggles 1× playback; dragging plays at drag velocity with pitch following speed; release returns to prior state.
- Grid edits persist to `library.sqlite` keyed by content hash; reopening the same audio (renamed included) restores the grid; in-memory store passes the same suite.
- No behavior changes to `automixah-cli`, `automixah-engine`, or `djcore` public contracts.

## Phases

1. **Skeleton + Services** — `automixah-ui` crate; `main.rs` with block-scoped `Services` assembly (AppPaths via `dirs`, daow pool, migrations, `GridStoreService`, tokio handle); eframe shell (`app.rs`) with top bar + placeholder waveform area; file open via `rfd` → djcore decode → `UiState::LoadedTrack`. `automixah-schema` leaf crate with v1 DDL + migration tests.
2. **Waveform** — `audio/bands.rs` Bessel-4 port (T1) and `audio/peaks.rs` stride/max/u8 scheme (T2); `view/waveform.rs` custom widget: zoom slider + wheel-zoom-at-cursor, pan, 3-band RGB columns (T3 smoke via pure math where possible).
3. **Grid overlay & editing** — `view/grid.rs` `EditableGrid` + `project()`; overlay painting (beats light, downbeats heavy); BPM DragValue, anchor slider/nudges (±1/±10/±100 ms), "set downbeat at cursor", "snap nearest beat to cursor"; live re-projection (T3/T4).
4. **Scrub audio** — `audio/scrub.rs`: `ScrubCore` varispeed reader (T5), cpal stream + device-rate fold (T7), drag-velocity state machine (T6), playhead↔cursor sharing, gain/soft-clip; UI wiring for space/drag/release.
5. **Persistence** — `store/`: `GridStore` trait + `GridStoreError`; `InMemoryGridStore` (T9), `SqliteGridStore` via daow + `#[dao]` (T8/T9); migration runner wrapper; save-on-edit (debounced) + load-on-open; status line.
6. **Verification** — full suite + `just lint`; manual pass on reference tracks (T10); Record Updates.

## Record Updates

Applied at the **end of implementation** (not at plan approval):

- **Amend** the persistence entry to: *"Track inputs are absolute paths passed via repeated `--track` flags in the given order; the CLI hashes, decodes, and analyzes them from scratch on every invocation. Manual grid alignments live in a SQLite library (`automixah-ui`, XDG data dir, keyed by content hash) which the CLI does not yet consume."*
- **Add**: *"automixah-ui is an egui desktop binary for manual beat-grid alignment: Mixxx-style 3-band Bessel waveform, grid editing (BPM/anchor/downbeat phase), and vinyl-style varispeed scrub-audition over cpal. Grid overrides persist to a SQLite library behind a store trait."*
