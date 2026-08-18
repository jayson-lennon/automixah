# automixah-ui Bugfixes & UX — Context-Rich Specification

## Problem
The first hands-on session surfaced five defects: edits never persisted (`library.sqlite` `beat_grids` has 0 rows — the user's gestures never modified the grid, and there is no save feedback), left-drag performs a confusing simultaneous pan+scrub, the anchor slider's post-edit wrap into `[0, bar)` reads as "looping between 0 and 1" with no explanation and too-coarse precision, nudge buttons are unlabeled intent ("nudge what?"), and beat/downbeat lines are near-invisible on a grayscale waveform (the gray base from `band_mix` swamps the band tints). Additionally, track loading (hash → decode → analyze → peaks) runs on the UI thread and freezes the window for seconds.

## Solution
1. **Off-thread loading**: pick a path → hand the whole load (hash, decode, analyze, override lookup, peaks build) to `spawn_blocking` on the existing tokio runtime → UI polls a channel each frame, the status line walks `hashing… → decoding… → analyzing… → ready`, the window stays live.
2. **Unified scrub interaction**: left-drag = scrub only (audio at drag velocity via the existing `ScrubMachine`; view follows the playhead); SHIFT+left-drag = move the grid anchor live (pixels → seconds delta); plain click = seek the playhead; wheel = zoom. Delete the pan-drag branch entirely — with the view following the playhead, scrub subsumes pan.
3. **Mixxx RGB waveform**: stacked columns — lows red, mids green, highs blue (real `band_mix` change, removing the gray-dominant `amplitude * 255.0` base). Beat lines **blue**, downbeat lines **white**, playhead stays yellow but wider (3 px).
4. **Anchor & nudge controls**: slider spans `[0, bar)` with a `bar` suffix, 3-decimal seconds readout, label "anchor (time of first beat within one bar)"; nudges relabeled `shift grid` with `‹100 ‹10 ‹1 · 1› 10› 100›` buttons and tooltips.
5. **Save fix + feedback**: every grid mutation path (slider, DragValue, nudges, shift-drag, snap/downbeat buttons) marks dirty → save flushes → status shows `grid saved 14:32:05` (red `save failed` on error); the stale "500 ms debounce" comment is corrected to match actual immediate flush. Verified by a real row landing in `beat_grids`.

## Dialectical Outcomes (Why)

- **One gesture, one behavior (scrub subsumes pan).** The user asked "what's the difference between scrub and pan? In my opinion there is only scrub." Dialectically they're right for this UI: when the view follows the playhead, dragging audio *is* navigation. The old code ran pan and scrub simultaneously on left-drag (pan consumed `drag_delta` inside `handle_input` while `app.rs` fed the same delta to `ScrubMachine`), producing the "grid moves right / waveform moves left" confusion. Resolution: delete the pan branch; left-drag is scrub only; the view follows the playhead while scrubbing or playing. Long-distance navigation works by scrubbing fast (±8× clamp) or zooming out and scrubbing.
- **SHIFT+left-drag for grid move** (user's explicit request #1). The grid anchor shifts by the pixel→seconds delta of the drag; the wrap invariant `[0, bar)` is preserved. This is a grid mutation and must mark the save path dirty.
- **Mixxx RGB stacked columns, not grayscale + colored lines** (user chose option B for #3). The old `band_mix` added `amplitude * 255.0` to *all three* channels, so a white/gray base dominated and the band tints were invisible at real volumes. Mixxx's scheme draws the low band red, mid green, high blue as stacked segments — kicks become visibly red stripes, which is exactly the visual aid manual alignment needs. Consequently beat lines become **blue** and downbeats **white** (red lines would vanish against red lows), and the playhead stays **yellow** but wider (3 px) per the user's "just make the playhead line wider".
- **Anchor slider = phase within one bar.** The anchor is a phase, not an absolute time: the grid is `anchor + k·beat` for all integers k, so anchor values differing by one bar produce the identical grid. The old slider ranged `0.0..=track_end` (~360 s) then wrapped into `[0, 1.71s)` after every change — the user saw "looping between 0 and 1". Resolution: make the slider honest — range `[0, bar)`, wrap by design, label explains the semantics, 3-decimal readout (user: "we are working near sample levels"; at 44.1 kHz a sample is 0.023 ms, so 1 ms = 0.001 s resolution is the sensible floor for a drag control; finer edits come from the ±1 ms nudge).
- **Nudges relabeled `shift grid`** (user's #5, option A): the user said "the nudge buttons work but I don't know what we are nudging". New label row `shift grid` with `‹100 ‹10 ‹1 · 1› 10› 100›` plus a tooltip stating "moves every beat line left/right by N ms (wraps within one bar)".
- **Save feedback** (user's #6, option A): automatic save + status text. Root cause of the unsaved edit: no gesture in the old UI mutated the grid, so no save was ever scheduled; and saves were silent. Both fixed: gesture-driven grid edits (shift-drag) now mark dirty, and every flush updates the status line.
- **Off-thread loading** (user's addition): `open_pick`/`load` currently runs hash → read → decode → analyze synchronously inside the egui update loop (`app.rs` calls `crate::track::open_pick(&self.services)` inline), freezing the window for the multi-second analysis. The tokio runtime handle already sits in `Services`; `spawn_blocking` is the natural seam. The rfd file dialog stays on the UI thread (it must).

## Relevant Files (Where)

Modified (all under `crates/automixah-ui/src/`):
- `app.rs` — load pipeline wiring (channel polling, status states), gesture rework (click-seek, shift-drag grid move, scrub-only drag), view-follows-playhead, save status feedback, playhead stroke width 3.
- `view/waveform.rs` — delete the pan branch in `handle_input`; `paint_column`/`band_mix` rework to stacked RGB; `center_frame` follow logic promoted from input-handler option to app-driven.
- `view/grid.rs` — `controls()`: anchor slider `[0, bar)` 3-decimal, `shift grid` nudge row + tooltips, line colors (blue beats, white downbeats).
- `track.rs` — split `load` into staged off-thread pieces or a state-emitting loader function; keep the synchronous path for tests.

New:
- `crates/automixah-ui/src/loader.rs` (if a dedicated module is cleaner than extending `track.rs`) — `LoadStage` enum, `spawn_load(services, path) -> Receiver<LoadEvent>`.

Existing tests to update: `view/grid.rs` tests (anchor bounds), `view/waveform.rs` tests (pan branch removal), new tests per Test Strategies.

Reference (read-only):
- `/mnt/zed/repos/third-party/mixxx/src/waveform/waveformrenderbeat.h` + `.cpp` (Mixxx beat/downbeat line colors — informational only; our colors are the user's choice).

## Key Code Context (What)

Current `handle_input` pan branch to DELETE (`view/waveform.rs:96-110`):
```rust
fn handle_input(
    response: &mut Response,
    view: &mut WaveformView,
    rect: Rect,
    center_frame: Option<f32>,
) {
    if response.dragged_by(egui::PointerButton::Primary)
        || response.dragged_by(egui::PointerButton::Secondary)
    {
        view.dragging = true;
        let dx = response.drag_delta().x;
        view.left_frame -= dx * view.frames_per_pixel;
        return;
    }
    view.dragging = false;

    if let Some(pos) = response.hover_pos() {
        let anchor_px = pos.x - rect.left();
        let wheel = response.ctx.input(|i| i.raw_scroll_delta.y);
        if wheel != 0.0 {
            view.zoom_at((wheel / 200.0).exp(), anchor_px);
        }
    }

    if let Some(frame) = center_frame {
        let visible = view.visible_frames(rect.width());
        view.left_frame = frame - visible / 2.0;
    }
}
```
Keep wheel-zoom and the `center_frame` follow (the follow becomes the app's standard behavior while scrubbing/playing; `WaveformView.dragging` field can be deleted with the pan branch if nothing else consumes it — verify with grep).

Current `band_mix` gray base to REPLACE (`view/waveform.rs:180-191`):
```rust
fn band_mix(q: &PeakQuartet, amplitude: f32) -> Color32 {
    let ch = |band: u8, tint: u8| f32::from(band) * f32::from(tint) / (255.0 * 255.0);
    let r =
        amplitude * 255.0 + ch(q.low, LOW_RGB.0) + ch(q.mid, MID_RGB.0) + ch(q.high, HIGH_RGB.0);
    let g =
        amplitude * 255.0 + ch(q.low, LOW_RGB.1) + ch(q.mid, MID_RGB.1) + ch(q.high, HIGH_RGB.1);
    let b =
        amplitude * 255.0 + ch(q.low, LOW_RGB.2) + ch(q.mid, MID_RGB.2) + ch(q.high, HIGH_RGB.2);
    #[expect(clippy::cast_possible_truncation, reason = "clamped to 255")]
    let byte = |v: f32| v.clamp(0.0, 255.0) as u8;
    Color32::from_rgb(byte(r), byte(g), byte(b))
}
```
Replace with stacked painting in `paint_column`: divide the half-height into three stacked segments — lows occupy the bottom (red), mids middle (green), highs top (blue) — each segment's height proportional to its band value / 255 (Mixxx draws all three bands across the full column with per-band alpha; the stacked layout is our simplification and reads clearly). If `PeakQuartet.all` is still needed for the `q.all == 0` early-out, keep it.

Current `paint_column` signature (`view/waveform.rs:157+`):
```rust
fn paint_column(painter: &Painter, x: f32, center_y: f32, half_h: f32, q: &PeakQuartet) {
    if q.all == 0 {
        return;
    }
    let amplitude = f32::from(q.all) / 255.0;
    let len = (half_h * amplitude).max(1.0);
    let color = band_mix(q, amplitude);
    painter.rect_filled(
        Rect::from_min_size(
            Pos2::new(x - 0.5, center_y - len),
            Vec2::new(1.0, len * 2.0),
        ),
        0.0,
        color,
    );
}
```

Current `controls()` anchor/nudge sections to REPLACE (`view/grid.rs:63-101`, full body in Key Context of the prior task; anchor slider `0.0..=track_end` with `rem_euclid(bar_seconds())` wrap at fn end; nudge row labeled "nudge" with `−100…+100` buttons).

New anchor section:
```rust
ui.horizontal(|ui| {
    ui.label("anchor").on_hover_text(
        "time of the first beat within one bar (the grid repeats every bar)",
    );
    let bar = grid.bar_seconds();
    ui.add_enabled(
        bar > 0.0,
        egui::Slider::new(&mut grid.anchor_seconds, 0.0..=bar)
            .suffix(" s")
            .custom_formatter(|n, _| format!("{n:.3}")),
    );
});
```
(The `..=bar` inclusive endpoint lets the user slide to exactly one bar, which the existing wrap folds to 0 — visually a wrap, semantically correct.)

New nudge section:
```rust
ui.horizontal(|ui| {
    ui.label("shift grid").on_hover_text(
        "moves every beat line left (‹) or right (›) by N milliseconds; wraps within one bar",
    );
    for (label, ms) in [
        ("‹100", -100.0), ("‹10", -10.0), ("‹1", -1.0),
        ("1›", 1.0), ("10›", 10.0), ("100›", 100.0),
    ] {
        if ui.small_button(label).clicked() {
            grid.anchor_seconds = (grid.anchor_seconds + ms / 1000.0).max(0.0);
        }
    }
});
```

Line colors (`view/grid.rs:9-11`) — replace:
```rust
const BEAT_COLOR: Color32 = Color32::from_rgba_premultiplied(220, 220, 220, 40);   // → blue
const DOWNBEAT_COLOR: Color32 = Color32::from_rgba_premultiplied(255, 170, 0, 110); // → white
```
with opaque-enough values, e.g. `BEAT_COLOR = from_rgba_premultiplied(80, 140, 255, 150)`, `DOWNBEAT_COLOR = from_rgba_premultiplied(255, 255, 255, 190)`. Playhead stroke (`app.rs:267`): `egui::Stroke::new(2.0, …)` → `egui::Stroke::new(3.0, egui::Color32::from_rgb(255, 210, 60))`.

Current synchronous load path (`track.rs:98-140`, called inline from `app.rs`):
```rust
pub fn load(
    path: &Path,
    services: &Services,
    registry: &DecoderRegistry,
) -> Result<LoadedTrack, Report<TrackLoadError>> {
    let hash = TrackHash(hash_file(path)?);
    let bytes = std::fs::read(path)…;
    let extension = …;
    let audio = registry.decode(&bytes, &extension)…;
    let AnalyzerOutput { beat_grid: auto_grid, .. } = analyze(&audio)?;
    let (grid, grid_source) = match stored_override(services, &hash) { … };
    …
}
```

Current save path (`app.rs:71-96`): `schedule_save()` sets `pending_save = Some((hash, edit_grid))`; `flush_save_if_due()` takes it, builds a `GridOverride`, `services.handle.spawn(async move { store.put(&hash, &grid).await })`, errors only `eprintln!`s. Rework: `flush_save_if_due` reports success/failure back through a shared status string (set from the main thread after the spawn completes via a oneshot channel polled next frame, or simplest: an `Arc<Mutex<Option<Result<(), String>>>>` the app checks — pick the channel approach to stay idiomatic).

Services container (`services.rs`) already holds `handle: Handle` and `grid_store: GridStoreService` — no changes needed there.

## Implementation Algorithm (How)

### Phase 1 — Off-thread loading
1. Define in `track.rs` (or new `loader.rs`):
   ```rust
   pub enum LoadEvent {
       Stage(LoadStage),
       Done(Box<Result<LoadedTrack, String>>), // report rendered to string off-thread
   }
   pub enum LoadStage { Hashing, Decoding, Analyzing, Ready }
   ```
2. `pub fn spawn_load(services: &Services, registry: &DecoderRegistry, path: PathBuf) -> std::sync::mpsc::Receiver<LoadEvent>`:
   - send `Stage(Hashing)`; hash file bytes.
   - send `Stage(Decoding)`; decode via registry (registry is `Clone` or wrap in `Arc` — check its definition; `DecoderRegistry::with_symphonia()` constructs fresh, so build it inside the blocking task).
   - send `Stage(Analyzing)`; `analyze(&audio)`, override lookup via `services.grid_store` + `services.handle.block_on` is NOT allowed inside spawn_blocking — instead do the store lookup with `tokio::task::block_in_place`? Simpler: perform override lookup on the main thread after `Done` arrives, using the existing `stored_override` (it already `block_on`s the runtime handle — acceptable on the UI thread for a single SQLite point-read).
   - build peaks inside the blocking task (pure CPU).
   - send `Done(Ok((track, peaks)))` or `Done(Err(report-string))`.
3. `app.rs`: replace the inline `open_pick` result handling with: rfd dialog (UI thread) → `spawn_load` → store receiver in `UiState` field `loading: Option<Receiver<LoadEvent>>`. Each `update()`: drain the receiver non-blockingly (`try_recv`); on `Stage(s)` set `status = "hashing…"/"decoding…"/"analyzing…"`; on `Done` apply the old apply-track logic (set track/peaks/grid/engine/pending_save=None) and `status = ready` string.
4. While `loading.is_some()`, disable the Open button and show a spinner (`ui.spinner()`).
5. Keep the synchronous `track::load` intact for the existing tests.

### Phase 2 — Gesture rework
1. `view/waveform.rs::handle_input`: delete the pan branch (and `view.dragging` if unconsumed elsewhere — grep first). Keep wheel-zoom.
2. `app.rs` central-panel waveform block:
   - `show(ui, peaks, &mut self.view, follow_frame)` where `follow_frame = Some(playhead_frame)` while scrub-dragging or playing; `None` when paused-and-not-dragging (so the user can still scroll freely while paused… but there is no scroll gesture anymore — when paused, the view simply stays where the playhead last was; zoom-at-cursor still works). Simplest correct rule: always pass `Some(playhead_frame)` when the engine exists; zoom anchor uses the cursor. Decide: follow whenever `engine.is_some()` — view always tracks the playhead. (If the user wants to inspect a far section while paused, they click to seek there first — coherent with "only scrub".)
   - Primary drag without SHIFT → existing `scrub.drag_start/drag_move/drag_end` path (unchanged).
   - Primary drag WITH SHIFT (`response.dragged_by` + `ctx.input(|i| i.modifiers.shift)`) → grid move: `delta_px = response.drag_delta().x; delta_s = delta_px * seconds_per_pixel; edit_grid.anchor_seconds += delta_s` then wrap `rem_euclid(bar_seconds)`; `schedule_save()`. Do NOT feed the scrub machine during shift-drag. Also visually: while shift-dragging, the whole grid overlay shifts live (already true — painting reads `edit_grid` each frame).
   - Click (not drag): `response.clicked()` → seek playhead to `pointer_time` (`*engine.playhead().seek.write() = Some(frame)`), do not touch the grid.
   - The old code sets `self.cursor_time` from hover every frame — keep for the snap/downbeat buttons.
3. Remove any now-dead `drag_last_seconds` handling differences: shift-drag should suppress the scrub path entirely (one gesture, one behavior).

### Phase 3 — RGB waveform + line colors
1. `paint_column` → stacked: compute `h_low = half_h * (q.low / 255)`, `h_mid`, `h_high`; draw three rects centered on `center_y`: lows `[center_y - h_low, center_y + h_low]` red, mids same vertical span green overlaid with alpha (Mixxx-style overlay) OR stacked bottom-to-top (spec decision below); recommended: **stacked segments**: bottom third of the column height budget goes to lows, next to mids, top to highs, each segment's filled height proportional to its value. Concretely: `seg = half_h / 3.0`; lows occupy `[center_y + seg*0 .. center_y + seg*1]` (lower), mids `[center_y .. center_y + seg*2]`?? — simplest unambiguous scheme, adopted here: draw from the center line outward in three stacked bands:
   - lows: rect from `center_y` to `center_y + h_low` (below center), red;
   - mids: rect from `center_y - h_mid` to `center_y` (above center), green;
   - highs: outermost overlay `±h_high` thin outline or alpha-blue rect over both.
   Choose ONE scheme and keep `paint_column` ≤ 20 lines with a helper per band; unit-test the color mapping of `band_mix`'s replacement (pure function returning `(Color32, f32)` per band).
2. Replace `BEAT_COLOR`/`DOWNBEAT_COLOR` constants; playhead stroke → 3.0 width.
3. Update `waveform.rs` widget tests that reference pan behavior; add `paint_column`-color unit tests via the extracted pure helper.

### Phase 4 — Anchor & nudge controls
As in Key Code Context. Ensure `controls()` still returns "changed" by value comparison so `app.rs` `schedule_save()` fires on every mutation. Add unit tests: slider clamp/wrap, nudge labels still mutate by the right ms, 3-decimal formatting (test the formatter closure indirectly by asserting `anchor_seconds` bounds only — egui formatter internals are not worth unit-testing).

### Phase 5 — Save status + verification
1. `flush_save_if_due`: replace `eprintln!` with a completion channel: create `std::sync::mpsc::channel` per flush (or one persistent `Receiver<SaveResult>` created at app construction and an `Arc` sender clone list — simplest: a single `(Sender<SaveOutcome>, Receiver<SaveOutcome>)` stored in the app; the spawned task sends `SaveOutcome { at: SystemTime, result: Result<(), String> }`); `update()` drains it and sets `status = format!("grid saved {}", time)` or a red-flagged `save failed: …` (egui `ui.colored_label` not needed in the status string itself — prefix `⚠ save failed:`).
2. Fix the `schedule_save`/`flush` comments (drop "500 ms debounce" wording; state immediate flush).
3. Verify: run the app logic path in an integration test with the in-memory store (existing `test_services_with_runtime` helper) — mutate grid through `controls()`-equivalent calls and assert exactly one `put` per mutation (wrap store in a counting fake if needed — an `Arc<InMemoryGridStore>` with an atomic counter via a small `CountingStore` wrapper implementing `GridStore`).
4. Full workspace suite + `just lint`; manual pass (user does T8).

## Anti-Goals (Out of Scope)
- No metronome/click track, no playback position automation beyond existing scrub.
- No pan gesture resurrection, no secondary/middle-button behaviors.
- No CLI changes, no engine/djcore changes, no schema migration (v1 `beat_grids` unchanged).
- No Mixxx cue-point import (deferred).
- No exact-Mixxx waveform GL renderer (we keep the CPU painter; only the color scheme changes).
- No persistence of view state (zoom/pan position) across sessions.

## Edge Cases & Gotchas
- **SHIFT+drag vs scrub double-fire**: while shift is held, `response.dragged_by(Primary)` is still true — the app must branch on `modifiers.shift` BEFORE feeding `ScrubMachine`, and must not both shift the grid and scrub. Also handle the case where the user presses SHIFT mid-drag: simplest is to evaluate shift per-frame (grid moves only on shift-frames; scrub machine is paused but not reset — acceptable) or lock the mode at `drag_started` (cleaner: capture `mode = shift?` on `drag_started_by`, apply to the whole drag).
- **Anchor wrap on negative nudge**: `(anchor + ms/1000).max(0.0)` then `rem_euclid` — `.max(0.0)` before wrap is redundant but harmless; from_euclid handles negatives correctly, keep as-is.
- **Inclusive slider endpoint `..=bar`**: user can park at exactly `bar`; wrap folds to 0. Fine, but the displayed value must use the formatter (3 decimals) so `1.717 s` doesn't read as a bug.
- **Follow-playhead vs zoom anchor**: `center_frame` re-centers every frame while following; wheel-zoom at cursor fights the re-center. Resolve: apply zoom AFTER the follow re-center within the same frame (order in `show()`: clamp → follow → zoom → clamp), so the zoom anchor pixel is meaningful. Current `show()` order is clamp → handle_input(zoom, then center overwrite) → clamp; verify the final ordering keeps zoom stable (the center overwrite after zoom effectively discards the anchor — acceptable because the view is playhead-centered anyway).
- **Load cancellation**: user clicks Open while a load is in flight → currently a second receiver would overwrite the first; guard with the `loading.is_some()` disable from Phase 1. Dropping the receiver cancels nothing (task runs to completion) — harmless, the result is discarded.
- **rfd dialog on UI thread** is required; do NOT move it into spawn_blocking.
- **Report → String off-thread**: `Done` carries `Result<LoadedTrack, String>` because `Report<TrackLoadError>` isn't `Send` in general; render with `{report:#}` inside the task.
- **Peaks build memory**: built on the blocking thread and moved; `Peaks` is plain data (`Vec<PeakQuartet>` + stride) — Send, fine.
- **`DecoderRegistry` construction**: build inside the blocking task via `with_symphonia()`; do not share across threads unless verified Send+Sync.
- **Zero-beat grids**: `bar_seconds()` divides by `grid_bpm.max(0.01)`; anchor slider disabled when `bar <= 0` guard already exists as `track_end > 0.0` — replace with `bar > 0.0` check.
- **DB has WAL files present** (`library.sqlite-wal`, `-shm`): the row-count check for verification must use the sqlite3 CLI or a fresh connection, not a stale cached handle.

## Navigation Anchors
- `crates/automixah-ui/src/app.rs` — `AutomixahUiApp::update` (gesture + status + polling wiring), `schedule_save`, `flush_save_if_due`, `start_engine`.
- `crates/automixah-ui/src/view/waveform.rs` — `show`, `handle_input`, `paint_column`, `band_mix`.
- `crates/automixah-ui/src/view/grid.rs` — `controls`, `paint` (line colors).
- `crates/automixah-ui/src/track.rs` — `load` (keep), `open_pick`, new `spawn_load`/`LoadEvent`.
- `crates/automixah-ui/src/services.rs` — `Services` (no change; `handle` + `grid_store` already present).

## Dependency Mappings
No new external crates. Internal: `tokio` (spawn_blocking — already a dependency), `std::sync::mpsc`, existing `djcore`/`automixah-engine`/`stratum-dsp`/`automixah-schema` usage unchanged.

## Test Strategies
- T1: extend `view/grid.rs` tests — `controls`-equivalent direct mutations: set anchor past bar → wrap invariant `0 <= a < bar` (assert after calling the same wrap logic); formatter coverage skipped (egui internal).
- T2: new unit test for the shift-drag math: extract `apply_grid_shift(grid: &mut EditableGrid, delta_seconds: f32)` in `view/grid.rs` or `app.rs`; test wraps correctly and is the only mutation path.
- T3: `view/waveform.rs` tests — delete/adjust pan tests; keep zoom tests; assert `handle_input` no longer mutates `left_frame` on drag (compile-time: field may go private/removed).
- T4: app-level: click handler test not practical (egui); cover by extracting `seek_to(engine, time)` and testing the seek write. The "grid unchanged" half is covered by T2/T6 separation.
- T5: `band_mix` replacement: pure fn `band_colors(q: &PeakQuartet) -> [Color32; 3]` (or heights) — unit test with synthetic quartets (low-only → red segment nonzero, others zero, etc.), plus `q.all == 0` early-out behavior preserved.
- T6: `CountingStore` wrapper (Arc atomics) around `InMemoryGridStore`; integration test drives grid mutations and asserts put-count == mutation-count; also assert `beat_grids` row exists via `SqliteGridStore` on a temp dir (already-covered helpers).
- T7: `spawn_load` unit/integration: temp WAV → spawn → collect events → assert stage order `[Hashing, Decoding, Analyzing, Ready|Done]` and a `LoadedTrack` with expected duration (reuse `wav_bytes` helper in `track.rs` tests).
- T8: manual (user): load reference OGG (UI responsive), shift-drag grid, watch `grid saved HH:MM:SS`, quit, `sqlite3 ~/.local/share/automixah/library.sqlite 'select count(*) from beat_grids'` ≥ 1, reopen track → grid restored.

## Phases
1. **Off-thread loading** — `LoadEvent`/`LoadStage`, `spawn_load` on `spawn_blocking`, app polls receiver, status walks stages, Open disabled while loading, spinner. Tests: T7.
2. **Gesture rework** — delete pan branch; scrub-only left-drag; SHIFT+left-drag grid shift (`apply_grid_shift` helper + wrap); click-seek; mode locked at drag start; view follows playhead whenever the engine exists. Tests: T2, T3, T4.
3. **RGB waveform + line colors** — stacked band painting replacing `band_mix` gray base; blue beats, white downbeats, 3 px yellow playhead. Tests: T5.
4. **Anchor & nudge controls** — `[0, bar)` slider, 3-decimal formatter, `bar`-semantics label+tooltip; `shift grid` nudge row with tooltips. Tests: T1.
5. **Save status & verification** — completion channel → status line `grid saved <time>` / `⚠ save failed: …`; comment fix; CountingStore integration test (T6); workspace suite + `just lint`; manual pass notes for T8.

## Acceptance Criteria
- Opening a track never freezes the UI; status walks through load states; waveform+grid appear when ready.
- Left-drag scrubs audio (speed follows drag, pitch follows speed) and the view tracks the playhead; SHIFT+drag slides grid lines in real time; click seeks the playhead; no gesture both pans and scrubs.
- Waveform renders red/green/blue bands; beat lines blue; downbeats white; playhead a wide yellow line — distinguishable at a glance.
- Anchor slider is bounded `[0, bar)`, 3-decimal seconds, wrap-by-design (labeled); nudges are labeled `shift grid` with tooltips.
- A grid edit produces a row in `library.sqlite` `beat_grids` (verifiable via sqlite3); status line confirms each save.

## Record Updates
None — no recorded facts change (scrub-audition, SQLite-persistence, and constant-grid entries remain accurate; gesture details are below record granularity).
