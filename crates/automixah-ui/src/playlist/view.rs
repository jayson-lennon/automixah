//! egui rendering for the playlist section (bottom panel).
//!
//! The panel is a full-width, user-resizable strip below the waveform:
//! the left column lists playlists (select / ＋ new / right-click
//! rename-delete; rename swaps the row for an in-place editor), the
//! right column shows the selected playlist's tracks. Row rendering
//!
//! All state lives on [`crate::playlist::PlaylistState`]; this module
//! only paints and emits user intents back through [`PanelActions`].

use egui::Color32;

use crate::playlist::queue::RowId;
use crate::playlist::{
    Contents, PlaylistRow, PlaylistState, RenameOutcome, RowStatus, rename_outcome,
};

/// User intents emitted by the panel (the app wires them to stores).
#[derive(Debug, PartialEq, Eq)]
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
    /// The Add… button was clicked (the app opens the file dialog).
    AddTracks,
    /// A ready row was clicked (load into the grid editor).
    LoadRow(RowId),
    /// A row was dragged onto another row's slot (reorder).
    MoveRow {
        /// Row being dragged.
        from: RowId,
        /// Slot the drag ended on.
        to: RowId,
    },
    /// Remove was chosen in a row's context menu.
    RemoveRow {
        /// Row to remove.
        row: RowId,
        /// Its position (store remove is position-addressed).
        position: i64,
    },
}

/// Collected user intents from one panel paint; drained by the app.
#[derive(Debug, Default)]
pub struct PanelActions {
    /// Intents in paint order.
    pub actions: Vec<PanelAction>,
}

/// Draws the whole playlist panel. Returns the collected intents.
pub fn panel(ctx: &egui::Context, state: &mut PlaylistState) -> PanelActions {
    let mut actions = PanelActions::default();
    egui::TopBottomPanel::bottom("playlist_panel")
        .resizable(true)
        .default_height(220.0)
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.strong("Playlists");
                if ui.button("New").clicked() {
                    actions.actions.push(PanelAction::NewPlaylist);
                }
                ui.separator();
                if state.selected.is_some() {
                    let add = ui.button("Add…");
                    if state.add_pending {
                        ui.spinner();
                    }
                    if add.clicked() {
                        actions.actions.push(PanelAction::AddTracks);
                    }
                }
                if let Some(selected) = state.selected {
                    ui.label(
                        state
                            .playlists
                            .iter()
                            .find(|p| p.id == selected)
                            .map_or("", |p| p.name.as_str()),
                    );
                }
            });
            ui.separator();
            egui::SidePanel::left("playlist_list")
                .resizable(false)
                .default_width(160.0)
                .show_inside(ui, |ui| {
                    playlist_list(ui, state, &mut actions);
                });
            egui::CentralPanel::default().show_inside(ui, |ui| {
                rows(ui, state, &mut actions);
            });
        });
    actions
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
fn rows(ui: &mut egui::Ui, state: &mut PlaylistState, actions: &mut PanelActions) {
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
        Contents::Loaded(rows) if rows.is_empty() => {
            ui.centered_and_justified(|ui| ui.weak("empty playlist. Add… to browse"));
            return;
        }
        Contents::Loaded(_) => {}
    }
    let Contents::Loaded(rows) = &mut state.contents else {
        return;
    };
    let rows = &*rows;
    let row_height = ui.text_style_height(&egui::TextStyle::Body) * 1.4;
    let mut drag_source: Option<RowId> = None;
    let mut drop_target: Option<RowId> = None;
    let mut prev_key: Option<djcore::key::Key> = None;
    egui::ScrollArea::vertical().show_rows(ui, row_height, rows.len(), |ui, range| {
        for i in range {
            let row = &rows[i];
            let interactive = row.is_ready();
            let dragging_row = drag_source.is_some();
            let response = row_ui(ui, row, prev_key.clone(), interactive, row_height);
            // Insertion-line preview: while a drag is live, the hovered
            // row shows the insertion line above or below its slot,
            // decided by the pointer's half of the slot.
            if dragging_row
                && response.hovered()
                && Some(row.row_id) != drag_source
                && let Some(pointer) = response.interact_pointer_pos()
            {
                let offset = insertion_offset_in_slot(
                    pointer.y,
                    response.rect.top(),
                    response.rect.height(),
                );
                let y = if offset == 0 {
                    response.rect.top()
                } else {
                    response.rect.bottom()
                };
                let painter = ui.painter();
                painter.line_segment(
                    [
                        egui::pos2(response.rect.left(), y),
                        egui::pos2(response.rect.right(), y),
                    ],
                    egui::Stroke::new(2.0, egui::Color32::from_rgb(255, 210, 60)),
                );
            }
            if interactive {
                if response.clicked() {
                    actions.actions.push(PanelAction::LoadRow(row.row_id));
                }
                if response.drag_started() {
                    drag_source = Some(row.row_id);
                }
                if response.drag_stopped() {
                    drop_target = Some(row.row_id);
                }
            }
            response.context_menu(|ui| {
                if ui.button("Remove").clicked() {
                    actions.actions.push(PanelAction::RemoveRow {
                        row: row.row_id,
                        position: row.position,
                    });
                    ui.close();
                }
            });
            if let Some(key) = row.key.clone() {
                prev_key = Some(key);
            }
        }
    });
    if let (Some(from), Some(to)) = (drag_source, drop_target)
        && from != to
    {
        actions.actions.push(PanelAction::MoveRow { from, to });
    }
}

/// One track row: an interaction rect plus painted content.
///
/// Painting (not widgets) keeps scrolling with hundreds of rows cheap;
/// the response carries click/drag/context-menu sense.
fn row_ui(
    ui: &mut egui::Ui,
    row: &PlaylistRow,
    prev_key: Option<djcore::key::Key>,
    interactive: bool,
    row_height: f32,
) -> egui::Response {
    let desired = egui::vec2(ui.available_width(), row_height);
    let (rect, response) = ui.allocate_at_least(desired, egui::Sense::click_and_drag());
    let hovered = response.hovered();
    let painter = ui.painter_at(rect);
    let bg = if !interactive {
        dimmed_row_color(ui)
    } else if hovered {
        hover_row_color(ui)
    } else {
        base_row_color(ui)
    };
    painter.rect_filled(rect.shrink(1.0), 2.0, bg);
    paint_row_content(ui, &painter, rect, row, prev_key, interactive);
    response.on_hover_text(format!("{}", row.path.display()))
}

/// Row background by state (non-ready rows dim, hover lightens).
fn base_row_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_gray(30)
    } else {
        Color32::from_gray(248)
    }
}

/// See [`base_row_color`].
fn hover_row_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_gray(45)
    } else {
        Color32::from_gray(232)
    }
}

/// See [`base_row_color`].
fn dimmed_row_color(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_gray(22)
    } else {
        Color32::from_gray(252)
    }
}

/// Paints status icon, artist–title, and right-aligned metadata.
fn paint_row_content(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    rect: egui::Rect,
    row: &PlaylistRow,
    prev_key: Option<djcore::key::Key>,
    interactive: bool,
) {
    let font_id = egui::FontId::proportional(ui.text_style_height(&egui::TextStyle::Body) * 0.9);
    let strong_color = ui.visuals().strong_text_color();
    let weak_color = ui.visuals().weak_text_color();
    let galley = |text: &str, color: Color32| {
        let mut layout = egui::text::LayoutJob::default();
        layout.append(
            text,
            0.0,
            egui::text::TextFormat {
                font_id: font_id.clone(),
                color,
                ..Default::default()
            },
        );
        ui.fonts(|f| f.layout_job(layout))
    };
    let center_y = rect.center().y;
    let mut x = rect.left() + 8.0;

    // Status / affordance glyph.
    let (glyph, color) = match &row.status {
        RowStatus::Ready => (" ", strong_color),
        RowStatus::Queued => ("🕓", weak_color),
        RowStatus::Analyzing => ("⭕", weak_color),
        RowStatus::Failed(_) => ("!", Color32::RED),
    };
    let icon_size = ui.text_style_height(&egui::TextStyle::Body).min(16.0);
    if row.status == RowStatus::Analyzing {
        // Animated spinner at the glyph slot; repaint comes from the
        // spinner itself.
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(x + icon_size / 2.0, center_y),
            egui::vec2(icon_size, icon_size),
        );
        egui::Spinner::new()
            .color(weak_color)
            .paint_at(ui, icon_rect);
        x += icon_size + 8.0;
    } else {
        let g = galley(glyph, color);
        painter.galley(egui::pos2(x, center_y - g.size().y / 2.0), g.clone(), color);
        x += g.size().x + 8.0;
    }

    // Artist – title (dimmed while pending).
    let title = if row.artist.is_empty() {
        row.title.clone()
    } else {
        format!("{} – {}", row.artist, row.title)
    };
    let title_color = if interactive {
        strong_color
    } else {
        weak_color
    };
    let t = galley(&title, title_color);
    painter.galley(
        egui::pos2(x, center_y - t.size().y / 2.0),
        t.clone(),
        title_color,
    );

    // Right-aligned metadata: duration, key (colored), BPM.
    let bpm = row
        .bpm
        .map_or_else(|| "--".to_owned(), |b| format!("{b:.0}"));
    let duration = row.duration.map_or_else(
        || "---".to_owned(),
        |d| format!("{}:{:02}", (d / 60.0) as u32, (d % 60.0) as u32),
    );
    let key_text = row.key.as_ref().map_or_else(
        || "--".to_owned(),
        |k| k.format_with(djcore::key::KeyFormat::Camelot),
    );
    let key_color = key_display_color(row.key.clone(), prev_key, strong_color);
    let d_galley = galley(&duration, weak_color);
    let k_galley = galley(&key_text, key_color);
    let b_galley = galley(&bpm, weak_color);
    let gap = 14.0;
    let mut rx = rect.right() - 8.0;
    for g in [&d_galley, &k_galley, &b_galley] {
        rx -= g.size().x;
        painter.galley(
            egui::pos2(rx, center_y - g.size().y / 2.0),
            g.clone(),
            weak_color,
        );
        rx -= gap;
    }
}

/// Whether a drag over a slot splices before (0) or after (1) it.
///
/// The upper half of a slot inserts before it; the lower half after.
#[must_use]
pub fn insertion_offset_in_slot(pointer_y: f32, slot_top: f32, slot_height: f32) -> usize {
    let half = slot_height / 2.0;
    if (pointer_y - slot_top) < half { 0 } else { 1 }
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
mod tests {
    use super::*;

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

    // Given the inline rename editor focused with a typed name.
    // When Enter is pressed.
    // Then a rename action with the typed name is emitted and the
    // editor closes.
    #[test]
    fn inline_editor_enter_commits() {
        let mut state = editing_state();
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let _ = panel(ctx, &mut state);
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
        let _ = ctx.run(input, |ctx| {
            actions = panel(ctx, &mut state);
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
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let _ = panel(ctx, &mut state);
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
        let _ = ctx.run(input, |ctx| {
            actions = panel(ctx, &mut state);
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
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            let _ = panel(ctx, &mut state);
        });
        state.rename.buffer = "new".to_owned();
        // Click-away is another widget grabbing focus during a frame.
        let mut actions = PanelActions::default();
        let _ = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.ctx()
                    .memory_mut(|m| m.request_focus(egui::Id::new("click_away_target")));
                let _ = ui.allocate_space(egui::vec2(1.0, 1.0));
            });
            actions = panel(ctx, &mut state);
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
    // When computing its display color.
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
