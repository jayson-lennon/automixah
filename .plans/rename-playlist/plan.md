# Spec — Inline Playlist Rename (`rename-playlist`)

## Problem

Playlist rename is routed through the right-click context menu itself: the `TextEdit` renders *inside* the popup (below the buttons) in `playlist_list`, it never auto-focuses, Enter is checked globally (`ui.input(|i| i.key_pressed(egui::Key::Enter))`) rather than on the field, there is no Esc handling, and clicking away silently discards the edit. The underlying store/action/event path (`RenamePlaylist` → `app.rs::rename_playlist` → store → `PlaylistRenamed` event) is sound — only the view-layer paradigm is broken.

## Solution

Replace the in-menu editor with a true inline editor in the playlist list (left column of the bottom panel): right-click → **Rename** closes the menu and swaps that playlist's `selectable_label` row for a focused, select-all `TextEdit` occupying the row. **Enter commits** (trimmed name), **Esc cancels**, **click-away (focus loss) commits**, **empty/whitespace reverts**, and **duplicate names are rejected inline** (hint shown, editor stays open) before any store call. The store trait, both backends, `PanelAction::RenamePlaylist`, and the `PlaylistRenamed` event path are unchanged. The `PlaylistRenamed` applier additionally re-sorts the list (it is name-ordered).

## Dialectical Outcomes (Why)

1. **Click-away behavior → commit (Finder/VSCode convention).** Chosen by user over cancel: silently destroying typed text on a stray click is the worst failure mode of inline editors. Consequence: Esc is the *only* discard path.
2. **Empty/whitespace Enter → silently revert to old name and close.** Chosen over "refuse to close + hint": the schema (`playlists.name TEXT NOT NULL UNIQUE`) cannot persist empty names, and an accidental Enter shouldn't trap the user in the field.
3. **Duplicate names → pre-check in the view, keep editing, inline hint.** The full name list is already in memory (`state.playlists`); checking is one predicate and avoids the store round-trip + lost typing. Store-level failures (the UNIQUE constraint as safety net) still surface via `CommandFailed` → status line, unchanged.
4. **Editor UX → select-all on open** (type-to-replace), agreed during planning without a question.
5. **Enter handling → `return_key(None)` on the TextEdit** rather than egui's default Enter-gives-up-focus. Rationale: with the default, Enter unfocuses the field, which makes the duplicate-rejection path (keep editing) require stealing focus back. Disabling the built-in return key gives full manual control: the view consumes Enter itself while the field has focus.
6. **Alternative rejected: modal rename dialog.** Heavier, breaks flow, user explicitly wants inline.
7. **Applier re-sort added** (not in the original bug report): `list_playlists` uses `ORDER BY name`, so without re-sort the renamed row sits misplaced until the next reload.

## Relevant Files (Where)

| File | Action |
|---|---|
| `crates/automixah-ui/src/playlist/view.rs` | **Main change.** Rewrite the rename flow in `playlist_list`; add commit/cancel logic; doc-comment updates; new tests (egui harness) |
| `crates/automixah-ui/src/playlist/mod.rs` | Extend `RenameEditor` (`pending_focus`, `hint`); add pure `rename_outcome` helper; re-sort in `PlaylistRenamed` applier; tests |
| `crates/automixah-ui/src/app.rs` | Doc-comment touch-ups only (no logic change) |

No store, schema, bus, or queue files change.

## Key Code Context (What)

**The broken code being replaced** — `view.rs::playlist_list` (left column). Current row loop:

```rust
let response = ui.selectable_label(selected, &name);
if response.clicked() {
    actions.actions.push(PanelAction::SelectPlaylist(id));
}
response.context_menu(|ui| {
    if ui.button("Rename…").clicked() {
        state.rename.begin(id, &name);          // menu NOT closed — bug
    }
    if state.rename.matches(id) {
        ui.text_edit_singleline(&mut state.rename.buffer);   // editor inside the popup — bug
        let submitted =
            ui.button("Apply").clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)); // global Enter — bug
        if submitted && !state.rename.buffer.trim().is_empty() { /* ... */ }
    }
    if ui.button("Delete").clicked() { /* ... */ }
});
```

**`RenameEditor` today** — `playlist/mod.rs` (keep the struct, extend it):

```rust
#[derive(Debug, Default)]
pub struct RenameEditor {
    pub id: Option<i64>,      // playlist being renamed (None when idle)
    pub buffer: String,       // the in-progress name
}

impl RenameEditor {
    pub fn begin(&mut self, id: i64, name: &str) { self.id = Some(id); self.buffer = name.to_owned(); }
    pub fn matches(&self, id: i64) -> bool { self.id == Some(id) }
    pub fn clear(&mut self) { self.id = None; self.buffer.clear(); }
}
```

**`PlaylistState`** (excerpt) — `playlists: Vec<PlaylistSummary>` (name-ordered), `selected: Option<i64>`, `rename: RenameEditor`.

**`PlaylistSummary`** — `playlist/store/mod.rs`: `{ pub id: i64, pub name: String }`.

**Panel action (unchanged shape)** — `view.rs`:

```rust
pub enum PanelAction {
    // ...
    RenamePlaylist { id: i64, name: String },
    // ...
}
```

**Applier today** — `playlist/mod.rs::PlaylistState::apply` (needs re-sort added):

```rust
Event::PlaylistRenamed { id, name } => {
    if let Some(p) = self.playlists.iter_mut().find(|p| p.id == *id) {
        p.name = name.clone();
    }
}
```

**Store ordering contract** — `playlist/store/sqlite.rs::list_playlists`: `SELECT id, name FROM playlists ORDER BY name` (SQLite default BINARY collation = byte order; Rust `String::cmp` matches it). Schema: `playlists.name TEXT NOT NULL UNIQUE` (automixah-schema v3 migration).

**Verified egui 0.32 API facts** (checked against `egui-0.32.3` source):

- `TextEdit::singleline(&mut buf)` builder has `.id(egui::Id)` and `.return_key(impl Into<Option<KeyboardShortcut>>)`; `.return_key(None::<egui::KeyboardShortcut>)` disables the built-in Enter handling so Enter neither commits nor unfocuses — the view reads it via `ctx.input`.
- `egui::widgets::text_edit::TextEditState` is public with `pub cursor: TextCursorState`; `TextEditState::load(ctx, id)` / `.store(ctx, id)`; `cursor.set_char_range(Some(CCursorRange::two(0, n)))` where `n` is the **char count** (not bytes). Seeding state before adding the widget selects all on open.
- `Response::has_focus()`, `Response::lost_focus()`, `Response::request_focus()` exist; `ctx.memory_mut(|m| m.surrender_focus(id))` exists (memory/mod.rs).
- Default singleline `EventFilter` does not lock Escape (`escape: false`), so Escape reaches `ctx.input` while the field is focused.
- egui's canonical commit pattern is `response.lost_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter))` (builder doc comment).
- Tests can drive panels headlessly: `let ctx = egui::Context::default(); ctx.run(raw_input, |ctx| panel(ctx, &mut state))` with `egui::RawInput { events: vec![egui::Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: egui::Modifiers::NONE }], ..Default::default() }`.

## Navigation Anchors

- `crates/automixah-ui/src/playlist/view.rs::playlist_list` — the row loop being rewritten (primary entry point).
- `crates/automixah-ui/src/playlist/view.rs::panel` — unchanged shell; calls `playlist_list`.
- `crates/automixah-ui/src/playlist/mod.rs::RenameEditor` — extended state.
- `crates/automixah-ui/src/playlist/mod.rs::PlaylistState::apply` — `PlaylistRenamed` arm gets the re-sort.
- `crates/automixah-ui/src/app.rs::handle_panel_actions` / `app.rs::rename_playlist` — wiring, unchanged logic.

## Implementation Algorithm (How)

### Editor state model

`RenameEditor` gains two fields:

```rust
pub struct RenameEditor {
    pub id: Option<i64>,
    pub buffer: String,
    /// Set by `begin`; the row render seeds focus + select-all once, then clears it.
    pub pending_focus: bool,
    /// Inline rejection hint shown while editing (`None` normally).
    pub hint: Option<&'static str>,
}
```

- `begin(id, name)`: as today, plus `pending_focus = true; hint = None;`
- `clear()`: as today, plus `pending_focus = false; hint = None;`

### Pure decision helper (`playlist/mod.rs`)

```rust
pub(crate) enum RenameOutcome {
    /// Commit this trimmed name.
    Submit(String),
    /// Empty/whitespace input: close the editor, emit nothing.
    Revert,
    /// Name already owned by another playlist: keep editing, show hint.
    RejectDuplicate,
}

pub(crate) fn rename_outcome(editor: &RenameEditor, playlists: &[PlaylistSummary]) -> RenameOutcome
```

Rules, in order:
1. `let trimmed = editor.buffer.trim()`; if `trimmed.is_empty()` → `Revert`.
2. If `playlists.iter().any(|p| p.id != id && p.name == trimmed)` (case-sensitive, byte-exact — mirrors SQLite BINARY UNIQUE) → `RejectDuplicate`.
3. Else → `Submit(trimmed.to_owned())`. Renaming to the playlist's *own* current name is allowed (no-op commit through the normal path).

`editor.id == None` never reaches this function (only called while editing).

### Row rendering (`playlist_list`)

For each playlist row:

- **Editing row** (`state.rename.matches(id)`):
  1. Stable focus id: `let edit_id = egui::Id::new("playlist_rename_editor");` (single editor at a time → constant is fine).
  2. If `state.rename.pending_focus`: seed select-all — `TextEditState` via load-or-default, `cursor.set_char_range(Some(CCursorRange::two(0, buffer.chars().count())))`, `.store(ui.ctx(), edit_id)`; then `ui.ctx().memory_mut(|m| m.request_focus(edit_id))`; set `pending_focus = false`.
  3. `let response = ui.add(TextEdit::singleline(&mut state.rename.buffer).id(edit_id).return_key(None::<egui::KeyboardShortcut>).desired_width(ui.available_width()));`
  4. Esc: `response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Escape))` → `state.rename.clear()` + `ui.ctx().memory_mut(|m| m.surrender_focus(edit_id))`. No action.
  5. Enter: `response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter))` → evaluate `rename_outcome`:
     - `Submit(name)` → push `PanelAction::RenamePlaylist { id, name }`, `clear()`, surrender focus.
     - `Revert` → `clear()`, surrender focus.
     - `RejectDuplicate` → `state.rename.hint = Some("name already exists")`; keep editing (focus is retained naturally since `return_key(None)` means Enter never unfocused it).
  6. Focus loss (click-away / Tab): `response.lost_focus()` → evaluate `rename_outcome`:
     - `Submit(name)` → push action, `clear()`.
     - `Revert` → `clear()`.
     - `RejectDuplicate` → `clear()` (revert; re-stealing focus would fight the user's click).
  7. Hint: if `hint.is_some()` render `ui.label(egui::RichText::new("name already exists").small().color(egui::Color32::RED))` below the field.
  8. No `selectable_label`, no click-to-select, no context menu on the editing row (right-click during edit keeps the editor).
- **Normal row**: unchanged `selectable_label` + click-to-select, with context menu:
  - "Rename…" → `state.rename.begin(id, &name); ui.close();` (the `ui.close()` is the fix for the menu staying open; the editor row renders on the next frame, which egui repaints to).
  - "Delete" → unchanged.

### Doc/comment updates (same change, per AGENTS.md "fix the document in the same change")

- `view.rs` module doc: "right-click rename-delete" → describe right-click rename with inline in-place editing.
- `PanelAction::RenamePlaylist` doc: "Rename was submitted in a playlist's context menu" → "Rename was submitted by the inline row editor".
- `RenameEditor` doc: drop "for the playlist context menu".
- `app.rs::rename_playlist` doc: "the panel's context menu carries the field" → "the panel's inline editor carries the field".

### Applier re-sort (`PlaylistState::apply`, `PlaylistRenamed` arm)

```rust
Event::PlaylistRenamed { id, name } => {
    if let Some(p) = self.playlists.iter_mut().find(|p| p.id == *id) {
        p.name = name.clone();
    }
    self.playlists.sort_by(|a, b| a.name.cmp(&b.name));
}
```

Matches the store's `ORDER BY name` (byte order both sides).

## Edge Cases & Gotchas

- **Enter does not unfocus by itself** (`return_key(None)`): every Enter path must explicitly `surrender_focus` on submit/revert, or keyboard focus stays on a widget that no longer renders.
- **Esc frame ordering**: Esc clears the editor *before* the focus-loss frame renders; since the editor no longer renders for that id, the subsequent `lost_focus()` frame cannot re-commit. Clearing and surrendering in the same frame is safe.
- **Click-away with a duplicate name reverts** (cannot keep an unfocused editor open without fighting the click). Documented behavior.
- **Tab commits** — Tab isn't focus-locked by singleline TextEdit, so Tab moves focus → `lost_focus()` → commit. Acceptable; consistent with click-away.
- **`CCursor` indices are char counts, not bytes** — use `buffer.chars().count()` for the select-all seed (non-ASCII names).
- **Seed state only once** (`pending_focus`); re-seeding every frame would reset the cursor/selection while typing.
- **Focus id must be stable across frames** — without `.id(edit_id)` egui mints an id from the buffer content and focus is lost on the first keystroke.
- **Case-sensitivity** — duplicate check is byte-exact, matching SQLite's default BINARY collation on the UNIQUE index. "Mix" vs "mix" are distinct names.
- **One frame of latency** — the menu click closes the menu at end of frame; the editor row appears next frame (egui repaints on interaction). Not a bug.
- **Duplicate hint lifetime** — cleared on `begin`, on submit/revert (`clear`), stays while rejected via Enter.
- **Same-name rename** commits through the normal path (harmless no-op UPDATE); explicitly not treated as duplicate.
- **Store-level failures** (e.g. UNIQUE race with a name created after load) still flow through `CommandFailed` → status line; the editor has already closed and the list keeps the old name on failure. Accepted safety net.

## Anti-Goals (Out of Scope)

- No modal/`Window`-based rename dialog.
- No renaming of *tracks*/rows in the right column — playlists only.
- No store, schema, SQL, or migration changes; no bus event changes; no queue changes.
- No `F2` accelerator or double-click-to-rename triggers (right-click menu only, as specified).
- No duplicate-name prevention on playlist *creation*.
- No case-insensitive or trimmed-name uniqueness semantics beyond what's specified.
- No multi-row or simultaneous editors.

## Dependency Mappings

No new external or internal dependencies. Everything used is already a dependency of `automixah-ui`: `egui` 0.32 (already imported in `view.rs`; tests use `egui::Context`, `RawInput`, `Event`, `Key`, `Modifiers`, `Id`, `widgets::text_edit::TextEditState`, `text::CCursorRange` — all reachable from the `egui` crate root / `egui::widgets`). `rstest` available in dev-deps if parameterizing.

## Test Strategies

All tests follow AGENTS.md §4: BDD Given/When/Then comments, one behavior per test, standalone-sentence names.

**Pure helper tests** (`playlist/mod.rs` `#[cfg(test)]`):
- `rename_decision_trims_nonempty_buffer` — non-empty buffer ⇒ `Submit` with trimmed name.
- `rename_decision_whitespace_reverts` — empty and whitespace-only ⇒ `Revert`.
- `rename_duplicate_name_keeps_editing` — trimmed name equal to *another* playlist's name ⇒ `RejectDuplicate`.
- `rename_same_name_is_not_duplicate` — name equal to the edited playlist's own name ⇒ `Submit`.

**Applier test** (`playlist/mod.rs`): `playlist_renamed_resorts_list` — two playlists `["A", "C"]`, rename `C`→`"B"` via `apply(PlaylistRenamed)` ⇒ list order `["A", "B"]`.

**View harness tests** (`view.rs` `#[cfg(test)]`, headless via `egui::Context::run` calling the public `panel(ctx, &mut state)`):
- `inline_editor_enter_commits` — seed two playlists, `rename.begin(id, "old")`; frame 1 runs `panel` (focus seeds); frame 2 runs `panel` with `RawInput` carrying Enter ⇒ exactly one `RenamePlaylist { id, name: "old" }` action (or edited buffer) and `state.rename.matches(id) == false`.
- `inline_editor_escape_cancels` — same setup, frame 2 carries Escape ⇒ no rename action, editor cleared.
- `inline_editor_focus_loss_commits` — same setup, then `ctx.memory_mut(|m| m.surrender_focus(egui::Id::new("playlist_rename_editor")))` between frames; next `panel` run emits the commit action (models click-away).
- Modify the buffer between frames (e.g. `state.rename.buffer.push_str(...)`) in at least one harness test to assert the *typed* name is what commits (trimmed).

**Existing tests to keep green**: `playlist_renamed_updates_list` (still passes — single entry re-sort is a no-op), `store_behavior.rs::rename_playlist_roundtrips`, `just check && just test && just lint` (clippy warnings-as-errors — mind `#[must_use]` on new pure helpers).

## Acceptance Criteria

- Right-click → Rename closes the menu and the row becomes a focused inline field with the current name selected (type-to-replace).
- Enter commits the trimmed name; the store round-trip and event application are unchanged.
- Esc reverts to the old name; empty/whitespace input reverts; clicking outside the field commits non-empty input.
- Renaming to an existing playlist's name keeps the editor open with an inline hint and fires no store call.
- No rename UI remains inside the context menu; the row re-sorts into name order after a successful rename.
- `just check`, `just test`, `just lint` all pass.

## Phases

1. **Editor lifecycle (view)** — Extend `RenameEditor` (`pending_focus`, `hint`); rewrite the rename flow in `playlist_list`: "Rename…" closes the menu via `ui.close()` and calls `begin`; the matching row renders a row-width `TextEdit` (stable id, `return_key(None)`, focus + select-all seeded on open) instead of the label; delete the in-menu editor block. Doc updates for module/`PanelAction`/`app.rs` comments.
2. **Commit rules (view + state)** — Add the pure `rename_outcome` helper in `playlist/mod.rs`; wire Enter (has-focus + Enter key), Esc (has-focus + Escape key), and focus-loss paths per the algorithm; render the duplicate hint.
3. **Applier re-sort** — Sort `playlists` by name after applying `PlaylistRenamed`.
4. **Tests** — Helper tests, applier re-sort test, egui harness tests; run `just check`, `just test`, `just lint`.
5. **Record update** — Write the approved entry (below) at end of implementation.

## Record Updates

Add to `.agents/RECORD.md` (verbatim, at end of implementation, only if implementation matches):

> `- (ui) Playlist renaming is inline: right-click → Rename swaps the playlist row for an in-place text editor where Enter or click-away commits (empty input reverts), Escape cancels, and duplicate names are rejected inline before the store.`

If implementation diverged from this entry, surface the divergence in the final summary instead of writing a wrong entry.
