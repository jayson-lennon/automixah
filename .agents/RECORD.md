# The Record

A curated list of factual, scoped statements asserting the application's **current** state. Authoritative for the present, never the future.

The planner consults this file before proposing a plan. If a feature **contradicts** an entry here, the contradiction is surfaced before the plan proceeds. If a feature **establishes a new high-level fact**, a verbatim entry is proposed for human approval as part of the plan.

## Format Rules

- **Factual.** Assert how things are _now_. Never future intent ("we will...", "should..."). Each entry is the current state of the application.
- **Scoped.** Name what each entry applies to — repo, app, frontend, or a named subsystem. An unscoped fact (e.g. "uses Fossil") is ambiguous: is that the repo, or the app's supported VCS list? Always disambiguate.
- **High-level.** One-liners (a few sentences at most). Capture decisions and facts a planner needs, not implementation minutiae.
- **Single tag.** Each entry carries exactly one subsystem tag as a `(tag)` prefix: `- (tools) The bash tool runs...`. One entry, one tag — this keeps tag usage a meaningful coverage metric (a tag growing large signals over-specification or a tag that should split). If you cannot decide between two tags for an entry, that is a signal to **re-evaluate the entry itself**, not to assign both. Use `(tag)` rather than `[tag]` to avoid colliding with markdown task-list (checkbox) syntax.
- **Singular concept.** Each entry should be a single sentence and only concerned with a single concept. Prefer multiple entries versus combining many things into one.

## Templates

| Pattern     | Form                                                             | Example                                                                                 |
| ----------- | ---------------------------------------------------------------- | --------------------------------------------------------------------------------------- |
| State       | `[Scope] currently [does X / is Y].`                             | "The TUI's first screen at startup is the chat screen."                                 |
| Persistence | `[Scope] persists [what] to [where].`                            | "Sessions persist to SQLite."                                                           |
| Flow        | `[Input/event] is handled by [actor/subsystem], which [action].` | "File edits route through the `edit` tool, which validates `LINE#HASH` anchors."        |
| Boundary    | `[Scope] is bounded by [constraint].`                            | "Project discovery walks ancestors until a VCS root or `$HOME`, whichever comes first." |

## Absence

A missing record, or an un-recorded area, simply means the list has no entry there yet. Absence is not a constraint — it is an open question, and a feature that fills a gap may establish the first entry for that area (proposed for human approval as part of the plan).

## Editing

Entries are added or amended **only with human approval**.

---

<!-- Add entries below. Keep them scoped, factual, and high-level. -->

- (analysis) Analysis lives in the shared `djcore` crate (a workspace member at `crates/djcore`, not a separate repository); it wraps stratum-dsp and uses a mono downmix.
- (analysis) Decode, stretch, render, and WAV output are stereo (interleaved).
- (analysis) Beat grids are constant-tempo: one rounded BPM and one phase anchor per track; beats/downbeats/bars arrays are projections of that grid, with the downbeat phase chosen by energy.
- (analysis) The grid BPM uses a Mixxx-style rounding ladder (integer → ½ → ⅓ → 1/12) within regression confidence bounds; near-musical values snap when the bounds admit them.
- (boundary) Fixed-BPM tracks are the only supported input; variable-tempo tracks are out of scope.
- (engine) automixah-engine exposes a mixdown pipeline: a job message (per-track path, canonical grid, key, duration, in playlist order) renders a session to WAV; audio is read and decoded from disk inside the render worker, never re-analyzed.
- (db) The grid library persists to a SQLite database in the XDG data dir, keyed by track content hash, behind a `GridStore` trait with SQLite and in-memory backends and versioned migrations.
- (db) Track analysis persists BPM, key, and the beat grid per content hash; manual grid edits preserve the stored key.
- (db) Playlists persist to the library database as ordered content-hash references with add-time paths; track tags (artist/title/duration) persist keyed by content hash.
- (db) A stored grid (manual override or auto-detected) short-circuits re-analysis on load; a fresh analysis persists its auto grid to the library.
- (engine) Playback is forward-only; seeking is permanently out of scope, and skip moves between transition points.
- (engine) Track order is user-authored; the engine plans transitions between adjacent tracks only.
- (engine) Transitions overlap: the incoming track cues at a grid downbeat at the window start, the window is phase-snapped to the outgoing track's stretched beat grid so both decks' beats coincide during the overlap, and session length reflects the overlap.
- (engine) Time-scaling supports pitch-adjusted resampling (default) and pitch-preserving WSOLA; the default heuristic uses pitch-adjusted for stretches within ±8%.
- (identity) automixah is an experimental auto-DJ application written in Rust, mixing fixed-BPM tracks into a continuous session.
- (mixing) The mixer is driven by an addressed, MIDI-shaped control bus; automations are data timelines of control events behind a `ControlSource` trait.
- (mixing) Automations are authored as RON `TransitionSpec` pairs addressed by deck role (outgoing/incoming); the default is a 16-bar equal-power fade, and `--automation <file>` loads a custom pair applied to every transition.
- (mixing) Default UX is zero-config: playlist → play; target BPM and transitions are auto-selected by rules, user overrides optional.
- (ui) automixah-ui is an egui desktop binary for manual beat-grid alignment: Mixxx-style 3-band Bessel waveform, grid editing (BPM/anchor/downbeat phase), and scrub-audition over cpal.
- (ui) The waveform view pins the playhead at a definable x-position and scrolls the waveform around it; pan clamping allows one screen of over-scroll on each side so track extremes stay previewable.
- (ui) Scrubbing is velocity-driven varispeed: drag speed sets playback speed (pitch follows, vinyl-style), and the audio thread advances position itself so there are no per-frame seek discontinuities.
- (ui) Track loading runs off the UI thread (hash → decode → analyze) with progress stages surfaced in the UI; the re-analyze button drops the record's analysis, deletes the stored grid before reloading, and playlist rows reflect the re-analysis automatically.
- (ui) Grid edits save to the grid library keyed by content hash, with save status surfaced in the UI.
- (ui) automixah-ui's playlist section (bottom panel) lists playlists and their tracks; rows show BPM, Camelot key colored by harmonic distance to the previous row, and duration.
- (ui) Track loading enters through the playlist: clicking a ready playlist row loads the track into the grid editor.
- (ui) Track analysis runs on a single worker draining a priority queue (user-forced highest, playlist above background library); duplicate arrivals deduplicate by content hash with priority escalation.
- (ui) Every library-index refresh (startup hydration and completed scans) derives background analysis jobs for indexed hashes lacking a stored analysis, deduplicated to one job per unique hash with paths joined from roots.
- (ui) All async and threaded work reports back through a single UI event bus; frontend state is mutated only when applying events.
- (ui) UI repaints are scheduled by event-bus sends with a 50 ms debounce; each frame drains events under a 10 ms budget before rendering.
- (ui) Playlist contents load on selection: the view clears and shows a spinner until the load event replaces them; contents are ordered content-hash lists and store rowids never reach the frontend.
- (ui) Frontend track knowledge lives in a single track database (content hash → tags, analysis state); playlist rows are ordered hash references and all row display state derives from the database at render time.
- (ui) Analysis lifecycle events address tracks by content hash; row ids are internal to playlist ordering and never address events.
- (ui) The editor holds a single optional Deck (media, working grid, engine, scrub, view) for the loaded track; decoded PCM/peaks exist only inside the deck and are dropped on load or re-analyze.
- (workflow) This repository uses git; commits go through `just commit '<message>'`, checks through `just check`, `just test`, and `just lint`.
- (analysis) djcore decodes mp3, flac, wav, ogg, aac, opus, and m4a (AAC or ALAC) input via symphonia; opus uses a bundled libopus adapter available on native targets only.
- (engine) Mixdown writes to a `.part` sibling of the output and atomically renames on success; cancel or failure removes the partial file.
- (ui) The playlist panel renders the selected playlist to a user-chosen WAV path: path input, browse (system save dialog), and a render/cancel button enabled only when the playlist has at least two rows, every row is analysis-ready, and a path is set.
- (ui) Render job metadata is snapshotted from the track database at click time; grid edits during a render cannot affect the in-flight job.
- (ui) Audition playback is source-rate agnostic: scrub reads at 1x in source frames, a single RateFolder pass converts to the device rate, and source channels fold to the device channel count.
- (ui) The scrub playhead is tracked in f64 frames; playback reaches the true end of any track (an f32 position would freeze at 2²⁴ frames ≈ 6.3 min).
- (workflow) `just test` runs the fast suite via nextest; slow real-audio tests are `#[ignore]`d and run via `just test-heavy`.
- (ui) Playlist renaming is inline: right-click → Rename swaps the playlist row for an in-place text editor where Enter or click-away commits (empty input reverts), Escape cancels, and duplicate names are rejected inline before the store.
- (ui) The waveform peak track advances visual slots by fractional stride accumulation, so non-44.1 kHz sources render on a true-time timeline (an integer counter vs 48000/441 previously stretched 48 kHz renders ~86 ms/min).
- (ui) Grid beat lines render as thin translucent white and hide when beat spacing falls below ~4 px; white lines (downbeats) thin out by beat stride — every 4th, 8th, 16th… beat — once 4-beat spacing falls below ~50 px, and the zoom slider shows the current beats-per-white-line.
- (ui) Playlist rows color the BPM light red when it deviates more than 8 BPM from the selected playlist's ready-track median.
- (engine) Transitions overlap: the incoming track starts at its selected source in-cue at the transition window start, and a valid outgoing out-cue starts the configured automation window; when cues are absent or invalid, existing grid-derived placement is used.
- (db) Per-track cue points persist in the library database by content hash as source-frame positions, with up to four in-cues and four out-cues, and survive grid re-analysis.
- (engine) Mixdown selects the earliest valid in-cue and earliest valid out-cue by source position; cue slots do not determine rendering precedence.
- (ui) Playlist track reordering is hash-addressed and optimistic: all loaded rows are draggable, upper/lower drop halves select before/after placement, and one FIFO persistence worker serializes writes with sequence-filtered success or rollback events.
- (ui) automixah-ui imports one .m3u file per action into a newly created playlist named from the file stem, suffixing duplicate names with the lowest available (N) suffix.
- (ui) M3U import accepts absolute local file paths, skips non-entry lines and unsupported entries, deduplicates tracks by content hash, preserves valid entry order, and reports imported and skipped counts.
- (db) The library index persists multiple root directories and their indexed files to the library database (path, content hash, tags, duration, mtime, size), behind a `LibraryStore` trait with SQLite and in-memory backends.
- (ui) Library rescans are triggered manually or by adding a root, never at startup; a scan reads each changed file once (hash, tags, duration), skips unchanged files by mtime+size, and prunes removed files.
- (ui) A rescan refreshes a track's stored path when its content hash appears at a new location; playlists reference hashes, so moved files keep rendering and mixing.
- (ui) The bottom panel has four columns: library roots, library entries, playlist entries, and playlists (with New and Import at the top of the playlists column).
- (ui) Adding tracks to a playlist is double-click on a library entry.
- (ui) Library search uses comma-separated fuzzy terms ANDed over title/artist/path with matched-glyph highlighting; the parser emits typed term nodes so field filters (BPM, key) can extend it.
- (ui) Library scan progress streams live: the walk enumerates and the scanner processes files incrementally, the display shows processed/discovered file counts during a scan, and the UI repaints while a scan is running.
- (ui) Library scans split into a blocking-pool walker that streams discovered files to an async classifier (stat/hash/tags via spawn_blocking); progress reports per file as processed/discovered counts.
- (ui) Scan progress is two-counter concurrent: the walker task reports file discovery (`seen`) independently of the classifier's per-file `done` count; the event sender merges both monotonic high-water marks so the wire event and applier stay unchanged.
- (ui) Library scans are single-flight app-wide: a latch on `Services` drops any spawn while one scan runs (a concurrent walker pair previously double-counted scan progress), enforced in `spawn_scan` so every call site is covered.
- (ui) A scan requested while another runs is queued as a follow-up full-library scan instead of dropped, so a folder added mid-scan is always indexed without a manual rescan.
- (ui) Preview playback is a player separate from the grid-editor deck; the two never sound simultaneously — starting a preview pauses the deck, loading a deck stops the preview.
- (ui) Middle-clicking a playlist row or library entry plays the track in the preview player via decode-only load (no hash, analysis, or peaks); playlist single-click keeps loading the editor.
- (ui) An always-visible transport bar spans the window width along the very bottom and scrubs the active preview (click and drag seek, play/pause).
