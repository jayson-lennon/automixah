# Fix: long-track playback freeze (f32 position precision wall)

## Problem

Audio playback in `automixah-ui` stops towards the end of long tracks. Reported on "Thousand Pieces (Extended Mix)" and "Miss The Scientist" (both in the user's local playlist library).

**Root cause (verified):** `ScrubCore` (`crates/automixah-ui/src/audio/scrub.rs`) tracks its read position as `f32` counted in **source frames**, advanced once per output frame:

```rust
// scrub.rs, read() loop
let last = frames.saturating_sub(1) as f32;
...
self.position = (pos + step).clamp(0.0, last);   // pos: f32, step ≈ 1.0 at 1×
```

`f32` represents integers exactly only up to 2²⁴ = 16,777,216. Beyond that, `pos + 1.0` rounds back to `pos`, so the position freezes permanently and `hermite()` re-emits the same sample — audible as playback stopping dead.

**Empirical verification (already performed during investigation — do not repeat):**

- Decode is NOT the problem: both tracks decode full-length through `djcore::DecoderRegistry` (Miss The Scientist: 21,773,120 frames = 493.722 s @ 44.1 kHz; Thousand Pieces: 20,704,008 frames = 431.334 s @ 48 kHz), matching ffprobe container durations exactly.
- The freeze point is deterministic: 2²⁴ frames = 380.4 s at 44.1 kHz, 349.5 s at 48 kHz.
- Playlist durations in the SQLite library (`~/.local/share/automixah/library.sqlite`) are ~2.4% short of container duration (480.9 vs 493.7; 421.4 vs 431.3). This is **desired behavior, not a bug**: stratum-dsp computes `duration_seconds` after silence trimming, and the app intentionally mixes only the musical portion.

Affected tracks in the user's playlist (all over 2²⁴ frames):

| Track | Frames | Freezes at | Cut |
|---|---|---|---|
| Ong NamAztec (ogg 44.1k, 363.4 s) | 16.03M | — | unaffected |
| Good For Himalaya (ogg 44.1k, 418.3 s) | 18.4M | 380.4 s | −9% |
| Miss The Scientist (ogg 44.1k, 493.7 s) | 21.8M | 380.4 s | −23% |
| Escape from Reality (opus 48k, 377.5 s) | 18.1M | 349.5 s | −7% |
| Thousand Pieces (opus 48k, 431.3 s) | 20.7M | 349.5 s | −19% |

## Solution

Widen the position domain from `f32` to `f64` through the UI playback path: `ScrubCore.position` (plus `read()` loop math and `hermite()`'s fractional position argument), `Playhead.position`/`Playhead.seek` in `output.rs`, `apply_pending_seek`, and the corresponding reads/writes in `app.rs` (the follow-latch field `position_at_update` and both seek-write sites). Convert to `f32` only at the display boundary (`waveform::show` takes `pin_frame: Option<f32>`).

`f64` holds integers exactly to 2⁵³ frames (≈285 years at 48 kHz) — the freeze is structurally eliminated, not merely deferred. Display-side values (`WaveformView.left_frame`, `drag_view_frame`, `pin_frame`) stay `f32` (sub-frame ULP is invisible at pixel resolution), and `Playhead.speed` stays `f32` (display extrapolation only).

This matches the engine crate's existing precedent: `SessionTime(u64)` (`automixah-engine/src/timeline/types.rs`) and `cubic_at(input: &[f32], pos: f64)` (`automixah-engine/src/render/resample.rs`).

## Acceptance Criteria

- A `ScrubCore` parked at 16,777,216.0 frames reading at 1× advances position to exactly 16,777,217.0 (guard test passes; would fail with f32 storage).
- Miss The Scientist plays past 380.4 s and Thousand Pieces past 349.5 s, through to their true ends (493.7 s / 431.3 s).
- No behavior change for short tracks; scrub varispeed, speed crossfade, seek, pause, and end-clamp behavior unchanged.
- Workspace check/test/lint green (`just check`, `just test`, `just lint`).

## Phases

### Phase 1 — ScrubCore to f64 (`scrub.rs`)

- `position: f64`; `ScrubCore::new(channels, start_frame: f64)`.
- `read()`: `last` becomes `f64` (`frames.saturating_sub(1) as f64`); the clamp stays `(pos + step).clamp(0.0, last)` — with pos/last f64, `step` may remain f32 (implicit widening; document if clippy wants an explicit cast).
- `hermite(samples, channels, ch, pos: f64)` — the floor/index math inside widens accordingly (`pos.floor()`, `t = pos - pos.floor()` as f64; tangent/interp coefficients can stay f32 — cast the final result; ensure the fractional `t` passed into the polynomial is f32-cast only at coefficient use, keeping full f64 precision for `i` so frame indexing is exact past 2²⁴).
- `set_speed`/`effective_step`/`fade_remaining` stay f32 (step is a small ratio, never large).
- Update existing tests' `ScrubCore::new(_, ...)` literals to f64 (e.g. `12_345.0` stays fine; `RATE * 1.0` style args fine).
- Add the guard test (see Test Strategies).

### Phase 2 — Playhead to f64 (`output.rs`)

- `Playhead.position: RwLock<f64>`, `seek: RwLock<Option<f64>>`; `speed` unchanged (f32).
- `apply_pending_seek(playhead, scrub, channels)`: takes `Some(frame: f64)`, `ScrubCore::new(channels, frame)`.
- Callback writebacks (`*cb_playhead.position.write() = scrub.position();` — two sites, paused and playing) flow f64 automatically.
- `OutputEngine::start(pcm, source_rate, channels, start_frame: f64)`.
- Update `paused_seek_updates_position` test literal types.

### Phase 3 — app.rs boundary wiring

- `position_at_update: f64` (field + initializer `0.0`).
- Follow block: `raw` is f64; extrapolation `raw + f64::from(speed) * f64::from(elapsed)`; convert to `f32` when building the `follow` option passed to `waveform::show` (`pin_frame: Option<f32>`): `raw as f32` with `#[expect(clippy::cast_precision_loss, reason = "...")]` per repo style.
- Two seek writes become f64: `Some(f64::from(frame))` (drag-end snap — `drag_view_frame` stays f32) and `Some(f64::from(t) * f64::from(sample_rate))` (click-to-seek).
- `start_engine` passes `0.0` (inferred f64).

### Phase 4 — Guard test

One behavioral test in `scrub.rs` (see Test Strategies). No heavy 67 MB buffer test — the f64 fix is structural; the guard exists only to catch a future silent f32 revert.

### Phase 5 — Verify + Record

- `just check`, `just test`, `just lint` all green.
- Manual confirmation on the two reported tracks (play past former freeze points). Files:
  - `/mnt/zed/music/320/trance/vocal/uplifting/Simon Patterson vs. Coldplay - Miss The Scientist (DJ Pitch vs. XiJaro Mashup).ogg`
  - `/mnt/zed/music/320/trance/vocal/uplifting/high energy/A & Z vs. Claudiu Adam & Clara Yates - Thousand Pieces (Extended Mix).opus`
- Record update per below.

## Dialectical Outcomes (Why)

- **f64 positions (chosen) vs integer-frame + f32-fraction split:** the split is exact by construction but requires restructuring `step` (fractional during varispeed), the crossfade blending, and `hermite` — invasive for no user-visible gain. f64 is a minimal diff matching engine-crate precedent (`cubic_at(pos: f64)`).
- **f32 seconds (rejected):** worse, not better — ULP in seconds exceeds the per-frame step around ~190 s @44.1 kHz, freezing even mid-length tracks.
- **Converting `hermite`'s `pos` to f64 too (not just storage):** converting to f32 at the call site would lose the fractional part past 2²⁴ (all f32 precision is spent on the integer part there), reintroducing zipper noise/quantization. The integer index must be derived from the f64 position.
- **Playlist duration discrepancy left as-is:** durations in the library DB come from stratum-dsp's post-silence-trim `duration_seconds`. User confirmed this is desired: the app mixes the musical portion only. Out of scope.
- **No heavy regression test:** user opted out; f64 makes the freeze structurally impossible. A cheap guard test (construct core at 2²⁴, read one frame, assert exact advance) covers the future-revert risk without any large buffer. It runs in the normal test suite.
- **Decode path exonerated by experiment:** a scratch example decoded both tracks through `DecoderRegistry::with_symphonia()` at full length (matching ffprobe). The scratch example was deleted; nothing about decoding changes in this task.

## Relevant Files (Where)

| File | Change |
|---|---|
| `crates/automixah-ui/src/audio/scrub.rs` | `position: f64`, f64 read-loop math, `hermite(pos: f64)`, test updates + new guard test |
| `crates/automixah-ui/src/audio/output.rs` | `Playhead.position/seek` → f64, `apply_pending_seek`, `start()` signature, test literal types |
| `crates/automixah-ui/src/app.rs` | `position_at_update` → f64, follow-extrapolation math, f32 cast at `waveform::show` boundary, two seek writes → f64 |

No new files. No changes to `view/waveform.rs`, `scrub_state.rs`, `djcore`, `stratum-dsp`, or the engine crates.

## Key Code Context (What)

Current declarations being changed:

```rust
// crates/automixah-ui/src/audio/scrub.rs
pub struct ScrubCore {
    channels: usize,
    /// Current fractional read position in frames.
    position: f32,
    step: f32,
    prev_step: f32,
    fade_remaining: f32,
}

impl ScrubCore {
    pub fn new(channels: usize, start_frame: f32) -> Self { /* position: start_frame */ }
    pub fn position(&self) -> f32 { self.position }
    pub fn read(&mut self, samples: &[f32], out: &mut [f32]) {
        let frames = samples.len() / channels;
        let last = frames.saturating_sub(1) as f32;
        for chunk in out.chunks_mut(channels) {
            let step = self.effective_step();
            if self.fade_remaining > 0.0 { self.fade_remaining -= 1.0; }
            let pos = self.position;
            if pos <= 0.0 && step <= 0.0 || pos >= last && step >= 0.0 {
                chunk.fill(0.0);
                self.position = pos.clamp(0.0, last);
                continue;
            }
            for (ch, o) in chunk.iter_mut().enumerate() {
                *o = hermite(samples, channels, ch, pos);
            }
            self.position = (pos + step).clamp(0.0, last);
        }
    }
}

fn hermite(samples: &[f32], channels: usize, ch: usize, pos: f32) -> f32 {
    let frames = samples.len() / channels;
    let i = pos.floor().clamp(0.0, (frames - 1) as f32) as usize;
    let t = pos - pos.floor();
    // Catmull-Rom tangents over at(i-1..=i+2)
}
```

```rust
// crates/automixah-ui/src/audio/output.rs
pub struct Playhead {
    pub position: RwLock<f32>,
    pub seek: RwLock<Option<f32>>,
    pub speed: RwLock<f32>,
}

fn apply_pending_seek(playhead: &Playhead, scrub: &mut ScrubCore, channels: usize) {
    if let Some(frame) = playhead.seek.write().take() {
        *scrub = ScrubCore::new(channels, frame);
    }
}

impl OutputEngine {
    pub fn start(
        pcm: Arc<Vec<f32>>, source_rate: u32, channels: usize, start_frame: f32,
    ) -> Result<Self, Report<OutputEngineError>> { /* ... */ }
}
// callback writebacks (two sites):
//   *cb_playhead.position.write() = scrub.position();
```

```rust
// crates/automixah-ui/src/app.rs
/// The position value at that instant.
position_at_update: f32,

// follow block (~line 903):
let raw = *ph.position.read();
if raw != self.position_at_update {
    self.position_at_update = raw;
    self.position_updated = Some(std::time::Instant::now());
    raw
} else {
    let speed = *ph.speed.read();
    let elapsed = self.position_updated.map_or(0.0, |t| t.elapsed().as_secs_f32());
    raw + speed * elapsed
}

// seek write sites (~992, ~1007):
*engine.playhead().seek.write() = Some(frame);          // drag-end snap (frame: f32 today)
*engine.playhead().seek.write() = Some(t * sample_rate); // click-to-seek (t: f32 seconds)
```

Display boundary (unchanged, f32 stays):

```rust
// crates/automixah-ui/src/view/waveform.rs
pub fn show(ui, peaks, view: &mut WaveformView, pin_frame: Option<f32>) -> (Response, Rect, f32)
```

## Implementation Algorithm (How)

1. **scrub.rs** — change `position` to `f64`; `new(channels, start_frame: f64)`; `position() -> f64`. In `read()`: `let last = frames.saturating_sub(1) as f64;` and `self.position = (pos + f64::from(step)).clamp(0.0, last);` (end-clamp branch likewise clamps f64). In `hermite`: `pos: f64`, `let fl = pos.floor(); let i = (fl as usize).min(frames - 1);` — derive the integer index from f64 directly (never from an f32 of pos); `let t = (pos - fl) as f32;` for the polynomial blend; sample lookup `at(i as isize + k)` unchanged. This keeps frame selection exact past 2²⁴ while the interpolation math stays f32.
2. **output.rs** — `Playhead { position: RwLock<f64>, seek: RwLock<Option<f64>>, speed: RwLock<f32> }`; `apply_pending_seek` now moves `Option<f64>` into `ScrubCore::new`; `start(..., start_frame: f64)`; the two callback writebacks need no edit (type flows).
3. **app.rs** — `position_at_update: f64`; in the follow block keep `raw` f64, extrapolate in f64 (`f64::from(speed) * f64::from(elapsed)`), and map the follow value to `Option<f32>` for `waveform::show` with a documented precision-loss expect. Convert the two seek writes to f64 (`f64::from(frame)`, `f64::from(t) * f64::from(sample_rate)`).
4. Tests — update literal types; add guard test.

Casting style: the repo uses `#[expect(clippy::cast_precision_loss, reason = "...")]` / `f64::from(...)` — follow it; `just lint` is warnings-as-errors.

## Anti-Goals (Out of Scope)

- No changes to decoding, analysis, silence trimming, or the playlist/library DB durations (post-trim duration is desired behavior).
- No changes to the engine crate (`SessionTime`, render, WSOLA, resample) — it already uses exact-position math.
- No changes to `WaveformView`/`drag_view_frame`/`pin_frame` (f32 display domain).
- No changes to `ScrubCommand.speed`, `scrub_state.rs`, or the varispeed UX.
- No heavy real-audio regression test with a 2²⁴+ buffer.
- No seeking features, no playback UX changes of any kind.

## Edge Cases & Gotchas

- **The freeze is silent:** no error, no panic — `pos + 1.0 == pos` forever, same sample emitted. A future revert to f32 would compile cleanly; the guard test is the only tripwire.
- **Don't f32-ify `hermite`'s input:** if `pos` were cast to f32 before indexing, the integer part eats all 24 mantissa bits past 2²⁴ and the fractional part vanishes (frame-quantized output = zipper noise). The floor/index must be computed from the f64 position.
- **`hermite`'s empty-buffer guard** (`if frames == 0 return 0.0`) must survive the type change.
- **End-clamp test tolerance:** `end_of_track_clamps` asserts `core.position() <= (frames - 1) as f32 + 1e-3` — becomes f64 compare with f64 literal.
- **Crossfade math untouched:** `effective_step`/`prev_step`/`fade_remaining` are small-ratio f32s; leave them.
- **`speed` writeback in the callback** (`cmd.speed * source_rate as f32`) is f32 by design (display extrapolation) — do not "fix" it.
- **Workspace lints:** clippy pedantic is warnings-as-errors; every widening cast needs `f64::from` (not `as`) where the source is f32; `as` on f64→f32 needs an `#[expect(clippy::cast_precision_loss, reason = "...")]`.
- **stale DB durations:** if verification compares library DB durations against playback, remember they are silence-trimmed (~2.4% short) on purpose; the waveform editor's duration (`frames/sample_rate`) is the full-decode one.

## Navigation Anchors

- `ScrubCore::read` — `crates/automixah-ui/src/audio/scrub.rs` (the freeze site; primary entry point).
- `hermite` — same file (index-from-f64 requirement).
- `Playhead` + `apply_pending_seek` + `OutputEngine::start` — `crates/automixah-ui/src/audio/output.rs`.
- Follow block + `DragMode::Scrub` drag-end + click-to-seek — `crates/automixah-ui/src/app.rs` (~lines 903–930, 985–995, 1000–1010; grep `position_at_update` and `seek.write()` to locate).

## Dependency Mappings

None. No new crates, features, or module dependencies. All changes are type-width changes within `automixah-ui`.

## Test Strategies

- **New guard test (scrub.rs):** `// Given` a core parked at exactly 2²⁴ frames (16_777_216.0) over a small dummy buffer with `set_speed(1.0)`, `fade_remaining = 0`; `// When` reading one chunk; `// Then` position advanced to exactly 16_777_217.0 (`assert_eq!` against the f64 — exact equality is the point; with f32 storage this is impossible). Follow §4 BDD style: name reads as a sentence, one behavior.
- **Update `paused_seek_updates_position` (output.rs):** literals to f64 (`12_345.0` already infers); assertion `assert_eq!(reported, 12_345.0)` still exact.
- **Existing scrub suite** (`one_x_preserves_frequency`, `half_x_drops_an_octave`, `two_x_rises_an_octave`, `speed_change_crossfades`, `end_of_track_clamps`, `speed_clamps_to_range`): must pass unchanged in behavior — only literal-type updates allowed.
- **Existing output suite** (`rate_fold_*`, `channel_fold_*`, `speed_scale_compensates_rates`, `shape_soft_clips`): untouched by design (RateFolder/fold_channels don't change).
- **Full gates:** `just check`, `just test`, `just lint` (warnings-as-errors).
- **Manual verification:** load each reported track in `automixah-ui`, seek to just before the former freeze point (380 s / 349 s), and confirm playback continues to the track's true end.

## Record Updates (apply at end of implementation, per policy)

- Add to `.agents/RECORD.md`:
  - `- (ui) The scrub playhead is tracked in f64 frames; playback reaches the true end of any track (an f32 position would freeze at 2²⁴ frames ≈ 6.3 min).`
- If implementation diverges from this entry, do not write a wrong entry — surface the divergence in the final summary instead.
