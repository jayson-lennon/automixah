# Style Guide

This document defines the _coding conventions_, _patterns_, and _architecture_ for the `automixah` codebase.

- The Mixxx source at `/mnt/zed/repos/third-party/mixxx` is a reference, not a dependency. Port algorithms by reading, do not vendor code.

## 1. Overview

automixah is an experimental auto-DJ application: fixed-BPM tracks are decoded, analyzed (constant beat grids), planned into overlapping transitions, and rendered to a mixed WAV. A companion egui desktop tool (`automixah-ui`) aligns beat grids by hand and auditions them with vinyl-style scrubbing.

This guide keeps patterns _sparse and generic_ so they survive architectural change. When this document and the code disagree, fix the document in the same change.

## 2. Core Patterns

### Error Handling

Use `wherror::Error` with `error_stack::Report` for all fallible operations.

**Colocate errors with their related types.** Never create standalone `error.rs` or `errors.rs` files. Error types belong in the same module as the trait, struct, or function that produces them (e.g. `GridStoreError` lives in `store/mod.rs` next to the `GridStore` trait).

```rust
use wherror::Error;

#[derive(Debug, Error)]
#[error(debug)]
pub struct DecoderError;
```

```rust
use error_stack::{Report, ResultExt};

pub fn decode(bytes: &[u8]) -> Result<Audio, Report<DecoderError>> {
    let pcm = inner(bytes)
        .change_context(DecoderError)
        .attach("failed to decode track")?;
    Ok(pcm)
}
```

Document errors on public functions:

```rust
/// # Errors
///
/// Returns an error if the output device cannot be opened.
pub fn start() -> Result<Engine, Report<OutputEngineError>>
```

### Trait Usage

Every external dependency or persistence backend gets a trait abstraction. Production implements the trait with the real backend; tests implement it with fakes (e.g. `GridStore` has `SqliteGridStore` and `InMemoryGridStore`).

```rust
pub trait GridStore: Send + Sync {
    fn get(&self, hash: &TrackHash) -> Result<Option<GridOverride>, Report<GridStoreError>>;
    fn put(&self, hash: &TrackHash, o: &GridOverride) -> Result<(), Report<GridStoreError>>;
}
```

**Service wrapper pattern** — structs wrap `Arc<dyn Trait>`; callers hold the wrapper, never the trait object:

```rust
#[derive(Clone)]
pub struct GridStoreService {
    backend: Arc<dyn GridStore>,
}
```

**Colocate traits with their related types.** Never create standalone `traits.rs`.

### Module System

- Directories use `mod.rs` (e.g. `store/mod.rs` declares `sqlite.rs` and `in_memory.rs`).
- One concept per file; feature directories group related modules.

### Dependency Injection

Binaries assemble a `Services` container once at entry and share clones everywhere. Every field is either cheap-to-clone (`Arc` of data or of a trait-backed service wrapper). See `automixah-ui/src/services.rs` for the pattern.

**Runtime lifetime gotcha:** if `Services` needs a tokio runtime, store `Arc<Runtime>` — not a `Handle`. A dropped `Runtime` turns every later `spawn`/`spawn_blocking` into a silent no-op (this shipped as a real bug once: a load spinner that never completed).

### Block Scoping

When a value requires multiple setup steps, wrap the setup in a block so the final binding is immutable and temporaries don't leak:

```rust
let services = {
    let paths = AppPaths::resolve();
    let store = GridStoreService::new(Arc::new(SqliteGridStore::open(&paths)?));
    Services::new(paths, store)
};
```

### Threading Boundaries

- **Never block the UI thread.** File I/O, hashing, decoding, and analysis run on `spawn_blocking`; results land on the bus.
- **The audio callback is a real-time boundary.** It reads shared state via `parking_lot` locks kept tiny; treat allocation and long work there as bugs to fix when touched.
- Cross-thread results are plain bus events (the single `Event` enum), never callbacks — see “Frontend State & Messaging” below.

### Frontend State & Messaging

The egui frontend is immediate-mode — what renders is a function of state, every frame. Each rule below was learned from a shipped bug; follow them when touching `automixah-ui`.

- **Messages address stable identities, never structural indexes.** The content hash is a track's identity everywhere — store key, playlist reference, and event target. Rowids, positions, and `Vec` indexes are implementation details of the structure that owns them; no other task can know them, so they never appear in events or cross-module signatures.
- **Each fact lives in exactly one place; everything else derives.** Track facts (tags, analysis state) live only in the track database keyed by hash; playlist contents are ordered hash lists. Display state (glyph, metadata, interactivity) is computed at render time from the record. If two structures can disagree, one of them is wrong.
- **Compute display state at render time.** The view reads `(row, record)` and derives what to paint. A status field copied onto rows or poked by event handlers is a cache that drifts.
- **One event dialect, one mutation path.** Every async outcome reports through the single bus as an `Event`; frontend state mutates only in the event applier. Two pipelines speaking two dialects for the same data is the bug — unify the dialect rather than bridging it.
- **The session is authoritative over the store.** Store reads fill absent records only; they never overwrite live or in-flight state. A user action that discards data discards it in memory first — the display follows automatically because it derives.
- **Count outstanding async work (`usize`), clear on every terminal outcome.** A task counter increments when work starts and decrements on every way it can end — success, skip, and failure alike.
- **One analysis pass per hash.** Analysis is CPU-heavy and serialized on a single worker; workers drop PCM and only the loaded deck holds decoded audio. Route one user action through one pipeline, never two.

### Frontend Anti-Patterns

Do not do these. Each one has shipped as a bug.

- **Do not address events by rowid, position, or index.** Tasks outside the owning structure cannot know them; use the content hash.
- **Do not cache display state on rows** (status enums, copied BPM/key/duration fields). Derive it at render from the track record.
- **Do not mutate frontend state outside the event applier.** Not from spawned tasks, not from render code, not from gesture handlers — emit an event or an action.
- **Do not let store reads overwrite session state.** Session truth wins; reads fill gaps only.
- **Do not duplicate a helper across files.** Hashing, tag resolution, and path extension live in `track::identity`; every call site consumes it.
- **Do not construct `StratumAnalyzer` directly.** Analysis runs through `services.analyzer` so the whole app is fake-analyzer testable (the sole construction site is the DI assembly in `main`).
- **Do not keep a second copy of a pipeline.** One load path, one analysis path; a "convenience" synchronous duplicate rots.

### Numeric & Time Conventions

- DSP math is `f32` throughout (with `#[expect(clippy::cast_precision_loss, ...)]` reasons where casts are needed).
- Distinguish _source time_ (seconds/frames in a track) from _session time_ (seconds in the mix) in names; never convert implicitly.
- Beat grids are canonical as `(grid_bpm, anchor, downbeat_phase)`; `beats`/`downbeats`/`bars` arrays are projections — never treat derived arrays as source of truth or edit them directly.

## 3. Architecture

Sparse by design — the crate list below is the only fixed map:

| Crate | Role |
| --- | --- |
| `stratum-dsp` | DSP feature extraction (onsets, tempo, beat grids) |
| `djcore` | Decode + analyze: files in, `AnalyzerOutput` (grid, key, BPM) out |
| `automixah-schema` | Shared data types |
| `automixah-engine` | Transition planning and mix rendering |
| `automixah-cli` | Offline render binary (WAV out) |
| `automixah-ui` | egui grid-alignment + scrub-audition tool |

Data flows one direction:

```
files → decode → analyze (constant grid) → plan (phase-snapped overlap) → render → WAV
                     ↘ sqlite grid library (manual overrides) ↗
                         ↘ playlist library (playlists, track tags, analysis queue) ↗
```

Rules that hold across all crates:

- The engine consumes analysis results; it never re-derives them.
- The UI consumes `djcore`/engine types read-only and persists edits through its store traits.
- Fixed-BPM input is a global assumption; variable-tempo support is out of scope.

When adding a subsystem, extend this diagram with a one-line box — resist documenting internals here.

## 4. Tests

- Tests verify _observable behavior_ only; testing internals is an anti-pattern.
- **One test, one behavior**: exactly one `// When` and one `// Then` (with `// And` elaborations of the same behavior). Split separate concepts into separate tests; duplicated setup is acceptable.
- BDD structure with Given/When/Then comments; the test name reads as a standalone sentence (`clamp_pan_allows_one_screen_overscroll_each_side`).
- Parameterize same-property-many-inputs cases with `rstest`; different behaviors get separate tests.
- Real-audio integration checks (decode → analyze on reference OGGs, rendered-mix alignment) live as `#[ignore]`-style heavy tests or examples (`beat_diag`, `align_check`) — keep them runnable without special setup when possible.

## 5. Documentation

- Module docs (`//!`) explain purpose and high-level behavior, not implementation detail.
- Type docs describe what a value _means_ (units, time domain), especially for floats: "seconds in source time", "frames", "device frames per callback".
- Comments explain _why_, never narrate the obvious.

## 6. Modification Guide

Locate concerns by convention, not hardcoded paths — `grep`/`rg` for the current location if unsure.

1. **DSP/analysis change** → `stratum-dsp/src/features/*`; grid math lives with the beat-tracking feature.
2. **Decode/analyze change** → `djcore/src` (`decoder/`, `analyzer.rs`).
3. **Transition/placement change** → `automixah-engine/src/timeline` (planning) and `render` (mixdown).
4. **New persisted data** → add a store trait method + SQLite migration (`_migrations` versioning) + in-memory impl, then wire through `Services`.
5. **UI change** → `automixah-ui/src`: `tracks.rs` (track database), `playlist/` (ordering, queue, panel view), `deck.rs` (loaded media/playback), `bus.rs` (event dialect), `view/` (waveform, grid overlay), `audio/` (output, scrub), `app.rs` (wiring + event applier).
6. **Write tests** per §4 — a unit test next to the module, an integration test for cross-crate behavior.
7. **Update `.agents/RECORD.md`** only via the Record Updates mechanism (human-approved, at end of implementation).

## 7. Tooling

Read the `justfile` for the full list; prefer `just` recipes over manual invocation.

| Role | Command | Description |
| --- | --- | --- |
| `vcs` | Git | This repo uses git. |
| `check` | `just check` | `cargo check --workspace --all-targets` |
| `test` | `just test` | `cargo nextest run --workspace` + doc tests — the fast suite; all must pass before committing |
| `test-heavy` | `just test-heavy` | the `#[ignore]`d real-audio tests (`--run-ignored only`) |
| `lint` | `just lint` | clippy with warnings as errors |
| `format` | `just fmt` | `cargo fmt --all` |
| `commit` | `just commit '<message>'` | Stages everything and commits |
| `build` | `just build` | Debug build of the CLI |
| `mix` | `just mix <out> <tracks...>` | Render a mix through the CLI |

### Plan Directory

Task plans live in `.plans/<task>/` (`plan.md` spec plus phase notes). The task list tracks progress; the spec is immutable — annotate divergence, never rewrite. The project record lives at `.agents/RECORD.md` and is edited only through approved Record Updates.

## 8. Misc

- Never hand-roll string splitting with `.chars()` indexing; use `unicode-segmentation` if it ever comes up.
- No trivial setters; prefer semantic actions (`shift_by`, `snap_nearest_beat`).
- Environment variables are read only at startup (path resolution) and saved into structs.
- Prefer `match` over `if` chains; `where` clauses for generics.
- Code comments must never discuss "spec divergence" or planning — plans are not persisted context.
