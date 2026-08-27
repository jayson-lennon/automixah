//! egui rendering for the playlist section (bottom panel).
//!
//! The panel is a full-width, user-resizable strip below the waveform:
//! the left column lists playlists (select / ＋ new / right-click
//! rename-delete; rename swaps the row for an in-place editor), the
//! right column shows the selected playlist's contents. All display
//! state is **derived**: contents are ordered content hashes, and
//! every visible fact (glyph, metadata, clickability) is a pure
//! function of the track database record for that hash — there is no
//! per-row state to mutate. The key text is colored by
//! [`harmonic_color`] against the previous ready row's key (the
//! DJ-adjacency view — reordering instantly recolors).
//!
//! All state lives on [`crate::playlist::PlaylistState`] and
//! [`crate::tracks::Tracks`]; this module only paints and emits user
//! intents back through [`PanelActions`].

use egui::Color32;

use crate::playlist::{Contents, PlaylistState, RenameOutcome, rename_outcome};
use crate::tracks::{AnalysisState, Tracks};
use automixah_engine::timeline::types::TrackHash;

/// User intents emitted by the panel (the app wires them to stores).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PanelAction {
    /// A playlist was clicked (selection change).
    SelectPlaylist(i64),
    /// The ＋ button was clicked.
    NewPlaylist,
    /// Rename was submitted by the inline row editor.
    RenamePlaylist {
        /// Playlist to rename.
        id: i64,
        /// Submitted name (non-empty; the editor reverts empties).
        name: String,
    },
    /// Delete was chosen in a playlist's context menu.
    DeletePlaylist(i64),
    /// The Import button was clicked (the app opens the single-file M3U dialog).
    ImportPlaylist,
    /// A ready row was clicked (load into the grid editor).
    LoadRow(TrackHash),
    /// A row was middle-clicked (play it in the instant preview player).
    PreviewRow(TrackHash),
    /// A row was dragged onto another row's slot (reorder).
    MoveRow {
        /// Row being dragged.
        from: TrackHash,
        /// Slot the drag ended on.
        to: TrackHash,
        /// `true` when the drop was in the lower half of the target slot.
        insert_after: bool,
    },
    /// Remove was chosen in a row's context menu.
    RemoveRow {
        /// Row to remove.
        hash: TrackHash,
    },
    /// The Browse button was clicked (system save dialog for the
    /// mixdown output path).
    BrowseRenderOut,
    /// The Render button was clicked (start the mixdown).
    Render,
    /// The Cancel button was clicked (cancel the in-flight mixdown).
    CancelRender,
}

/// Render-controls view state passed in by the app (owned fields
/// borrowed for the paint).
pub struct RenderUiState<'a> {
    /// Free-text output path buffer.
    pub mix_path: &'a mut String,
    /// `true` while a mixdown is in flight (Cancel mode).
    pub running: bool,
    /// Derived: render button enablement when idle.
    pub can_render: bool,
    /// Latest stage to display (spinner text) while running.
    pub stage: Option<crate::bus::RenderStage>,

    /// Target BPM for mix.
    pub bpm: &'a mut f32,
}

/// Intents from the whole bottom panel: playlist-side plus
/// library-side in paint order.
#[derive(Debug, Default)]
pub struct PanelActions {
    /// Playlist-side intents in paint order.
    pub actions: Vec<PanelAction>,
    /// Library-side intents in paint order.
    pub library: crate::library::view::LibraryActions,
}

/// Draws the whole bottom panel (four columns: library roots, library
/// entries, playlist entries, playlists). Returns the intents this
/// frame collected in emission order.
pub fn panel(
    ctx: &egui::Context,
    state: &mut PlaylistState,
    tracks: &Tracks,
    library: &mut crate::library::LibraryState,
    library_filter: &mut String,
    library_sort: &mut crate::library::sort::SortState,
    render: RenderUiState<'_>,
) -> PanelActions {
    let mut actions = PanelActions::default();
    egui::TopBottomPanel::bottom("playlist_panel")
        .resizable(true)
        .default_height(220.0)
        .show(ctx, |ui| {
            render_controls(ui, render, &mut actions);
            ui.separator();
            four_columns(
                ui,
                state,
                tracks,
                library,
                library_filter,
                library_sort,
                &mut actions,
            );
        });
    actions
}

/// Test-only panel-position capture: records where each named panel
/// landed this frame so layout tests can assert against drift. Compiled
/// out of non-test builds entirely.
#[cfg(test)]
pub(crate) mod panel_capture {
    use std::cell::RefCell;

    thread_local! {
        static RECTS: RefCell<Vec<(&'static str, egui::Rect)>> =
            const { RefCell::new(Vec::new()) };
    }

    pub fn record(name: &'static str, rect: egui::Rect) {
        RECTS.with(|r| r.borrow_mut().push((name, rect)));
    }

    /// Clears the buffer and returns nothing; call before a frame batch.
    pub fn reset() {
        RECTS.with(|r| r.borrow_mut().clear());
        ROWS.with(|r| r.borrow_mut().clear());
    }

    /// Latest recorded rect for `name`.
    pub fn latest(name: &str) -> Option<egui::Rect> {
        RECTS.with(|r| {
            r.borrow()
                .iter()
                .rev()
                .find(|(n, _)| *n == name)
                .map(|(_, rect)| *rect)
        })
    }

    thread_local! {
        static ROWS: RefCell<Vec<egui::Rect>> = const { RefCell::new(Vec::new()) };
    }

    /// Records one laid-out row slot, in paint order.
    pub fn record_row(rect: egui::Rect) {
        ROWS.with(|r| r.borrow_mut().push(rect));
    }

    /// Every row slot laid out during the last captured frame batch,
    /// ordered top-to-bottom.
    pub fn rows() -> Vec<egui::Rect> {
        ROWS.with(|r| r.borrow().clone())
    }
}

/// The four content columns under the render controls.
///
/// The four content columns under the render controls: two imposed
/// rectangles — the library half and the playlist half — separated by
/// a draggable divider. Each half is an egui child `Ui` opened with a
/// fixed `max_rect`; children are clipped to it and cannot influence
/// any other rect, because geometry flows top-down from pure math in
/// [`super::layout`] with no content measurement feeding back.
fn four_columns(
    ui: &mut egui::Ui,
    state: &mut PlaylistState,
    tracks: &Tracks,
    library: &mut crate::library::LibraryState,
    library_filter: &mut String,
    library_sort: &mut crate::library::sort::SortState,
    actions: &mut PanelActions,
) {
    let row = ui.available_rect_before_wrap();
    let fraction = ui
        .ctx()
        .data_mut(|d| d.get_persisted::<f32>(egui::Id::new(LIBRARY_FRACTION_KEY)))
        .unwrap_or(crate::playlist::layout::DEFAULT_LIBRARY_FRACTION);
    let rects = crate::playlist::layout::split_row(row, fraction);

    // The divider: a slim grip whose drag rewrites the stored fraction.
    let divider_response = ui
        .interact(
            rects.divider,
            egui::Id::new("library_playlist_divider"),
            egui::Sense::drag(),
        )
        .on_hover_cursor(egui::CursorIcon::ResizeHorizontal);
    if divider_response.dragged() {
        // Fraction straight from the pointer's absolute position: no
        // delta accumulation, so vertical-only drags change nothing and
        // no per-frame feedback term can drift the split.
        if let Some(pointer) = ui.ctx().pointer_latest_pos() {
            let new_fraction = (pointer.x - row.left()) / row.width();
            store_fraction(ui.ctx(), new_fraction);
        }
    }
    if divider_response.double_clicked() {
        // Double-click resets to the even split.
        store_fraction(ui.ctx(), crate::playlist::layout::DEFAULT_LIBRARY_FRACTION);
    }
    {
        let painter = ui.painter();
        let grip = egui::Rect::from_center_size(
            rects.divider.center(),
            egui::vec2(2.0, rects.divider.height() * 0.9),
        );
        let color = if divider_response.hovered() || divider_response.dragged() {
            egui::Color32::from_gray(140)
        } else {
            egui::Color32::from_gray(60)
        };
        painter.rect_filled(grip, 1.0, color);
    }

    let library_side = super::layout::LibraryWidths::from_side(rects.library.width());
    open_half(ui, "library", rects.library, |ui| {
        let roots = egui::Rect::from_min_size(
            rects.library.min,
            egui::vec2(library_side.roots, rects.library.height()),
        );
        let entries = egui::Rect::from_min_size(
            roots.right_top(),
            egui::vec2(library_side.entries, rects.library.height()),
        );
        let clip = rects.library;
        child_column(ui, clip, "roots", roots, |ui| {
            crate::library::view::roots_column(ui, library, &mut actions.library);
        });
        child_column(ui, clip, "entries", entries, |ui| {
            crate::library::view::entries_column(
                ui,
                library,
                Some(tracks),
                library_filter,
                library_sort,
                state.selected_rows().map(<[TrackHash]>::to_vec).as_deref(),
                &mut actions.library,
            );
        });
    });
    let playlist_side = super::layout::PlaylistWidths::from_side(rects.playlist.width());
    open_half(ui, "playlists_half", rects.playlist, |ui| {
        let tracks_col = egui::Rect::from_min_size(
            rects.playlist.min,
            egui::vec2(playlist_side.tracks, rects.playlist.height()),
        );
        let playlists_col = egui::Rect::from_min_size(
            tracks_col.right_top(),
            egui::vec2(playlist_side.playlists, rects.playlist.height()),
        );
        let clip = rects.playlist;
        child_column(ui, clip, "playlist_tracks", tracks_col, |ui| {
            playlist_tracks_column(ui, state, tracks, actions);
        });
        child_column(ui, clip, "playlists", playlists_col, |ui| {
            playlists_column(ui, state, actions);
        });
    });
}

const LIBRARY_FRACTION_KEY: &str = "playlist_library_fraction";

fn store_fraction(ctx: &egui::Context, fraction: f32) {
    ctx.data_mut(|d| d.insert_persisted(egui::Id::new(LIBRARY_FRACTION_KEY), fraction));
}

/// Opens one half as its own clipped universe: fixed `max_rect`, no
/// escape hatch. Content painting outside is cut at the boundary;
/// content *measuring* outside changes nothing elsewhere by
/// construction.
fn open_half(
    ui: &mut egui::Ui,
    name: &'static str,
    rect: egui::Rect,
    content: impl FnOnce(&mut egui::Ui),
) {
    ui.scope_builder(
        egui::UiBuilder::new().id_salt(name).max_rect(rect),
        |child| {
            child.set_clip_rect(rect);
            content(child);
        },
    );
}

/// Lays out one fixed-width column inside its half at an explicitly
/// given rectangle. Rectangles are pre-computed by the caller; no
/// cursor state participates, so column placement can never depend on
/// what a previous column contained.
fn child_column(
    ui: &mut egui::Ui,
    parent_clip: egui::Rect,
    name: &'static str,
    rect: egui::Rect,
    content: impl FnOnce(&mut egui::Ui),
) {
    #[cfg(test)]
    panel_capture::record(name, rect);
    // Paint on the half's layer but clip to the exact column slice, so
    // even unclipped widget internals stay inside their own column.
    ui.scope_builder(
        egui::UiBuilder::new().id_salt(name).max_rect(rect),
        |child| {
            child.set_clip_rect(rect.intersect(parent_clip));
            content(child);
        },
    );
}

/// The playlist-entries column: header (selection name + add spinner)
/// and the track rows.
fn playlist_tracks_column(
    ui: &mut egui::Ui,
    state: &mut PlaylistState,
    tracks: &Tracks,
    actions: &mut PanelActions,
) {
    ui.horizontal(|ui| {
        let name = state
            .selected
            .and_then(|id| state.playlists.iter().find(|p| p.id == id))
            .map_or("", |p| p.name.as_str());
        ui.strong(name);
        if state.adds_in_flight > 0 {
            ui.spinner();
        }
    });
    ui.separator();
    rows(ui, state, tracks, actions);
}

/// The playlists column: New/Import controls on top, the playlist list
/// below.
fn playlists_column(ui: &mut egui::Ui, state: &mut PlaylistState, actions: &mut PanelActions) {
    ui.horizontal(|ui| {
        ui.strong("Playlists");
        if ui.button("New").clicked() {
            actions.actions.push(PanelAction::NewPlaylist);
        }
        let import = ui.add_enabled(!state.import.busy, egui::Button::new("Import"));
        if import.clicked() {
            actions.actions.push(PanelAction::ImportPlaylist);
        }
    });
    if state.import.busy {
        ui.horizontal(|ui| {
            ui.spinner();
            if let Some(progress) = state.import.progress {
                ui.weak(format!(
                    "{} / {} · imported {} · skipped {}",
                    progress.processed, progress.total, progress.imported, progress.skipped
                ));
            }
        });
    }
    if let Some(status) = &state.import.result {
        ui.weak(status);
    }
    ui.separator();
    playlist_list(ui, state, actions);
}

/// Mixdown controls: output path field, Browse, Render↔Cancel, and
/// staged progress while running.
fn render_controls(ui: &mut egui::Ui, render: RenderUiState<'_>, actions: &mut PanelActions) {
    ui.horizontal(|ui| {
        let field = egui::TextEdit::singleline(render.mix_path)
            .hint_text("mix.wav")
            .desired_width(ui.available_width() - 260.0);
        ui.add(field);
        if ui.button("Browse…").clicked() {
            actions.actions.push(PanelAction::BrowseRenderOut);
        }
        if render.running {
            let cancel = egui::Button::new("Cancel");
            if ui.add(cancel).clicked() {
                actions.actions.push(PanelAction::CancelRender);
            }
            ui.spinner();
            if let Some(stage) = render.stage {
                ui.weak(stage_text(stage));
            }
        } else {
            let render_btn = egui::Button::new("Render");
            if ui.add_enabled(render.can_render, render_btn).clicked() {
                actions.actions.push(PanelAction::Render);
            }
            ui.add_enabled(
                true,
                egui::DragValue::new(render.bpm)
                    .min_decimals(1)
                    .max_decimals(1)
                    .clamp_existing_to_range(true)
                    .range(0..=250),
            );
        }
    });
}

/// Human text for one progress stage.
fn stage_text(stage: crate::bus::RenderStage) -> String {
    use crate::bus::RenderStage;
    match stage {
        RenderStage::Decoding { done, total } => format!("decoding {done}/{total}"),
        RenderStage::Stretching { done, total } => format!("stretching {done}/{total}"),
        RenderStage::Mixing { fraction } => format!("mixing {:.0}%", fraction * 100.0),
    }
}

/// The left column: one selectable row per playlist with a context menu;
/// the renamed row swaps in-place for a focused text editor.
fn playlist_list(ui: &mut egui::Ui, state: &mut PlaylistState, actions: &mut PanelActions) {
    let n = state.playlists.len();
    for i in 0..n {
        let (id, name) = {
            let playlist = &state.playlists[i];
            (playlist.id, playlist.name.clone())
        };
        let selected = state.selected == Some(id);
        if state.rename.matches(id) {
            rename_row(ui, state, id, actions);
            continue;
        }
        let response = ui.selectable_label(selected, &name);
        if response.clicked() {
            actions.actions.push(PanelAction::SelectPlaylist(id));
        }
        response.context_menu(|ui| {
            if ui.button("Rename…").clicked() {
                state.rename.begin(id, &name);
                ui.close();
            }
            if ui.button("Delete").clicked() {
                actions.actions.push(PanelAction::DeletePlaylist(id));
                ui.close();
            }
        });
    }
    if state.playlists.is_empty() {
        ui.weak("no playlists");
    }
}

/// The rename editor row: replaces the playlist's label in-place with
/// a focused text edit over the current name.
fn rename_row(ui: &mut egui::Ui, state: &mut PlaylistState, id: i64, actions: &mut PanelActions) {
    let edit_id = egui::Id::new("playlist_rename_editor");
    if state.rename.pending_focus {
        seed_select_all(ui.ctx(), edit_id, &state.rename.buffer);
        ui.ctx().memory_mut(|m| m.request_focus(edit_id));
        state.rename.pending_focus = false;
    }
    let response = ui.add(
        egui::TextEdit::singleline(&mut state.rename.buffer)
            .id(edit_id)
            .return_key(None::<egui::KeyboardShortcut>)
            .desired_width(ui.available_width()),
    );
    if state.rename.hint.is_some() {
        ui.label(
            egui::RichText::new("name already exists")
                .small()
                .color(egui::Color32::RED),
        );
    }
    // Escape never reaches `has_focus`: egui clears focus pre-frame on
    // Escape (the TextEdit event filter cannot lock it), so the cancel
    // lands in `lost_focus` below.
    if response.has_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
        match rename_outcome(&state.rename, &state.playlists) {
            RenameOutcome::Submit(name) => {
                actions
                    .actions
                    .push(PanelAction::RenamePlaylist { id, name });
                state.rename.clear();
                ui.ctx().memory_mut(|m| m.surrender_focus(edit_id));
            }
            RenameOutcome::Revert => {
                state.rename.clear();
                ui.ctx().memory_mut(|m| m.surrender_focus(edit_id));
            }
            RenameOutcome::RejectDuplicate => {
                state.rename.hint = Some("name already exists");
            }
        }
        return;
    }
    if response.lost_focus() {
        if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            state.rename.clear();
            return;
        }
        match rename_outcome(&state.rename, &state.playlists) {
            RenameOutcome::Submit(name) => {
                actions
                    .actions
                    .push(PanelAction::RenamePlaylist { id, name });
            }
            // A duplicate cannot stay open without the field fighting
            // the click that took focus; revert instead.
            RenameOutcome::Revert | RenameOutcome::RejectDuplicate => {}
        }
        state.rename.clear();
    }
}

/// Seeds the editor's text state so the current name opens fully
/// selected (type-to-replace). Char indices, not bytes.
fn seed_select_all(ctx: &egui::Context, id: egui::Id, buffer: &str) {
    let mut text_state = egui::widgets::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    let end = buffer.chars().count();
    text_state
        .cursor
        .set_char_range(Some(egui::text::CCursorRange::two(
            egui::text::CCursor::default(),
            egui::text::CCursor {
                index: end,
                prefer_next_row: false,
            },
        )));
}

/// The right column: track rows with status icons, drag-reorder,
/// click-to-load, and per-row context menus.
fn rows(ui: &mut egui::Ui, state: &mut PlaylistState, tracks: &Tracks, actions: &mut PanelActions) {
    match &state.contents {
        Contents::None => {
            ui.centered_and_justified(|ui| ui.weak("no playlist selected"));
            return;
        }
        Contents::Loading => {
            ui.centered_and_justified(|ui| {
                ui.spinner();
                ui.weak("loading…");
            });
            return;
        }
        Contents::Failed(message) => {
            ui.centered_and_justified(|ui| ui.weak(format!("\u{26a0} {message}")));
            return;
        }
        Contents::Loaded(hashes) if hashes.is_empty() => {
            ui.centered_and_justified(|ui| ui.weak("empty playlist. Add… to browse"));
            return;
        }
        Contents::Loaded(_) => {}
    }
    let Contents::Loaded(hashes) = &state.contents else {
        return;
    };
    let hashes = hashes.as_slice();
    let median_bpm = playlist_median_bpm(tracks, hashes);
    let style = crate::library::view::RowStyle::resolve(ui);
    let header_font = egui::FontId::proportional(ui.text_style_height(&egui::TextStyle::Body));

    // Right-pinned column geometry. Widths are computed ONCE per frame
    // from the container's right edge — content plays no part, so no
    // track can ever move or squeeze these columns. Key/Duration keep
    // their header-derived widths; the artist/title boundary is the
    // user-adjustable one.
    let available = ui.available_rect_before_wrap();
    let [bpm_w, key_w, duration_w] = ["BPM", "Key", "Duration"]
        .map(|label| crate::library::view::header_column_width(ui.ctx(), label, &header_font));

    const COL_SPACING: f32 = 4.0;
    // Lay out right-to-left: the pinned metadata cluster first.
    let duration_r = egui::Rect::from_min_size(
        egui::pos2(available.right() - duration_w, available.top()),
        egui::vec2(duration_w, available.height()),
    );
    let key_r = egui::Rect::from_min_size(
        egui::pos2(duration_r.left() - COL_SPACING - key_w, available.top()),
        egui::vec2(key_w, available.height()),
    );
    let bpm_r = egui::Rect::from_min_size(
        egui::pos2(key_r.left() - COL_SPACING - bpm_w, available.top()),
        egui::vec2(bpm_w, available.height()),
    );

    // The user-resizable artist width (persisted across restarts),
    // clamped so Title always keeps at least half of the text strip.
    let text_left = available.left();
    let title_right = bpm_r.left() - COL_SPACING;
    let artist_max = ((title_right - text_left) * 0.5).max(24.0);
    // The 50% cap always wins over the floor when the half itself gets
    // tiny — order the limits so the range can never invert.
    let artist_min = 40.0_f32.min(artist_max);
    let artist_w = ui
        .ctx()
        .data_mut(|d| d.get_persisted::<f32>(egui::Id::new(ARTIST_WIDTH_KEY)))
        .unwrap_or(ARTIST_COLUMN_WIDTH)
        .clamp(artist_min, artist_max);
    let artist_r = egui::Rect::from_min_size(
        egui::pos2(text_left + GLYPH_RESERVE, available.top()),
        egui::vec2(artist_w, available.height()),
    );
    let title_r = egui::Rect::from_min_size(
        egui::pos2(artist_r.right() + COL_SPACING, available.top()),
        egui::vec2(
            (title_right - artist_r.right() - COL_SPACING).max(0.0),
            available.height(),
        ),
    );
    let columns = PlaylistColumns {
        rows: available,
        glyph: egui::Rect::from_min_size(
            available.min,
            egui::vec2(GLYPH_RESERVE, available.height()),
        ),
        artist: artist_r,
        title: title_r,
        bpm: bpm_r,
        key: key_r,
        duration: duration_r,
    };

    draw_playlist_header(ui, &columns, &style);

    // The artist/title resize handle sits between those two columns:
    // dragging it redistributes space inside the text strip while the
    // pinned metadata cluster never moves.
    let handle_rect = egui::Rect::from_min_max(
        egui::pos2(title_r.left() - COL_SPACING - 4.0, available.top()),
        egui::pos2(title_r.left() + 2.0, available.bottom()),
    );
    #[cfg(test)]
    panel_capture::record("artist_handle", handle_rect);
    let handle = ui
        .interact(
            handle_rect,
            egui::Id::new("playlist_artist_title_resize"),
            egui::Sense::drag(),
        )
        .on_hover_cursor(egui::CursorIcon::ResizeColumn);
    if handle.dragged() {
        // Absolute-position math again: no deltas, no feedback.
        if let Some(pointer) = ui.ctx().pointer_latest_pos() {
            let new_artist_w = (pointer.x - artist_r.left()).clamp(artist_min, artist_max);
            store_artist_width(ui.ctx(), new_artist_w);
        }
    }
    {
        let painter = ui.painter();
        let color = if handle.hovered() || handle.dragged() {
            ui.visuals().widgets.hovered.bg_stroke.color
        } else {
            ui.visuals().widgets.noninteractive.bg_stroke.color
        };
        painter.line_segment(
            [
                egui::pos2(title_r.left() - COL_SPACING / 2.0, available.top()),
                egui::pos2(title_r.left() - COL_SPACING / 2.0, available.bottom()),
            ],
            egui::Stroke::new(1.0, color),
        );
    }

    #[cfg(test)]
    for (name, rect) in [
        ("header_glyph", columns.glyph),
        ("header_artist", columns.artist),
        ("header_title", columns.title),
        ("header_bpm", columns.bpm),
        ("header_key", columns.key),
        ("header_duration", columns.duration),
    ] {
        panel_capture::record(name, rect);
    }

    let mut drop: Option<(TrackHash, TrackHash, bool)> = None;
    let mut prev_key: Option<djcore::key::Key> = None;
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show_rows(ui, style.height, hashes.len(), |ui, range| {
            // Rows are hand-painted at exact height multiples; any
            // inter-item spacing would advance the cursor further than
            // show_rows' virtualization math strides.
            ui.spacing_mut().item_spacing.y = 0.0;
            for index in range {
                let hash = &hashes[index];
                let record = tracks.get(hash);
                let analysis = record.map(|r| &r.analysis);
                let interactive = analysis.is_some_and(|a| a.is_ready() || a.is_pending());
                let row_context = PlaylistRowContext {
                    record,
                    analysis,
                    prev_key: prev_key.clone(),
                    median_bpm,
                    interactive,
                    path: record.map_or_else(String::new, |r| r.tags.path.display().to_string()),
                };
                let mut drop_here = None;
                playlist_row(
                    ui,
                    (&columns, &style),
                    row_context,
                    hash,
                    actions,
                    &mut drop_here,
                );
                if let Some(d) = drop_here {
                    drop = Some(d);
                }
                if let Some(AnalysisState::Ready(a)) = analysis {
                    prev_key = Some(a.key.clone());
                }
            }
        });
    if let Some(action) = move_action_for_drop(drop) {
        actions.actions.push(action);
    }
}

/// Pre-computed column rectangles for one frame of the playlist rows.
///
/// Every consumer derives its pixels from these; nothing measures
/// content horizontally, so track text can never move a column.
struct PlaylistColumns {
    /// Full row strip (all six columns).
    rows: egui::Rect,
    glyph: egui::Rect,
    artist: egui::Rect,
    title: egui::Rect,
    bpm: egui::Rect,
    key: egui::Rect,
    duration: egui::Rect,
}

/// Header labels painted over the same column rects as the body rows.
fn draw_playlist_header(
    ui: &mut egui::Ui,
    columns: &PlaylistColumns,
    style: &crate::library::view::RowStyle,
) {
    let allocate_label = |ui: &mut egui::Ui, rect: egui::Rect, label: &str| {
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            ui.add(
                egui::Label::new(egui::RichText::new(label).weak())
                    .truncate()
                    .sense(egui::Sense::hover()),
            );
        });
    };
    let top = columns.rows.top();
    for (rect, label) in [
        (columns.glyph, ""),
        (columns.artist, "Artist"),
        (columns.title, "Title"),
        (columns.bpm, "BPM"),
        (columns.key, "Key"),
        (columns.duration, "Duration"),
    ] {
        let cell = egui::Rect::from_min_size(
            egui::pos2(rect.left(), top),
            egui::vec2(rect.width(), style.height),
        );
        allocate_label(ui, cell, label);
    }
}

const ARTIST_WIDTH_KEY: &str = "playlist_artist_column_width";

fn store_artist_width(ctx: &egui::Context, width: f32) {
    ctx.data_mut(|d| d.insert_persisted(egui::Id::new(ARTIST_WIDTH_KEY), width));
}

/// Fixed left inset reserving room for the status glyph in every row.
const GLYPH_RESERVE: f32 = 28.0;

/// Default artist-column width; user-resizable between the drag
/// handle's clamps.
const ARTIST_COLUMN_WIDTH: f32 = 140.0;

/// Per-row display facts derived at render time from the record.
struct PlaylistRowContext<'a> {
    record: Option<&'a crate::tracks::TrackRecord>,
    analysis: Option<&'a AnalysisState>,
    prev_key: Option<djcore::key::Key>,
    median_bpm: Option<f32>,
    interactive: bool,
    /// Full file path for the hover tooltip.
    path: String,
}

/// One virtualized playlist row: six cells (glyph, artist, title,
/// bpm, key, duration); the union of cell responses carries
/// click-to-load, DnD drag payload, hover tooltip, and context menu.
fn playlist_row(
    ui: &mut egui::Ui,
    layout: (&PlaylistColumns, &crate::library::view::RowStyle),
    context: PlaylistRowContext<'_>,
    hash: &TrackHash,
    actions: &mut PanelActions,
    drop: &mut Option<(TrackHash, TrackHash, bool)>,
) {
    let (columns, style) = layout;
    // The row OCCUPIES layout space before anything paints on it: hand-
    // painted cells neither allocate nor advance the cursor, so without
    // this allocation every virtualized row measures the same frozen
    // cursor position and stacks on one rect.
    let row_rect = ui
        .allocate_exact_size(
            egui::vec2(columns.rows.width(), style.height),
            egui::Sense::hover(),
        )
        .0;
    #[cfg(test)]
    panel_capture::record_row(row_rect);
    let ready = context.analysis.and_then(|a| match a {
        AnalysisState::Ready(a) => Some(a),
        _ => None,
    });

    // Glyph + spinner state derived from the analysis state.
    let weak_color = style.metadata;
    let strong_color = style.main;
    let (glyph, glyph_color) = match context.analysis {
        Some(AnalysisState::Ready(_)) => ("\u{25c9}", strong_color),
        Some(AnalysisState::Queued) | None => ("🕓", weak_color),
        Some(AnalysisState::Analyzing) => ("⭕", weak_color),
        Some(AnalysisState::Failed(_)) => ("!", Color32::RED),
    };

    let artist = context
        .record
        .map_or_else(String::new, |r| r.tags.artist.clone());
    let title = context
        .record
        .map_or_else(String::new, |r| r.tags.title.clone());
    let bpm_text = ready.map_or_else(|| "--".to_owned(), |a| format!("{:.0}", a.bpm));
    let duration_text = ready.map_or_else(
        || "---".to_owned(),
        |a| {
            format!(
                "{}:{:02}",
                (a.duration_seconds / 60.0) as u32,
                (a.duration_seconds % 60.0) as u32
            )
        },
    );
    let key_text = ready.map_or_else(
        || "--".to_owned(),
        |a| a.key.format_with(djcore::key::KeyFormat::Camelot),
    );
    let key_color = key_display_color(ready.map(|a| a.key.clone()), context.prev_key, strong_color);
    let bpm_color = ready.map_or(weak_color, |a| {
        bpm_display_color(a.bpm, context.median_bpm, weak_color)
    });

    let tone = if context.interactive {
        style.main
    } else {
        style.metadata
    };

    // Cell rects for this row (columns were computed for the full
    // strip height).
    let offset = row_rect.min.to_vec2() - columns.rows.min.to_vec2();
    let cells: Vec<(&'static str, egui::Rect)> = [
        ("glyph", columns.glyph),
        ("artist", columns.artist),
        ("title", columns.title),
        ("bpm", columns.bpm),
        ("key", columns.key),
        ("duration", columns.duration),
    ]
    .into_iter()
    .map(|(name, rect)| (name, rect.translate(offset).intersect(row_rect)))
    .collect();

    // The row is ONE interactive surface registered after its cells:
    // sole owner of clicks/drags on the row, reliably hit-tested
    // (scope containers are not). Cells themselves are paint-only.
    let analyzing = matches!(context.analysis, Some(AnalysisState::Analyzing));
    let row_id = egui::Id::new("playlist_row").with(&hash.0);
    let response = ui
        .interact(row_rect, row_id, egui::Sense::click_and_drag())
        .on_hover_text(context.path.clone());

    // Hover background reacts to the row surface, not per-cell probes.
    let hovered = response.hovered();
    for (name, rect) in &cells {
        if rect.width() <= 0.0 || rect.height() <= 0.0 {
            continue;
        }
        #[cfg(test)]
        panel_capture::record(
            match *name {
                "glyph" => "cell_glyph",
                "artist" => "cell_artist",
                "title" => "cell_title",
                "bpm" => "cell_bpm",
                "key" => "cell_key",
                _ => "cell_duration",
            },
            *rect,
        );
        let painter = ui.painter_at(*rect);
        let bg = if !context.interactive {
            dimmed_color(ui)
        } else if hovered {
            hover_color(ui)
        } else {
            base_color(ui)
        };
        painter.rect_filled(rect.shrink(1.0), 2.0, bg);
        match *name {
            "glyph" => {
                paint_glyph_cell(ui, *rect, analyzing, glyph, glyph_color, weak_color, style)
            }
            _ => {
                let text = match *name {
                    "artist" => &artist,
                    "title" => &title,
                    "bpm" => &bpm_text,
                    "key" => &key_text,
                    _ => &duration_text,
                };
                let color = match *name {
                    "bpm" => bpm_color,
                    "key" => key_color,
                    "duration" => weak_color,
                    _ => tone,
                };
                crate::library::view::painted_text_cell(ui, *rect, text.clone(), color, style);
            }
        }
    }

    response.dnd_set_drag_payload(hash.clone());

    // While another row is dragged, `hovered` is false by design;
    // dnd_hover_payload/contains_pointer are the drop-zone APIs.
    if let (Some(pointer), Some(_)) = (
        response.ctx.input(|input| input.pointer.interact_pos()),
        response.dnd_hover_payload::<TrackHash>(),
    ) {
        let rect = response.rect;
        let insert_after = pointer.y >= rect.center().y;
        let y = if insert_after {
            rect.bottom()
        } else {
            rect.top()
        };
        let painter = egui::Painter::new(response.ctx.clone(), response.layer_id, response.rect);
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 210, 60)),
        );
        if let Some(released) = response.dnd_release_payload::<TrackHash>() {
            *drop = Some((released.as_ref().clone(), hash.clone(), insert_after));
        }
    }
    if let Some(action) = load_action_for_row(
        hash,
        context.interactive,
        response.clicked_by(egui::PointerButton::Primary),
        response.dragged(),
    ) {
        actions.actions.push(action);
    }
    if let Some(action) = preview_action_for_row(
        hash,
        response.clicked_by(egui::PointerButton::Middle),
        response.dragged_by(egui::PointerButton::Middle),
    ) {
        actions.actions.push(action);
    }
    response.context_menu(|ui| {
        if ui.button("Remove").clicked() {
            actions
                .actions
                .push(PanelAction::RemoveRow { hash: hash.clone() });
            ui.close();
        }
    });
}

/// Dimmed/lifted/fallback background shared by every playlist cell.
fn dimmed_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_gray(22)
    } else {
        Color32::from_gray(252)
    }
}

fn hover_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_gray(45)
    } else {
        Color32::from_gray(232)
    }
}

fn base_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_gray(30)
    } else {
        Color32::from_gray(248)
    }
}

/// Status glyph or animated spinner in the leading narrow cell.
#[allow(clippy::too_many_arguments)]
fn paint_glyph_cell(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    analyzing: bool,
    glyph: &str,
    glyph_color: Color32,
    weak_color: Color32,
    style: &crate::library::view::RowStyle,
) {
    let center_y = rect.center().y;
    if analyzing {
        let icon_size = ui.text_style_height(&egui::TextStyle::Body).min(16.0);
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 14.0, center_y),
            egui::vec2(icon_size, icon_size),
        );
        egui::Spinner::new()
            .color(weak_color)
            .paint_at(ui, icon_rect);
    } else {
        let g = plain_galley(ui, glyph, style.font.clone(), glyph_color);
        ui.painter_at(rect).galley(
            egui::pos2(rect.left() + 8.0, center_y - g.size().y / 2.0),
            g,
            glyph_color,
        );
    }
}

/// A single-line galley of `text` in `color`.
fn plain_galley(
    ui: &egui::Ui,
    text: &str,
    font_id: egui::FontId,
    color: egui::Color32,
) -> std::sync::Arc<egui::Galley> {
    let mut layout = egui::text::LayoutJob::default();
    layout.append(
        text,
        0.0,
        egui::text::TextFormat {
            font_id,
            color,
            ..Default::default()
        },
    );
    ui.fonts(|f| f.layout_job(layout))
}

/// Converts a released payload into a reorder intent, ignoring cancellation
/// and self-drops.
#[must_use]
fn move_action_for_drop(drop: Option<(TrackHash, TrackHash, bool)>) -> Option<PanelAction> {
    let (from, to, insert_after) = drop?;
    (from != to).then_some(PanelAction::MoveRow {
        from,
        to,
        insert_after,
    })
}

/// Converts one row response into its load intent, if the gesture was a
/// primary click on a row whose analysis can be loaded.
#[must_use]
fn load_action_for_row(
    hash: &TrackHash,
    interactive: bool,
    primary_clicked: bool,
    dragged: bool,
) -> Option<PanelAction> {
    if interactive && primary_clicked && !dragged {
        Some(PanelAction::LoadRow(hash.clone()))
    } else {
        None
    }
}

/// Converts one row response into its preview intent: any middle click
/// that did not become a middle drag previews the row — regardless of
/// analysis state (a failed row can still be auditioned).
#[must_use]
fn preview_action_for_row(
    hash: &TrackHash,
    middle_clicked: bool,
    middle_dragged: bool,
) -> Option<PanelAction> {
    (middle_clicked && !middle_dragged).then_some(PanelAction::PreviewRow(hash.clone()))
}

/// The upper half of a slot inserts before it; the lower half after.
#[must_use]
pub fn insertion_offset_in_slot(pointer_y: f32, slot_top: f32, slot_height: f32) -> usize {
    let half = slot_height / 2.0;
    if (pointer_y - slot_top) < half { 0 } else { 1 }
}

/// Median BPM of the ready tracks among `hashes`, or `None` when no
/// track has analysis.
///
/// Odd counts take the middle value; even counts average the two
/// middle values.
fn playlist_median_bpm(tracks: &Tracks, hashes: &[TrackHash]) -> Option<f32> {
    let mut bpms: Vec<f32> = hashes
        .iter()
        .filter_map(|hash| {
            tracks.get(hash).and_then(|r| match &r.analysis {
                AnalysisState::Ready(a) => Some(a.bpm),
                _ => None,
            })
        })
        .collect();
    if bpms.is_empty() {
        return None;
    }
    bpms.sort_by(f32::total_cmp);
    let n = bpms.len();
    Some(if n % 2 == 1 {
        bpms[n / 2]
    } else {
        (bpms[n / 2 - 1] + bpms[n / 2]) / 2.0
    })
}
/// Light red used to flag BPM outliers (fixed, theme-independent
/// like the key heatmap colors).
const OUTLIER_BPM_COLOR: Color32 = Color32::from_rgb(255, 120, 120);

/// Color for a row's BPM text: light red when the track deviates
/// more than 8 BPM from the playlist median, the fallback otherwise.
fn bpm_display_color(bpm: f32, median: Option<f32>, fallback: Color32) -> Color32 {
    match median {
        Some(m) if (bpm - m).abs() > 8.0 => OUTLIER_BPM_COLOR,
        _ => fallback,
    }
}

/// Color for a row's key text: gradient against the previous row's key,
/// or the fallback when uncolorable (first row or missing key).
///
/// Pure so tests can pin the endpoints.
fn key_display_color(
    key: Option<djcore::key::Key>,
    prev_key: Option<djcore::key::Key>,
    fallback: Color32,
) -> Color32 {
    match (key, prev_key) {
        (Some(k), Some(prev)) => harmonic_color(k.harmonic_distance(&prev)),
        _ => fallback,
    }
}

/// Maps a normalized harmonic distance (0.0–1.0) to a heatmap color.
///
/// Ported from harmonic-playlist's dynamic-playlist pane:
/// `0.0` → blue (identical), `0.2` cyan, `0.4` green, `0.6` yellow,
/// `0.8` orange, `1.0` red (opposite).
#[must_use]
pub fn harmonic_color(distance: f32) -> Color32 {
    const STOPS: [(f32, (u8, u8, u8)); 6] = [
        (0.0, (70, 130, 230)), // blue
        (0.2, (0, 190, 220)),  // cyan
        (0.4, (40, 200, 100)), // green
        (0.6, (240, 200, 40)), // yellow
        (0.8, (245, 140, 30)), // orange
        (1.0, (220, 60, 50)),  // red
    ];
    let clamped = distance.clamp(0.0, 1.0);
    let idx = STOPS
        .windows(2)
        .position(|w| clamped >= w[0].0 && clamped <= w[1].0)
        .unwrap_or(STOPS.len() - 2);
    let (t0, c0) = STOPS[idx];
    let (t1, c1) = STOPS[idx + 1];
    let t = if (t1 - t0).abs() < f32::EPSILON {
        0.0
    } else {
        (clamped - t0) / (t1 - t0)
    };
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "u8 channel from lerped f32"
    )]
    let channel = |a: u8, b: u8| {
        let lerped = f32::from(a) + t * (f32::from(b) - f32::from(a));
        lerped.round() as u8
    };
    Color32::from_rgb(
        channel(c0.0, c1.0),
        channel(c0.1, c1.1),
        channel(c0.2, c1.2),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn render_slice<'a>(
        mix_path: &'a mut String,
        bpm: &'a mut f32,
    ) -> RenderUiState<'a> {
        RenderUiState {
            mix_path,
            running: false,
            can_render: false,
            stage: None,
            bpm,
        }
    }

    // Given a pointer position inside a row slot.
    // When computing the insertion offset.
    // Then the upper half splices before (0) and the lower after (1).
    #[rstest::rstest]
    #[case(29.0, 20.0, 0)]
    #[case(31.0, 20.0, 1)]
    fn insertion_offset_splits_the_slot(
        #[case] pointer_y: f32,
        #[case] slot_top: f32,
        #[case] expected: usize,
    ) {
        assert_eq!(
            insertion_offset_in_slot(pointer_y, slot_top, 20.0),
            expected
        );
    }

    fn hash(id: u32) -> TrackHash {
        TrackHash(format!("h{id}"))
    }

    // Given a loadable row and a primary click with no drag.
    // When classifying the row gesture.
    // Then a LoadRow action targets that row.
    #[test]
    fn primary_click_on_loadable_row_emits_load_action() {
        let row = hash(1);

        let action = load_action_for_row(&row, true, true, false);

        assert_eq!(action, Some(PanelAction::LoadRow(row)));
    }

    // Given a loadable row whose pointer gesture became a drag.
    // When classifying the row gesture.
    // Then no LoadRow action is emitted.
    #[test]
    fn dragging_loadable_row_suppresses_load_action() {
        let row = hash(1);

        let action = load_action_for_row(&row, true, true, true);

        assert_eq!(action, None);
    }

    // Given a row with each analysis lifecycle state or no record.
    // When classifying a primary click.
    // Then only ready, queued, and analyzing rows emit LoadRow.
    #[rstest::rstest]
    #[case::ready(crate::tracks::AnalysisState::Ready(analysis(128.0)), true)]
    #[case::queued(crate::tracks::AnalysisState::Queued, true)]
    #[case::analyzing(crate::tracks::AnalysisState::Analyzing, true)]
    #[case::failed(crate::tracks::AnalysisState::Failed("boom".to_owned()), false)]
    fn loadability_matches_analysis_lifecycle(
        #[case] state: crate::tracks::AnalysisState,
        #[case] expected_loadable: bool,
    ) {
        let row = hash(1);
        let record = Some(state);
        let interactive = record
            .as_ref()
            .is_some_and(|state| state.is_ready() || state.is_pending());

        let action = load_action_for_row(&row, interactive, true, false);

        assert_eq!(action.is_some(), expected_loadable);
    }

    // Given no track record for a displayed hash.
    // When classifying a primary click.
    // Then no LoadRow action is emitted.
    #[test]
    fn unknown_row_does_not_emit_load_action() {
        let row = hash(1);

        let action = load_action_for_row(&row, false, true, false);

        assert_eq!(action, None);
    }

    // Given a middle click with no drag.
    // When classifying the row gesture.
    // Then a PreviewRow action targets that row.
    #[test]
    fn middle_click_row_emits_preview_row() {
        let row = hash(3);

        let action = preview_action_for_row(&row, true, false);

        assert_eq!(action, Some(PanelAction::PreviewRow(row)));
    }

    // Given a middle-button gesture that became a drag.
    // When classifying the row gesture.
    // Then no PreviewRow action is emitted (drag stays reorder territory).
    #[test]
    fn middle_drag_suppresses_preview_action() {
        let row = hash(3);

        let action = preview_action_for_row(&row, true, true);

        assert_eq!(action, None);
    }

    // Given a primary click or a no-button hover.
    // When classifying the row gesture for preview.
    // Then nothing is emitted — only middle clicks preview.
    #[test]
    fn non_middle_gestures_never_emit_preview_action() {
        let row = hash(3);

        let clicked = preview_action_for_row(&row, false, false);
        let dragged = preview_action_for_row(&row, false, true);

        assert_eq!(clicked, None);
        assert_eq!(dragged, None);
    }

    // Given a non-interactive row (failed analysis, unknown record).
    // When a plain middle click arrives.
    // Then it still previews — interactivity never gates the gesture.
    #[test]
    fn middle_click_previews_non_interactive_row() {
        let row = hash(4);

        let action = preview_action_for_row(&row, true, false);

        assert_eq!(action, Some(PanelAction::PreviewRow(row)));
    }

    // Given a released drag over the upper or lower half of another row.
    // When converting the drop to an action.
    // Then the target and insertion direction are preserved.
    #[rstest::rstest]
    #[case(false)]
    #[case(true)]
    fn valid_drop_emits_move_action_with_insertion_direction(#[case] insert_after: bool) {
        let from = hash(1);
        let to = hash(2);

        let action = move_action_for_drop(Some((from.clone(), to.clone(), insert_after)));

        assert_eq!(
            action,
            Some(PanelAction::MoveRow {
                from,
                to,
                insert_after,
            })
        );
    }

    // Given a drag with no valid target.
    // When converting the drop to an action.
    // Then the drop is cancelled without an action.
    #[test]
    fn drop_without_target_is_cancelled() {
        assert_eq!(move_action_for_drop(None), None);
    }

    // Given a released drag over its own row.
    // When converting the drop to an action.
    // Then the self-drop is cancelled without an action.
    #[test]
    fn self_drop_is_cancelled() {
        let row = hash(1);

        assert_eq!(move_action_for_drop(Some((row.clone(), row, true))), None);
    }

    fn analysis(bpm: f32) -> crate::tracks::Analysis {
        crate::tracks::Analysis {
            grid: djcore::analyzer::BeatGrid::default(),
            bpm,
            key: djcore::key::Key {
                root: 9,
                mode: djcore::key::KeyMode::Minor,
            },
            duration_seconds: 61.0,
            cues: automixah_engine::timeline::types::CuePoints::default(),
        }
    }

    fn upsert_with(
        tracks: &mut crate::tracks::Tracks,
        id: u32,
        state: crate::tracks::AnalysisState,
    ) {
        tracks.upsert(crate::tracks::TrackRecord {
            hash: hash(id),
            tags: crate::tracks::TrackTags {
                title: format!("T{id}"),
                artist: String::new(),
                path: std::path::PathBuf::from(format!("/t{id}")),
            },
            analysis: state,
        });
    }

    fn ready(bpm: f32) -> crate::tracks::AnalysisState {
        crate::tracks::AnalysisState::Ready(analysis(bpm))
    }

    // Given three ready tracks at 128, 130, 132 BPM.
    // When computing the playlist median.
    // Then the middle value 130 is the median.
    #[test]
    fn odd_count_median_is_middle_bpm() {
        let mut tracks = crate::tracks::Tracks::default();
        for bpm in [128.0, 132.0, 130.0] {
            tracks.set_analysis(&TrackHash(format!("h{}", bpm as u32)), ready(bpm));
        }
        let hashes = vec![
            TrackHash("h128".to_owned()),
            TrackHash("h132".to_owned()),
            TrackHash("h130".to_owned()),
        ];

        let median = playlist_median_bpm(&tracks, &hashes);

        assert_eq!(median, Some(130.0));
    }

    // Given two ready tracks at 128 and 130 BPM.
    // When computing the playlist median.
    // Then the average of the middles 129 is the median.
    #[test]
    fn even_count_median_averages_middles() {
        let mut tracks = crate::tracks::Tracks::default();
        tracks.set_analysis(&TrackHash("h1".to_owned()), ready(128.0));
        tracks.set_analysis(&TrackHash("h2".to_owned()), ready(130.0));
        let hashes = vec![TrackHash("h1".to_owned()), TrackHash("h2".to_owned())];

        let median = playlist_median_bpm(&tracks, &hashes);

        assert_eq!(median, Some(129.0));
    }

    // Given rows in every non-ready analysis state plus one ready.
    // When computing the playlist median.
    // Then only the ready BPM participates.
    #[test]
    fn median_counts_only_ready_rows() {
        let mut tracks = crate::tracks::Tracks::default();
        upsert_with(&mut tracks, 1, crate::tracks::AnalysisState::Queued);
        upsert_with(&mut tracks, 2, crate::tracks::AnalysisState::Analyzing);
        upsert_with(
            &mut tracks,
            3,
            crate::tracks::AnalysisState::Failed("boom".to_owned()),
        );
        upsert_with(&mut tracks, 4, ready(140.0));
        let hashes: Vec<TrackHash> = (1..=4).map(hash).collect();

        let median = playlist_median_bpm(&tracks, &hashes);

        assert_eq!(median, Some(140.0), "only the ready BPM counted");
    }

    // Given a track exactly 8 BPM off the median.
    // When deciding its BPM color.
    // Then the fallback color is kept (strictly-greater boundary).
    #[test]
    fn exactly_eight_off_median_is_not_colored() {
        let fallback = Color32::WHITE;

        let color = bpm_display_color(138.0, Some(130.0), fallback);

        assert_eq!(color, fallback);
    }

    // Given a track more than 8 BPM off the median.
    // When deciding its BPM color.
    // Then the fixed light red flags it.
    #[rstest::rstest]
    #[case(138.1)]
    #[case(121.9)]
    fn beyond_eight_off_median_flags_light_red(#[case] bpm: f32) {
        let color = bpm_display_color(bpm, Some(130.0), Color32::WHITE);

        assert_eq!(color, OUTLIER_BPM_COLOR);
        assert_eq!(OUTLIER_BPM_COLOR, Color32::from_rgb(255, 120, 120));
    }

    // Given a playlist with no analyzed tracks.
    // When computing the median.
    // Then it is None and any BPM falls back uncolored.
    #[test]
    fn no_ready_tracks_yields_none_and_fallback_color() {
        let tracks = crate::tracks::Tracks::default();
        let _library = crate::library::LibraryState::default();
        let _filter = String::new();
        let hashes: Vec<TrackHash> = Vec::new();

        let median = playlist_median_bpm(&tracks, &hashes);
        let color = bpm_display_color(128.0, median, Color32::WHITE);

        assert_eq!(median, None);
        assert_eq!(color, Color32::WHITE);
    }

    // Given a playlist with a single analyzed track.
    // When computing the median and its own color.
    // Then the median is its own BPM and it stays uncolored.
    #[test]
    fn single_ready_track_is_own_median_and_uncolored() {
        let mut tracks = crate::tracks::Tracks::default();
        tracks.set_analysis(&TrackHash("h1".to_owned()), ready(128.0));
        let hashes = vec![TrackHash("h1".to_owned())];

        let median = playlist_median_bpm(&tracks, &hashes);
        let color = bpm_display_color(128.0, median, Color32::WHITE);

        assert_eq!(median, Some(128.0));
        assert_eq!(color, Color32::WHITE, "deviation is zero");
    }

    fn key(root: u8) -> djcore::key::Key {
        djcore::key::Key {
            root,
            mode: djcore::key::KeyMode::Minor,
        }
    }

    fn editing_state() -> PlaylistState {
        let mut state = PlaylistState {
            playlists: vec![crate::playlist::store::PlaylistSummary {
                id: 1,
                name: "old".to_owned(),
            }],
            ..Default::default()
        };
        state.rename.begin(1, "old");
        state
    }

    // Given a focused, open rename editor.
    // When the user presses Enter and then keeps rendering frames.
    // Then exactly one rename action was emitted in total — the commit
    // neither duplicates nor evaporates across subsequent frames.
    #[test]
    fn enter_commit_survives_multipass_frames() {
        let mut state = editing_state();
        let ctx = egui::Context::default();
        ctx.options_mut(|o| o.max_passes = std::num::NonZeroUsize::new(2).expect("2 > 0"));
        let tracks = crate::tracks::Tracks::default();
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let mut sort = crate::library::sort::SortState::default();

        // Frame 0: the editor appears (layout changes → discard + rerun).
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let _ = panel(
                ctx,
                &mut state,
                &tracks,
                &mut library,
                &mut filter,
                &mut sort,
                render_slice(&mut String::new(), &mut 138.0),
            );
        });

        state.rename.buffer = "new".to_owned();
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::default(),
            }],
            ..Default::default()
        };
        let out = ctx.run(input, |ctx| {
            let _ = panel(
                ctx,
                &mut state,
                &tracks,
                &mut library,
                &mut filter,
                &mut sort,
                render_slice(&mut String::new(), &mut 138.0),
            );
        });

        assert_eq!(
            out.platform_output.num_completed_passes, 1,
            "commit frame must not trigger another layout change"
        );
        assert!(!state.rename.matches(1), "editor closed");
    }

    // Given the inline rename editor focused with a typed name.
    // When Enter is pressed.
    // Then a rename action with the typed name is emitted and the
    // editor closes.
    #[test]
    fn inline_editor_enter_commits() {
        let mut state = editing_state();
        let ctx = egui::Context::default();
        let mut out = String::new();
        let mut bpm = 138.0;
        let tracks = crate::tracks::Tracks::default();
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let _ = panel(
                ctx,
                &mut state,
                &tracks,
                &mut library,
                &mut filter,
                &mut crate::library::sort::SortState::default(),
                render_slice(&mut out, &mut bpm),
            );
        });
        state.rename.buffer = "  new  ".to_owned();
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Enter,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut actions = PanelActions::default();
        let tracks = crate::tracks::Tracks::default();
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let _ = ctx.run(input, |ctx| {
            actions = panel(
                ctx,
                &mut state,
                &tracks,
                &mut library,
                &mut filter,
                &mut crate::library::sort::SortState::default(),
                render_slice(&mut out, &mut bpm),
            );
        });

        assert_eq!(
            actions.actions,
            vec![PanelAction::RenamePlaylist {
                id: 1,
                name: "new".to_owned(),
            }],
            "trimmed typed name commits"
        );
        assert!(!state.rename.matches(1), "editor closed");
    }

    // Given the inline rename editor focused.
    // When Escape is pressed.
    // Then no rename action is emitted and the editor closes.
    #[test]
    fn inline_editor_escape_cancels() {
        let mut state = editing_state();
        let ctx = egui::Context::default();
        let mut out = String::new();
        let mut bpm = 138.0;
        let tracks = crate::tracks::Tracks::default();
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let _ = panel(
                ctx,
                &mut state,
                &tracks,
                &mut library,
                &mut filter,
                &mut crate::library::sort::SortState::default(),
                render_slice(&mut out, &mut bpm),
            );
        });
        let input = egui::RawInput {
            events: vec![egui::Event::Key {
                key: egui::Key::Escape,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }],
            ..Default::default()
        };
        let mut actions = PanelActions::default();
        let tracks = crate::tracks::Tracks::default();
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let _ = ctx.run(input, |ctx| {
            actions = panel(
                ctx,
                &mut state,
                &tracks,
                &mut library,
                &mut filter,
                &mut crate::library::sort::SortState::default(),
                render_slice(&mut out, &mut bpm),
            );
        });

        assert!(actions.actions.is_empty(), "nothing submitted");
        assert!(!state.rename.matches(1), "editor closed");
    }

    // Given the inline rename editor with a typed name that then loses
    // focus (click-away).
    // When the next frame renders.
    // Then the typed name commits like Enter.
    #[test]
    fn inline_editor_focus_loss_commits() {
        let mut state = editing_state();
        let ctx = egui::Context::default();
        let mut out = String::new();
        let mut bpm = 138.0;
        let tracks = crate::tracks::Tracks::default();
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let _ = panel(
                ctx,
                &mut state,
                &tracks,
                &mut library,
                &mut filter,
                &mut crate::library::sort::SortState::default(),
                render_slice(&mut out, &mut bpm),
            );
        });
        state.rename.buffer = "new".to_owned();
        // Click-away is another widget grabbing focus during a frame.
        let mut actions = PanelActions::default();
        let tracks = crate::tracks::Tracks::default();
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.ctx()
                    .memory_mut(|m| m.request_focus(egui::Id::new("click_away_target")));
                let _ = ui.allocate_space(egui::vec2(1.0, 1.0));
            });
            actions = panel(
                ctx,
                &mut state,
                &tracks,
                &mut library,
                &mut filter,
                &mut crate::library::sort::SortState::default(),
                render_slice(&mut out, &mut bpm),
            );
        });

        assert_eq!(
            actions.actions,
            vec![PanelAction::RenamePlaylist {
                id: 1,
                name: "new".to_owned(),
            }],
            "focus loss commits the typed name"
        );
        assert!(!state.rename.matches(1), "editor closed");
    }

    // Given a row with a key but no previous row.
    // When computing its display color.
    // Then the fallback color is used unmodified.
    #[test]
    fn first_row_key_uses_fallback_color() {
        let fallback = Color32::WHITE;

        let color = key_display_color(Some(key(9)), None, fallback);

        assert_eq!(color, fallback);
    }

    // Given a row with no key after a keyed row.
    // When computing the row color.
    // Then the fallback color is used unmodified.
    #[test]
    fn missing_key_uses_fallback_color() {
        let fallback = Color32::WHITE;

        let color = key_display_color(None, Some(key(9)), fallback);

        assert_eq!(color, fallback);
    }

    // Given two identical keys.
    // When computing the row color.
    // Then it is the zero-distance blue endpoint.
    #[test]
    fn same_key_colors_blue() {
        let color = key_display_color(Some(key(9)), Some(key(9)), Color32::WHITE);

        assert_eq!(color, harmonic_color(0.0));
    }

    // Given distance 0 (identical keys).
    // When colored.
    // Then the blue endpoint is returned.
    #[test]
    fn harmonic_color_zero_is_blue() {
        assert_eq!(harmonic_color(0.0), Color32::from_rgb(70, 130, 230));
    }

    // Given distance 1 (opposite keys).
    // When colored.
    // Then the red endpoint is returned.
    #[test]
    fn harmonic_color_one_is_red() {
        assert_eq!(harmonic_color(1.0), Color32::from_rgb(220, 60, 50));
    }

    // Given the midpoints.
    // When colored.
    // Then each named stop color is returned exactly.
    #[test]
    fn harmonic_color_hits_named_stops() {
        assert_eq!(harmonic_color(0.2), Color32::from_rgb(0, 190, 220));
        assert_eq!(harmonic_color(0.4), Color32::from_rgb(40, 200, 100));
        assert_eq!(harmonic_color(0.6), Color32::from_rgb(240, 200, 40));
        assert_eq!(harmonic_color(0.8), Color32::from_rgb(245, 140, 30));
    }

    // Given an out-of-range distance.
    // When colored.
    // Then it clamps instead of extrapolating past the endpoints.
    #[test]
    fn harmonic_color_clamps_out_of_range() {
        assert_eq!(harmonic_color(-3.0), harmonic_color(0.0));
        assert_eq!(harmonic_color(9.0), harmonic_color(1.0));
    }

    // Given distances between stops.
    // When colored.
    // Then the result lerps between the neighboring stops.
    #[test]
    fn harmonic_color_interpolates_between_stops() {
        let mid = harmonic_color(0.5);
        // 0.5 is halfway between green (40,200,100) and yellow (240,200,40).
        assert_eq!(mid, Color32::from_rgb(140, 200, 70));
    }
}

#[cfg(test)]
mod column_isolation {
    //! Headless proof that no pointer gesture near the library seam can
    //! move playlist-section geometry: the row is two imposed halves with
    //! no resize grips, so sibling displacement has no mechanism.
    use super::tests::render_slice;
    use super::{panel_capture, *};
    use automixah_engine::timeline::types::CuePoints;
    use djcore::analyzer::BeatGrid;
    // Given the four-column layout at the production viewport size.
    // When the pointer presses at the library seam and drags right 80px.
    // Then playlist_tracks must not move.
    #[test]
    fn seam_drag_shifts_playlist_columns() {
        let ctx = egui::Context::default();
        let mut state = PlaylistState::default();
        let tracks = crate::tracks::Tracks::default();
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let mut sort = crate::library::sort::SortState::default();

        let mut run = |ctx: &egui::Context, raw: egui::RawInput| -> egui::FullOutput {
            ctx.run(raw, |ctx| {
                let mut out = String::new();
                let mut bpm = 138.0_f32;
                let render = RenderUiState {
                    mix_path: &mut out,
                    running: false,
                    can_render: false,
                    stage: None,
                    bpm: &mut bpm,
                };
                panel(
                    ctx,
                    &mut state,
                    &tracks,
                    &mut library,
                    &mut filter,
                    &mut sort,
                    render,
                );
            })
        };
        let screen = |pos| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 720.0),
            )),
            events: pos,
            ..egui::RawInput::default()
        };

        // Warm-up frames so layout settles.
        panel_capture::reset();
        run(&ctx, screen(vec![]));
        run(&ctx, screen(vec![]));
        let before = panel_capture::latest("playlist_tracks")
            .expect("warm frame")
            .min
            .x;

        // Hover the roots/entries boundary (formerly a resizable panel
        // grip), press, and drag right through several frames.
        let seam_x = 220.0; // inside the library half, far from any grip
        let y = 300.0;
        let at = |x: f32| vec![egui::Event::PointerMoved(egui::pos2(x, y))];
        let press = |x: f32| {
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }]
        };
        run(&ctx, screen(at(seam_x)));
        run(&ctx, screen(press(seam_x)));
        for dx in [10.0, 30.0, 55.0, 80.0] {
            run(&ctx, screen(at(seam_x + dx)));
        }

        let after = panel_capture::latest("playlist_tracks")
            .expect("post-drag frame")
            .min
            .x;
        assert_eq!(before, after, "playlist section must not move");
    }

    // Given the rendered bottom panel with its divider grip.
    // When the pointer drags the divider left/right across several
    // frames.
    // Then the halves follow the pointer while both keep their
    // minimum widths; releasing keeps the new split (persisted).
    #[test]
    fn divider_drag_moves_halves_and_respects_floors() {
        let ctx = egui::Context::default();
        let mut state = PlaylistState::default();
        let tracks = crate::tracks::Tracks::default();
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let mut sort = crate::library::sort::SortState::default();

        let mut run = |ctx: &egui::Context, raw: egui::RawInput| {
            ctx.run(raw, |ctx| {
                let mut out = String::new();
                let mut bpm = 138.0_f32;
                panel(
                    ctx,
                    &mut state,
                    &tracks,
                    &mut library,
                    &mut filter,
                    &mut sort,
                    RenderUiState {
                        mix_path: &mut out,
                        running: false,
                        can_render: false,
                        stage: None,
                        bpm: &mut bpm,
                    },
                );
            })
        };
        let screen = |events| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 720.0),
            )),
            events,
            ..egui::RawInput::default()
        };
        fn assert_close(a: f32, b: f32, what: &str) {
            assert!((a - b).abs() < 1e-3, "{what}: {a} vs {b}");
        }
        let press = |x, y| {
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }]
        };
        let release = |x, y| {
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }]
        };
        let at = |x, y| vec![egui::Event::PointerMoved(egui::pos2(x, y))];

        for _ in 0..3 {
            panel_capture::reset();
            run(&ctx, screen(vec![]));
        }
        // Default split: divider is a 12px strip just right of the
        // library half.
        let divider_x = panel_capture::latest("entries")
            .expect("warm frame")
            .right()
            + 6.0;
        let y = 560.0;

        // Press on the divider and drag it 100px left.
        run(&ctx, screen(at(divider_x, y)));
        run(&ctx, screen(press(divider_x, y)));
        for dx in [-25.0, -50.0, -75.0, -100.0] {
            run(&ctx, screen(at(divider_x + dx, y)));
        }
        run(&ctx, screen(release(divider_x - 100.0, y)));
        for _ in 0..2 {
            run(&ctx, screen(vec![]));
        }
        let dragged_left = panel_capture::latest("entries")
            .expect("left frame")
            .right();
        assert!(
            dragged_left < 634.0 - 90.0,
            "library half shrank after dragging left: {dragged_left}"
        );
        // Halves still tile: playlist tracks starts one divider later.
        assert_close(
            panel_capture::latest("playlist_tracks").expect("pt").left() - dragged_left,
            crate::playlist::layout::DIVIDER_WIDTH,
            "divider gap after drag",
        );

        // Drag hard right toward the window edge: floors stop it before
        // either half collapses.
        run(&ctx, screen(at(dragged_left + 6.0, y)));
        run(&ctx, screen(press(dragged_left + 6.0, y)));
        for _ in 0..6 {
            run(&ctx, screen(at(1500.0, y)));
            run(&ctx, screen(at(1900.0, y)));
        }
        run(&ctx, screen(release(1900.0, y)));
        for _ in 0..2 {
            run(&ctx, screen(vec![]));
        }
        let maxed = panel_capture::latest("entries").expect("max").right();
        // Library cannot take so much that the playlist half drops
        // under PLAYLIST_MIN (280): entries.right <= 1280 - 12/2? -
        // margins — check bounds loosely but strictly below full row.
        assert!(
            maxed < 1272.0 - crate::playlist::layout::PLAYLIST_MIN + 20.0,
            "library half hit the floor well before the window edge: {maxed}"
        );

        // Release then re-render: position persists (no spring-back).
        for _ in 0..2 {
            run(&ctx, screen(vec![]));
        }
        assert_close(
            panel_capture::latest("entries").expect("stable").right(),
            maxed,
            "split persists after release",
        );
    }

    // Given the rendered bottom panel with its divider grip.
    // When the pointer wiggles the divider purely vertically for many
    // frames.
    // Then the split does not drift at all — vertical drags are no-ops,
    // so neither half can creep in any direction.
    #[test]
    fn vertical_divider_drag_causes_no_drift() {
        let ctx = egui::Context::default();
        let mut state = PlaylistState::default();
        let tracks = crate::tracks::Tracks::default();
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let mut sort = crate::library::sort::SortState::default();

        let mut run = |ctx: &egui::Context, raw: egui::RawInput| {
            ctx.run(raw, |ctx| {
                let mut out = String::new();
                let mut bpm = 138.0_f32;
                panel(
                    ctx,
                    &mut state,
                    &tracks,
                    &mut library,
                    &mut filter,
                    &mut sort,
                    RenderUiState {
                        mix_path: &mut out,
                        running: false,
                        can_render: false,
                        stage: None,
                        bpm: &mut bpm,
                    },
                );
            })
        };
        let screen = |events| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 720.0),
            )),
            events,
            ..egui::RawInput::default()
        };

        for _ in 0..3 {
            panel_capture::reset();
            run(&ctx, screen(vec![]));
        }
        fn assert_close(a: f32, b: f32, what: &str) {
            assert!((a - b).abs() < 1e-3, "{what}: {a} vs {b}");
        }
        let x = panel_capture::latest("entries").expect("warm").right() + 6.0;
        let start = panel_capture::latest("entries").expect("warm").right();
        let press = |x, y| {
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }]
        };

        // Grab the divider and jiggle up/down across many frames.
        run(&ctx, screen(press(x, 500.0)));
        for dy in [5.0, -7.0, 11.0, -3.0, 9.0, -13.0, 4.0, -6.0] {
            run(
                &ctx,
                screen(vec![egui::Event::PointerMoved(egui::pos2(x, 500.0 + dy))]),
            );
        }
        run(&ctx, screen(release_all()));
        run(&ctx, screen(vec![]));

        let settled = panel_capture::latest("entries").expect("settled");
        assert_close(
            settled.right(),
            start,
            "library half unmoved by vertical wiggle",
        );
    }

    // Given a loaded playlist whose one track has absurdly long tags.
    // When the panel renders into a deliberately narrow playlist half.
    // Then the metadata columns sit at identical positions regardless
    // of tag width — cells elide instead of overlapping.
    #[test]
    fn narrow_half_keeps_metadata_columns_content_proof() {
        let build = |long: bool| {
            let ctx = egui::Context::default();
            let mut state = PlaylistState::default();
            let mut tracks = crate::tracks::Tracks::default();
            let hash = TrackHash("h1".to_owned());
            let analysis = crate::tracks::Analysis {
                grid: BeatGrid {
                    grid_bpm: 174.0,
                    anchor_seconds: 0.0,
                    downbeats: vec![],
                    beats: vec![],
                    bars: vec![],
                },
                bpm: 174.0,
                key: djcore::key::Key {
                    root: 8,
                    mode: djcore::key::KeyMode::Minor,
                },
                duration_seconds: 200.0,
                cues: CuePoints::default(),
            };
            let record = crate::tracks::TrackRecord {
                hash: hash.clone(),
                tags: crate::tracks::TrackTags {
                    title: if long {
                        "T".repeat(300)
                    } else {
                        "t".to_owned()
                    },
                    artist: if long {
                        "A".repeat(300)
                    } else {
                        "a".to_owned()
                    },
                    path: "/x/a.mp3".into(),
                },
                analysis: AnalysisState::Ready(analysis),
            };
            tracks.upsert(record);
            state.contents = Contents::Loaded(vec![hash]);
            let mut library = crate::library::LibraryState::default();
            let mut filter = String::new();
            let mut sort = crate::library::sort::SortState::default();
            for _ in 0..3 {
                let _ = ctx.run(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(1280.0, 720.0),
                        )),
                        ..egui::RawInput::default()
                    },
                    |ctx| {
                        let mut out = String::new();
                        let mut bpm = 138.0_f32;
                        panel_capture::reset();
                        panel(
                            ctx,
                            &mut state,
                            &tracks,
                            &mut library,
                            &mut filter,
                            &mut sort,
                            RenderUiState {
                                mix_path: &mut out,
                                running: false,
                                can_render: false,
                                stage: None,
                                bpm: &mut bpm,
                            },
                        );
                    },
                );
            }
            (
                panel_capture::latest("playlists").expect("col"),
                panel_capture::latest("playlist_tracks").expect("half"),
            )
        };
        let (playlists_short, _) = build(false);
        let (playlists_long, _) = build(true);

        // The far-right playlists column keeps the exact same rect
        // under both contents: nothing leaked out of its cell.
        assert_eq!(playlists_short, playlists_long);
    }

    // Given a loaded playlist with a very long title and the split
    // dragged so the playlist half is at its narrowest.
    // When the panel renders.
    // Then every metadata header cell stays fully inside the tracks
    // half — the title column absorbs the squeeze instead of
    // overflowing and pinning BPM/Key/Duration off-screen.
    #[test]
    fn squeezed_half_keeps_metadata_cells_inside_tracks_rect() {
        let ctx = egui::Context::default();
        let mut state = PlaylistState::default();
        let mut tracks = crate::tracks::Tracks::default();
        let hash = TrackHash("h1".to_owned());
        let analysis = crate::tracks::Analysis {
            bpm: 174.0,
            key: djcore::key::Key {
                root: 8,
                mode: djcore::key::KeyMode::Minor,
            },
            duration_seconds: 200.0,
            grid: BeatGrid {
                grid_bpm: 174.0,
                anchor_seconds: 0.0,
                downbeats: vec![],
                beats: vec![],
                bars: vec![],
            },
            cues: CuePoints::default(),
        };
        tracks.upsert(crate::tracks::TrackRecord {
            hash: hash.clone(),
            tags: crate::tracks::TrackTags {
                title: "T".repeat(300),
                artist: "A".repeat(300),
                path: "/x/a.mp3".into(),
            },
            analysis: AnalysisState::Ready(analysis),
        });
        state.contents = Contents::Loaded(vec![hash]);
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let mut sort = crate::library::sort::SortState::default();

        fn assert_close(a: f32, b: f32, what: &str) {
            assert!((a - b).abs() < 1e-3, "{what}: {a} vs {b}");
        }

        for _ in 0..3 {
            panel_capture::reset();
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 720.0),
                    )),
                    ..egui::RawInput::default()
                },
                |ctx| {
                    let mut out = String::new();
                    let mut bpm = 138.0_f32;
                    panel(
                        ctx,
                        &mut state,
                        &tracks,
                        &mut library,
                        &mut filter,
                        &mut sort,
                        RenderUiState {
                            mix_path: &mut out,
                            running: false,
                            can_render: false,
                            stage: None,
                            bpm: &mut bpm,
                        },
                    );
                },
            );
        }
        let half = panel_capture::latest("playlist_tracks").expect("half");
        for name in ["header_bpm", "header_key", "header_duration"] {
            let cell = panel_capture::latest(name).expect(name);
            assert!(
                cell.right() <= half.right() + 1e-3,
                "{name} escaped the tracks half: cell right {} > half right {}",
                cell.right(),
                half.right()
            );
            assert!(cell.width() > 20.0, "{name} collapsed");
        }
        // And duration ends flush inside the half (the table fills it).
        let duration = panel_capture::latest("header_duration").expect("dur");
        assert_close(
            duration.right(),
            half.right(),
            "table fills the half exactly",
        );
    }

    // Given a playlist rendered while its half was WIDE, with long
    // titles.
    // When the half then shrinks to its narrowest over more frames.
    // Then BPM/Key/Duration header cells remain inside the tracks half.
    #[test]
    fn shrinking_half_keeps_metadata_visible_after_wide_start() {
        let ctx = egui::Context::default();
        let state = PlaylistState::default();
        let mut tracks = crate::tracks::Tracks::default();
        let hash = TrackHash("h1".to_owned());
        let analysis = crate::tracks::Analysis {
            bpm: 174.0,
            key: djcore::key::Key {
                root: 8,
                mode: djcore::key::KeyMode::Minor,
            },
            duration_seconds: 200.0,
            grid: BeatGrid {
                grid_bpm: 174.0,
                anchor_seconds: 0.0,
                downbeats: vec![],
                beats: vec![],
                bars: vec![],
            },
            cues: CuePoints::default(),
        };
        tracks.upsert(crate::tracks::TrackRecord {
            hash: hash.clone(),
            tags: crate::tracks::TrackTags {
                title: "T".repeat(300),
                artist: "A".repeat(2),
                path: "/x/a.mp3".into(),
            },
            analysis: AnalysisState::Ready(analysis),
        });
        let mut st = state;
        st.contents = Contents::Loaded(vec![hash]);
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let mut sort = crate::library::sort::SortState::default();

        let mut frame = |ctx: &egui::Context| {
            panel_capture::reset();
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 720.0),
                    )),
                    ..egui::RawInput::default()
                },
                |ctx| {
                    let mut out = String::new();
                    let mut bpm = 138.0_f32;
                    panel(
                        ctx,
                        &mut st,
                        &tracks,
                        &mut library,
                        &mut filter,
                        &mut sort,
                        RenderUiState {
                            mix_path: &mut out,
                            running: false,
                            can_render: false,
                            stage: None,
                            bpm: &mut bpm,
                        },
                    );
                },
            );
        };

        // Wide start (this is where a huge title cell would get baked
        // into egui_extras state, if that were possible).
        for f in [0.8_f32; 4] {
            ctx.data_mut(|d| d.insert_persisted(egui::Id::new(LIBRARY_FRACTION_KEY), f));
            frame(&ctx);
        }
        // Squeeze far left across frames (like holding the drag).
        for _ in 0..8 {
            ctx.data_mut(|d| d.insert_persisted(egui::Id::new(LIBRARY_FRACTION_KEY), 0.02_f32));
            frame(&ctx);
        }
        let half = panel_capture::latest("playlist_tracks").expect("half");
        for name in ["header_bpm", "header_key", "header_duration"] {
            let cell = panel_capture::latest(name).unwrap_or_else(|| panic!("{name} not painted"));
            assert!(
                cell.right() <= half.right() + 1e-3,
                "{name} pinned off-screen after squeeze: right {} vs half {}",
                cell.right(),
                half.right()
            );
            assert!(cell.width() > 10.0, "{name} collapsed after squeeze");
        }
    }

    // Given a seeded library entry and one ready playlist track, both
    // rendered through the real panel.
    // When the pointer single-clicks a playlist row's title cell.
    // Then exactly one LoadRow action is emitted — the row must not
    // require a double click and cell scoping must not swallow clicks.
    #[test]
    fn playlist_row_single_click_loads_into_editor() {
        let ctx = egui::Context::default();
        let mut tracks = crate::tracks::Tracks::default();
        let hash = TrackHash("h1".to_owned());
        let analysis = crate::tracks::Analysis {
            bpm: 174.0,
            key: djcore::key::Key {
                root: 8,
                mode: djcore::key::KeyMode::Minor,
            },
            duration_seconds: 200.0,
            grid: BeatGrid {
                grid_bpm: 174.0,
                anchor_seconds: 0.0,
                downbeats: vec![],
                beats: vec![],
                bars: vec![],
            },
            cues: CuePoints::default(),
        };
        tracks.upsert(crate::tracks::TrackRecord {
            hash: hash.clone(),
            tags: crate::tracks::TrackTags {
                title: "song".into(),
                artist: "act".into(),
                path: "/x/s.mp3".into(),
            },
            analysis: AnalysisState::Ready(analysis),
        });
        let mut st = PlaylistState {
            contents: Contents::Loaded(vec![hash.clone()]),
            ..Default::default()
        };
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let mut sort = crate::library::sort::SortState::default();
        let screen = |events| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 720.0),
            )),
            events,
            ..egui::RawInput::default()
        };
        let mut frame = |ctx: &egui::Context, raw: egui::RawInput| {
            let mut out = String::new();
            let mut bpm = 138.0_f32;
            let mut actions = PanelActions::default();
            let _ = ctx.run(raw, |ctx| {
                actions = panel(
                    ctx,
                    &mut st,
                    &tracks,
                    &mut library,
                    &mut filter,
                    &mut sort,
                    render_slice(&mut out, &mut bpm),
                );
            });
            actions
        };

        for _ in 0..3 {
            let _ = frame(&ctx, screen(vec![]));
        }
        // Click inside the playlist half on the first row. The playlist
        // rows live in the lower strip; aim inside the captured
        // "playlist_tracks" area, left portion (title zone).
        panel_capture::reset();
        let _ = frame(&ctx, screen(vec![]));
        let half = panel_capture::latest("playlist_tracks").expect("half rect");
        println!("CELL title={:?}", panel_capture::latest("cell_title"));
        println!("CELL artist={:?}", panel_capture::latest("cell_artist"));
        println!("HALF={half:?}");
        println!("HANDLE={:?}", panel_capture::latest("artist_handle"));
        // First data row sits below the painted column header.
        let row_y = half.top() + 60.0;
        let x = half.left() + 60.0;
        let click = vec![
            egui::Event::PointerButton {
                pos: egui::pos2(x, row_y),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: egui::pos2(x, row_y),
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        let actions = frame(&ctx, screen(click));
        assert_eq!(
            actions.actions,
            vec![PanelAction::LoadRow(hash)],
            "single click must load"
        );
    }

    // Given a rendered library entry column with one indexed file.
    // When the pointer double-clicks an entry row.
    // Then an AddTrack intent is emitted to the library side of the
    // panel's action output.
    #[test]
    fn library_row_double_click_emits_add_track() {
        let ctx = egui::Context::default();
        let mut tracks = crate::tracks::Tracks::default();
        let hash = TrackHash("h1".to_owned());
        let analysis = crate::tracks::Analysis {
            bpm: 174.0,
            key: djcore::key::Key {
                root: 8,
                mode: djcore::key::KeyMode::Minor,
            },
            duration_seconds: 200.0,
            grid: BeatGrid {
                grid_bpm: 174.0,
                anchor_seconds: 0.0,
                downbeats: vec![],
                beats: vec![],
                bars: vec![],
            },
            cues: CuePoints::default(),
        };
        tracks.upsert(crate::tracks::TrackRecord {
            hash: hash.clone(),
            tags: crate::tracks::TrackTags {
                title: "lib song".into(),
                artist: "lib act".into(),
                path: "/x/l.mp3".into(),
            },
            analysis: AnalysisState::Ready(analysis),
        });
        let st = PlaylistState::default();
        let mut library = crate::library::LibraryState::default();
        let entry_hash = hash.clone();
        library.entries.push(crate::library::LibraryEntry {
            root_id: 1,
            rel_path: "/x/l.mp3".into(),
            hash: entry_hash,
            title: "lib song".into(),
            artist: "lib act".into(),
            duration: Some(200.0),
            bpm: None,
            key: None,
            mtime_secs: 0,
            size_bytes: 0,
        });
        library.roots.push(crate::library::LibraryRoot {
            id: 1,
            path: "/x".into(),
        });
        let mut filter = String::new();
        let mut sort = crate::library::sort::SortState::default();
        let _ = &mut tracks;
        let screen = |events| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 720.0),
            )),
            events,
            ..egui::RawInput::default()
        };
        let mut state = st;
        let mut frame = |ctx: &egui::Context, raw: egui::RawInput| {
            let mut out = String::new();
            let mut bpm = 138.0_f32;
            let mut actions = PanelActions::default();
            let _ = ctx.run(raw, |ctx| {
                actions = panel(
                    ctx,
                    &mut state,
                    &crate::tracks::Tracks::default(),
                    &mut library,
                    &mut filter,
                    &mut sort,
                    render_slice(&mut out, &mut bpm),
                );
            });
            actions
        };

        for _ in 0..3 {
            let _ = frame(&ctx, screen(vec![]));
        }
        // Double-click inside the captured library entries column,
        // on its first row.
        panel_capture::reset();
        let _ = frame(&ctx, screen(vec![]));
        let _ = panel_capture::latest("entries");
        // Warm frames ran; now aim at the actual painted title cell of
        // the first row.
        let cell = panel_capture::latest("library_cell_title").expect("title cell rect");
        let pos = egui::pos2(cell.center().x, cell.center().y);
        // Each pointer phase gets its own frame so egui's click counter
        // sees a real two-click sequence.
        let moved = vec![egui::Event::PointerMoved(pos)];
        // Warm hit-testing: the widget under the pointer must exist for
        // a full frame before presses register against it.
        let _ = frame(&ctx, screen(moved.clone()));
        let _ = frame(&ctx, screen(moved.clone()));
        let btn = |pressed| {
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::NONE,
            }]
        };
        let _ = frame(&ctx, screen(btn(true)));
        let _ = frame(&ctx, screen(btn(false)));
        let _ = frame(&ctx, screen(btn(true)));
        let actions = frame(&ctx, screen(btn(false)));
        assert!(!actions.library.actions.is_empty(), "double click must add");
    }

    // Given the playlist rows with their right-pinned metadata columns.
    // When the BPM separator handle is dragged.
    // Then the persisted BPM width changes and survives restarts of the
    // context; and when the half shrinks, the metadata columns stay
    // glued to its right edge while Title absorbs the change.
    #[test]
    fn artist_title_handle_resizes_and_metadata_stay_pinned_right() {
        fn frame_block(
            ctx: &egui::Context,
            st: &mut PlaylistState,
            tracks: &Tracks,
            library: &mut crate::library::LibraryState,
            filter: &mut String,
            sort: &mut crate::library::sort::SortState,
        ) {
            panel_capture::reset();
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 720.0),
                    )),
                    ..egui::RawInput::default()
                },
                |ctx| {
                    let mut out = String::new();
                    let mut bpm = 138.0_f32;
                    panel(
                        ctx,
                        st,
                        tracks,
                        library,
                        filter,
                        sort,
                        RenderUiState {
                            mix_path: &mut out,
                            running: false,
                            can_render: false,
                            stage: None,
                            bpm: &mut bpm,
                        },
                    );
                },
            );
        }
        let build = || {
            let ctx = egui::Context::default();
            let mut tracks = crate::tracks::Tracks::default();
            let hash = TrackHash("h1".to_owned());
            let analysis = crate::tracks::Analysis {
                bpm: 174.0,
                key: djcore::key::Key {
                    root: 8,
                    mode: djcore::key::KeyMode::Minor,
                },
                duration_seconds: 200.0,
                grid: BeatGrid {
                    grid_bpm: 174.0,
                    anchor_seconds: 0.0,
                    downbeats: vec![],
                    beats: vec![],
                    bars: vec![],
                },
                cues: CuePoints::default(),
            };
            tracks.upsert(crate::tracks::TrackRecord {
                hash: hash.clone(),
                tags: crate::tracks::TrackTags {
                    title: "t".into(),
                    artist: "a".into(),
                    path: "/x".into(),
                },
                analysis: AnalysisState::Ready(analysis),
            });
            let st = PlaylistState {
                contents: Contents::Loaded(vec![hash]),
                ..Default::default()
            };
            (ctx, st, tracks)
        };

        // Drag the BPM handle left by 40px in one absolute move.
        let (ctx, mut st, tracks) = build();
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let mut sort = crate::library::sort::SortState::default();
        for _ in 0..3 {
            frame_block(&ctx, &mut st, &tracks, &mut library, &mut filter, &mut sort);
        }
        fn assert_close(a: f32, b: f32, what: &str) {
            assert!((a - b).abs() < 1e-3, "{what}: {a} vs {b}");
        }
        let before = panel_capture::latest("header_artist").expect("artist cell");
        // Handle sits just right of the artist column.
        let hx = panel_capture::latest("header_title")
            .expect("title cell")
            .left()
            - 2.0;
        let y = 500.0;
        let press = |x| {
            vec![egui::Event::PointerButton {
                pos: egui::pos2(x, y),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }]
        };
        let move_to = |x| vec![egui::Event::PointerMoved(egui::pos2(x, y))];
        frame_block(&ctx, &mut st, &tracks, &mut library, &mut filter, &mut sort);
        frame_block(&ctx, &mut st, &tracks, &mut library, &mut filter, &mut sort);
        // press+drag via raw input frames:
        {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 720.0),
                    )),
                    events: press(hx),
                    ..egui::RawInput::default()
                },
                |_| {},
            );
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 720.0),
                    )),
                    // Leftward: the artist column starts at its 50%-of-text-strip
                    // ceiling, so shrinking is the direction with room.
                    events: move_to(hx - 40.0),
                    ..egui::RawInput::default()
                },
                |ctx| {
                    let mut out = String::new();
                    let mut bpm = 138.0_f32;
                    panel_capture::reset();
                    panel(
                        &ctx.clone(),
                        &mut st,
                        &tracks,
                        &mut library,
                        &mut filter,
                        &mut sort,
                        RenderUiState {
                            mix_path: &mut out,
                            running: false,
                            can_render: false,
                            stage: None,
                            bpm: &mut bpm,
                        },
                    );
                },
            );
        }
        // Release + settle.
        {
            let _ = ctx.run(
                egui::RawInput {
                    screen_rect: Some(egui::Rect::from_min_size(
                        egui::Pos2::ZERO,
                        egui::vec2(1280.0, 720.0),
                    )),
                    events: release_all(),
                    ..egui::RawInput::default()
                },
                |ctx| {
                    let mut out = String::new();
                    let mut bpm = 138.0_f32;
                    panel(
                        &ctx.clone(),
                        &mut st,
                        &tracks,
                        &mut library,
                        &mut filter,
                        &mut sort,
                        RenderUiState {
                            mix_path: &mut out,
                            running: false,
                            can_render: false,
                            stage: None,
                            bpm: &mut bpm,
                        },
                    );
                },
            );
        }
        frame_block(&ctx, &mut st, &tracks, &mut library, &mut filter, &mut sort);
        let after = panel_capture::latest("header_artist").expect("artist after");
        assert!(
            after.width() < before.width() - 30.0,
            "artist shrank when handle dragged left: {} -> {}",
            before.width(),
            after.width()
        );
        // Space redistributed, not destroyed: Title grew accordingly.
        let title_before = panel_capture::latest("header_title").expect("title before");
        let _ = title_before;
        let title_after = panel_capture::latest("header_title")
            .expect("title after")
            .width();
        assert!(
            title_after > after.width() + 30.0,
            "title absorbed the space artist gave up"
        );

        // Pinning: duration stays flush against the half's right edge
        // under two different widths.
        let rect_a = panel_capture::latest("header_duration").expect("dur");
        ctx.data_mut(|d| d.insert_persisted(egui::Id::new(LIBRARY_FRACTION_KEY), 0.65_f32));
        frame_block(&ctx, &mut st, &tracks, &mut library, &mut filter, &mut sort);
        let half_wide = panel_capture::latest("playlist_tracks").expect("half");
        let rect_b = panel_capture::latest("header_duration").expect("dur wide");
        assert_close(
            half_wide.right() - rect_b.right(),
            0.0,
            "duration pinned to half's right edge",
        );
        let _ = rect_a;
    }

    /// Releases the pointer without caring where it is.
    fn release_all() -> Vec<egui::Event> {
        vec![egui::Event::PointerButton {
            pos: egui::Pos2::ZERO,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: egui::Modifiers::NONE,
        }]
    }

    // Given a library containing an entry with very long text fields.
    // When the four columns render.
    // Then every column sits at its imposed rect — oversized content
    // cannot push or shrink siblings.
    #[test]
    fn wide_library_content_cannot_displace_columns() {
        let mut state = PlaylistState::default();
        let tracks = crate::tracks::Tracks::default();
        let mut library = crate::library::LibraryState::default();
        library.entries.push(crate::library::store::LibraryEntry {
            root_id: 1,
            rel_path: "a/very/long/path/that/goes/on/and/on/and/on.mp3".into(),
            hash: TrackHash("h1".to_owned()),
            artist: "A".repeat(200),
            title: "T".repeat(200),
            bpm: None,
            key: None,
            duration: None,
            mtime_secs: 0,
            size_bytes: 0,
        });
        let mut filter = String::new();
        let mut sort = crate::library::sort::SortState::default();

        let ctx = egui::Context::default();
        let screen = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 720.0),
            )),
            ..egui::RawInput::default()
        };
        for _ in 0..3 {
            panel_capture::reset();
            let _unused = ctx.run(screen.clone(), |ctx| {
                panel(
                    ctx,
                    &mut state,
                    &tracks,
                    &mut library,
                    &mut filter,
                    &mut sort,
                    render_slice(&mut String::new(), &mut 138.0),
                );
            });
        }
        fn assert_close(a: f32, b: f32, what: &str) {
            assert!((a - b).abs() < 1e-3, "{what}: {a} vs {b}");
        }
        let get = |name: &str| panel_capture::latest(name).expect("captured");
        // 1280 viewport minus the panel's 8px inner margin on both
        // sides: halves split 1264 px evenly around a 12px divider. The
        // playlist columns sit at fixed offsets no matter how wide the
        // library's content measures.
        let panel_left = get("roots").left();
        let expected_tracks = 646.0 - 8.0 + panel_left;
        assert_close(
            get("playlist_tracks").left(),
            expected_tracks,
            "tracks x fixed",
        );
        // Playlists column: preferred width 170 inside its half.
        let expected_playlists = get("playlist_tracks").right();
        assert_close(
            get("playlists").left(),
            expected_playlists,
            "playlists abuts tracks",
        );
        // Library half and playlist half are separated by the 12px
        // divider — nothing overlaps and nothing shares an edge.
        let gap = get("playlist_tracks").left() - get("entries").right();
        assert_close(
            gap,
            crate::playlist::layout::DIVIDER_WIDTH,
            "divider gap between halves",
        );
        assert!(get("playlists").width() > 100.0, "playlists alive");
    }

    // Given a context pre-seeded with junk remembered geometry under
    // every panel id the old SidePanel row persisted, and a fresh
    // context with none.
    // When the four columns render on both.
    // Then the column rects are identical — stale layout state cannot
    // influence the imposed split.
    #[test]
    fn seeded_geometry_cannot_influence_column_layout() {
        const REMOVED_PANEL_IDS: [&str; 3] =
            ["library_roots", "library_entries", "playlist_tracks"];

        let run = |ctx: &egui::Context| {
            let mut state = PlaylistState::default();
            let tracks = crate::tracks::Tracks::default();
            let mut library = crate::library::LibraryState::default();
            let mut filter = String::new();
            let mut sort = crate::library::sort::SortState::default();
            let screen = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 720.0),
                )),
                ..egui::RawInput::default()
            };
            for _ in 0..3 {
                panel_capture::reset();
                let _unused = ctx.run(screen.clone(), |ctx| {
                    panel(
                        ctx,
                        &mut state,
                        &tracks,
                        &mut library,
                        &mut filter,
                        &mut sort,
                        render_slice(&mut String::new(), &mut 138.0),
                    );
                });
            }
            [
                ("roots", panel_capture::latest("roots")),
                ("entries", panel_capture::latest("entries")),
                ("playlist_tracks", panel_capture::latest("playlist_tracks")),
                ("playlists", panel_capture::latest("playlists")),
            ]
            .map(|(name, rect)| (name, rect.expect("captured")))
        };

        let pristine = run(&egui::Context::default());

        let seeded = egui::Context::default();
        for id in REMOVED_PANEL_IDS {
            // Whatever shape the old panels used to persist their state
            // under these ids, bury every likely key with junk: the row
            // must not read any of it.
            seeded.data_mut(|d| {
                d.insert_temp(
                    egui::Id::new(id),
                    egui::Rect::from_min_size(egui::pos2(7_000.0, 7_000.0), egui::vec2(1.0, 1.0)),
                );
                d.insert_persisted(
                    egui::Id::new(id),
                    egui::Rect::from_min_size(egui::pos2(8_000.0, 8_000.0), egui::vec2(9.0, 9.0)),
                );
            });
            // Also bury the divider fraction the layout legitimately
            // reads: garbage values must clamp to sane bounds rather
            // than corrupt the split.
            seeded.data_mut(|d| {
                d.insert_persisted(egui::Id::new(LIBRARY_FRACTION_KEY), f32::NAN);
            });
        }
        let polluted = run(&seeded);

        assert_eq!(
            pristine, polluted,
            "column rects must not depend on remembered state"
        );
    }
    // Given a bare egui context drawing one click widget inside two
    // nested scopes (the shape of our column/row containment).
    // When the pointer moves onto it across warm frames and clicks.
    // Then clicked() fires on the release frame — hit-testing works in
    // this environment even under nested scoping.
    #[test]
    fn minimal_hover_click_works_headless() {
        let ctx = egui::Context::default();
        let screen = |events| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(800.0, 600.0),
            )),
            events,
            ..egui::RawInput::default()
        };
        let pos = egui::pos2(100.0, 30.0);
        let mut results: Vec<(bool, bool, bool)> = Vec::new();
        let mut frames: Vec<Vec<egui::Event>> = vec![
            vec![egui::Event::PointerMoved(pos)],
            vec![egui::Event::PointerMoved(pos)],
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            }],
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            }],
        ];
        for events in frames.drain(..) {
            let _ = ctx.run(screen(events), |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.scope_builder(
                        egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(400.0, 300.0),
                        )),
                        |outer| {
                            outer.set_clip_rect(egui::Rect::from_min_size(
                                egui::Pos2::ZERO,
                                egui::vec2(400.0, 300.0),
                            ));
                            outer.scope_builder(
                                egui::UiBuilder::new().max_rect(egui::Rect::from_min_size(
                                    egui::pos2(40.0, 20.0),
                                    egui::vec2(200.0, 60.0),
                                )),
                                |inner| {
                                    let (rect, resp) = inner.allocate_exact_size(
                                        egui::vec2(120.0, 24.0),
                                        egui::Sense::click(),
                                    );
                                    eprintln!("PROBE rect={rect:?}");
                                    results.push((
                                        resp.hovered(),
                                        resp.clicked(),
                                        rect.contains(pos),
                                    ));
                                },
                            );
                        },
                    );
                });
            });
        }
        eprintln!("HOVERPROBE {:?}", results);
        let last = results.last().copied().unwrap_or((false, false, false));
        assert!(
            last.1 && last.2,
            "click must land and register: {results:?}"
        );
    }

    fn ready_track(hash: &str, title: &str) -> crate::tracks::TrackRecord {
        crate::tracks::TrackRecord {
            hash: TrackHash(hash.to_owned()),
            tags: crate::tracks::TrackTags {
                title: title.into(),
                artist: format!("artist of {title}"),
                path: format!("/x/{title}.mp3").into(),
            },
            analysis: AnalysisState::Ready(crate::tracks::Analysis {
                bpm: 174.0,
                key: djcore::key::Key {
                    root: 8,
                    mode: djcore::key::KeyMode::Minor,
                },
                duration_seconds: 200.0,
                grid: BeatGrid {
                    grid_bpm: 174.0,
                    anchor_seconds: 0.0,
                    downbeats: vec![],
                    beats: vec![],
                    bars: vec![],
                },
                cues: CuePoints::default(),
            }),
        }
    }

    /// Headless driver for the row-layout regression tests: a fully
    /// ready 3-track playlist rendered through the real [`super::panel`].
    struct RowHarness {
        ctx: egui::Context,
        state: PlaylistState,
        tracks: crate::tracks::Tracks,
        library: crate::library::LibraryState,
        filter: String,
        sort: crate::library::sort::SortState,
    }

    impl RowHarness {
        fn new() -> Self {
            let mut tracks = crate::tracks::Tracks::default();
            let hashes: Vec<TrackHash> = ["one", "two", "three"]
                .iter()
                .enumerate()
                .map(|(i, title)| {
                    let hash = TrackHash(format!("t{}", i + 1));
                    tracks.upsert(ready_track(&hash.0, title));
                    hash
                })
                .collect();
            Self {
                ctx: egui::Context::default(),
                state: PlaylistState {
                    contents: Contents::Loaded(hashes),
                    ..Default::default()
                },
                tracks,
                library: crate::library::LibraryState::default(),
                filter: String::new(),
                sort: crate::library::sort::SortState::default(),
            }
        }

        /// Renders one frame with the given raw input, capturing row and
        /// panel rects into `panel_capture`; returns emitted actions.
        fn frame(&mut self, raw: egui::RawInput) -> PanelActions {
            panel_capture::reset();
            let Self {
                ctx,
                state,
                tracks,
                library,
                filter,
                sort,
            } = self;
            let mut out = String::new();
            let mut bpm = 138.0_f32;
            let mut actions = PanelActions::default();
            let _ = ctx.run(raw, |ctx| {
                actions = panel(
                    ctx,
                    state,
                    tracks,
                    library,
                    filter,
                    sort,
                    super::tests::render_slice(&mut out, &mut bpm),
                );
            });
            actions
        }
    }

    fn screen(events: Vec<egui::Event>) -> egui::RawInput {
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 720.0),
            )),
            events,
            ..egui::RawInput::default()
        }
    }

    // Given three ready playlist tracks.
    // When the panel renders warm frames.
    // Then three distinct row strips are laid out: pairwise disjoint,
    // vertically contiguous, uniform height, equal width.
    #[test]
    fn multi_row_playlist_renders_distinct_rows() {
        let mut harness = RowHarness::new();

        // When warm-up frames settle the layout.
        for _ in 0..3 {
            harness.frame(screen(vec![]));
        }

        // Then every track occupies its own strip.
        let rows = panel_capture::rows();
        assert_eq!(rows.len(), 3, "each visible track lays out one row");
        for pair in rows.windows(2) {
            let [a, b] = [pair[0], pair[1]];
            assert!(
                b.top() >= a.bottom() - f32::EPSILON && b.top() < a.bottom() + 1e-3,
                "row strips must abut: {a:?} then {b:?}"
            );
        }
        // And all strips share the row style's geometry.
        assert!(
            rows.windows(2)
                .all(|p| (p[0].height() - p[1].height()).abs() < 1e-3),
            "uniform row heights"
        );
        assert!(
            rows.windows(2).all(|p| p[0].width() == p[1].width()),
            "equal row widths"
        );
    }

    // Given a warmed 3-track playlist.
    // When the pointer clicks dead-center on the second row.
    // Then exactly one LoadRow action fires carrying that row's hash.
    #[test]
    fn click_nth_row_loads_nth_track() {
        let mut harness = RowHarness::new();
        for _ in 0..3 {
            harness.frame(screen(vec![]));
        }
        let rows = panel_capture::rows();
        assert_eq!(rows.len(), 3);

        // When clicking the middle of the SECOND row.
        let target = rows[1].center();
        let click = vec![
            egui::Event::PointerMoved(target),
            egui::Event::PointerButton {
                pos: target,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::NONE,
            },
            egui::Event::PointerButton {
                pos: target,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::NONE,
            },
        ];
        let actions = harness.frame(screen(click));

        // Then the second track is the only thing loaded.
        assert_eq!(
            actions.actions,
            vec![PanelAction::LoadRow(TrackHash("t2".to_owned()))],
            "the clicked row decides what loads"
        );
    }

    // Given a playlist longer than the scroll viewport.
    // When the view scrolls down by an exact number of row strides.
    // Then each visible row's top sits at a whole multiple of the row
    // stride from the strip top.
    #[test]
    fn rows_align_with_scroll_offset() {
        let ctx = egui::Context::default();
        let mut tracks = crate::tracks::Tracks::default();
        let hashes: Vec<TrackHash> = (0..40u32)
            .map(|i| {
                let h = TrackHash(format!("r{i}"));
                tracks.upsert(ready_track(&h.0, &format!("track {i}")));
                h
            })
            .collect();
        let mut state = PlaylistState {
            contents: Contents::Loaded(hashes),
            ..Default::default()
        };
        let mut library = crate::library::LibraryState::default();
        let mut filter = String::new();
        let mut sort = crate::library::sort::SortState::default();
        let screen = |events| egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 720.0),
            )),
            events,
            ..egui::RawInput::default()
        };
        let mut run = |events: Vec<egui::Event>| {
            panel_capture::reset();
            let mut out = String::new();
            let mut bpm = 138.0_f32;
            let _ = ctx.run(screen(events), |ctx| {
                panel(
                    ctx,
                    &mut state,
                    &tracks,
                    &mut library,
                    &mut filter,
                    &mut sort,
                    super::tests::render_slice(&mut out, &mut bpm),
                );
            });
        };

        // When scrolling the playlist-entries viewport down some rows.
        run(vec![]);
        run(vec![]);
        // The observed stride between adjacent laid-out strips IS the
        // virtualization grid; deriving it keeps this test decoupled
        // from style internals.
        let warm_rows = panel_capture::rows();
        assert!(warm_rows.len() >= 2, "viewport shows several rows");
        let stride = warm_rows[1].top() - warm_rows[0].top();
        run(vec![egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -(stride * 5.0)),
            modifiers: egui::Modifiers::NONE,
        }]);
        // Let the smooth-scroll animation converge before measuring;
        // intermediate frames sit at deliberately fractional offsets.
        for _ in 0..40 {
            run(vec![]);
        }

        // Then every visible row sits at the same stride-grid phase as
        // before scrolling — whole rows slid under the viewport and no
        // per-row jitter crept in.
        let rows = panel_capture::rows();
        assert!(!rows.is_empty(), "scrolled view still renders rows");
        let phase_of = |top: f32| {
            let p = top.rem_euclid(stride);
            if (stride - p).abs() < 1e-3 { 0.0 } else { p }
        };
        let base = phase_of(warm_rows[0].top());
        for rect in &rows {
            assert!(
                (phase_of(rect.top()) - base).abs() < 1e-3,
                "row drifted off the virtualization grid: {rect:?} vs base phase {base}"
            );
        }
    }
}
