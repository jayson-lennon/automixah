//! egui rendering for the library columns (roots + entries) of the
//! bottom panel.
//!
//! Pure painting from derived state: roots and entries come from
//! [`crate::library::LibraryState`]; the visible/filtered/sorted set is
//! derived each frame from `(entries, filter, sort)`; duplicate dimming
//! derives from the selected playlist's hash list. This module only
//! paints and emits user intents back through [`LibraryActions`].

use std::collections::HashSet;

use egui_extras::{Column, TableBuilder};

use crate::library::filter;
use crate::library::sort::{self, SortColumn, SortState};
use crate::library::store::{LibraryEntry, LibraryRoot};
use crate::library::{LibraryState, ScanProgress};
use crate::tracks::AnalysisState;
use automixah_engine::timeline::types::TrackHash;

/// User intents emitted by the library columns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LibraryAction {
    /// The Add Folder… button was clicked (the app opens the directory
    /// picker and, on success, adds the root and starts a scan).
    AddRoot,
    /// The Rescan button was clicked (ignored while a scan runs).
    Rescan,
    /// Remove was chosen in a root's context menu.
    RemoveRoot {
        /// The root's database id.
        id: i64,
    },
    /// An entry was double-clicked (add to the selected playlist).
    AddTrack {
        /// The entry's content hash.
        hash: TrackHash,
    },
    /// An entry was middle-clicked (play it in the instant preview player).
    PreviewTrack {
        /// The entry's content hash.
        hash: TrackHash,
    },
}

/// Collected intents from one library-columns paint.
#[derive(Debug, Default, Clone)]
pub struct LibraryActions {
    /// Intents in paint order.
    pub actions: Vec<LibraryAction>,
}

/// Draws the roots column (Add Folder…, Rescan, progress, root rows).
pub fn roots_column(ui: &mut egui::Ui, state: &LibraryState, actions: &mut LibraryActions) {
    ui.horizontal(|ui| {
        if ui.button("Add Folder…").clicked() {
            actions.actions.push(LibraryAction::AddRoot);
        }
        let rescan = ui.add_enabled(!state.scanning, egui::Button::new("Rescan"));
        if rescan.clicked() {
            actions.actions.push(LibraryAction::Rescan);
        }
    });
    if state.scanning {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.weak(progress_text(state.progress));
        });
    }
    ui.separator();
    egui::ScrollArea::vertical().show(ui, |ui| {
        root_rows(ui, state, actions);
    });
}

/// Human text for one scan-progress echo.
fn progress_text(progress: Option<ScanProgress>) -> String {
    match progress {
        Some(p) => format!("{} / {} files", p.files_done, p.files_seen),
        None => "scanning…".to_owned(),
    }
}

/// One row per root: basename + tooltip, context-menu Remove (disabled
/// while scanning).
fn root_rows(ui: &mut egui::Ui, state: &LibraryState, actions: &mut LibraryActions) {
    for root in &state.roots {
        let label = root.path.file_name().map_or_else(
            || root.path.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        );
        let response = ui
            .selectable_label(false, &label)
            .on_hover_text(root.path.display().to_string());
        response.context_menu(|ui| {
            let remove = ui.add_enabled(!state.scanning, egui::Button::new("Remove"));
            if remove.clicked() {
                actions
                    .actions
                    .push(LibraryAction::RemoveRoot { id: root.id });
                ui.close();
            }
        });
    }
    if state.roots.is_empty() {
        ui.weak("no library folders");
    }
}

// Frozen widths for the library section's two columns.
//
/// Frozen roots-column width.
///
/// This column has no resize grip: its right edge doubles as the visual
/// seam beside the entries table, and egui lays an invisible drag grab
/// over the whole edge of any resizable side panel, so a gesture there
/// silently resizes this panel and shifts every later column. A fixed
/// width keeps the section's geometry immune to stray gestures.
pub const ROOTS_COLUMN_WIDTH: f32 = 220.0;

/// Frozen width of the entries-table column.
///
/// Fixed alongside [`ROOTS_COLUMN_WIDTH`] by the same decoupling rule;
/// sized wide enough for the six columns' header labels at comfortable
/// reading width. On very narrow windows it is clamped to what remains.
pub const ENTRIES_COLUMN_WIDTH: f32 = 530.0;

/// Draws the entries column: search box, summary, sortable table.
///
/// Header left clicks sort ascending / toggle descending, right click
/// restores the default ordering; row double-click emits an add intent.
pub fn entries_column(
    ui: &mut egui::Ui,
    state: &LibraryState,
    tracks: Option<&crate::tracks::Tracks>,
    filter_buffer: &mut String,
    sort_state: &mut SortState,
    selected_hashes: Option<&[TrackHash]>,
    actions: &mut LibraryActions,
) {
    ui.horizontal(|ui| {
        let search = egui::TextEdit::singleline(filter_buffer)
            .hint_text("search (a, b = both)")
            .desired_width(ui.available_width());
        ui.add(search);
    });
    let terms = filter::parse(filter_buffer);
    let visible = sort::visible_entries(&state.entries, &terms, sort_state);
    ui.weak(format!(
        "{} / {} tracks",
        visible.len(),
        state.entries.len()
    ));

    let height = row_height(ui);
    let style = RowStyle::resolve(ui);
    // Metadata columns are exact-width so their content can never drive
    // column sizing. egui_extras' `Column::auto()` measures per-frame
    // rendered cells; combined with the virtualized body this feeds back
    // into itself and long text squeezed other columns mid-value or to
    // near-zero (the egui_extras #3178 class of defect). Fixed widths
    // derived once from the header text make the columns content-proof.
    let [bpm_w, key_w, duration_w] = [
        SortColumn::Bpm.label(),
        SortColumn::Key.label(),
        SortColumn::Duration.label(),
    ]
    .map(|label| header_column_width(ui.ctx(), label, &style.font));
    let glyph_w = single_line_width(ui.ctx(), "\u{25c9}", &style.font) + 16.0;
    TableBuilder::new(ui)
        .striped(false)
        .vscroll(true)
        // The table fills its container exactly; shrink-to-content would
        // re-couple it to cell text and restart the squeeze loop.
        .auto_shrink([false, false])
        // `clip` disables egui_extras' "max 8px shrinkage per frame"
        // rate limiter (their compatibility hack for non-clip columns),
        // which made leftward separator drags lag many frames behind
        // the pointer. Our body cells elide instead of reflowing, so
        // clipping costs nothing.
        // `at_most` caps keep both text columns from collectively
        // starving the remainder (folder) column on narrow windows.
        // Status glyph: narrow exact column, fixed like the playlist's.
        .column(Column::exact(glyph_w))
        .column(
            Column::initial(120.0)
                .resizable(true)
                .clip(true)
                .at_most(240.0),
        )
        .column(
            Column::initial(170.0)
                .resizable(true)
                .clip(true)
                .at_most(300.0),
        )
        .column(Column::exact(bpm_w))
        .column(Column::exact(key_w))
        .column(Column::exact(duration_w))
        // Same load-bearing `clip` as the playlist title column: an
        // unclipped remainder gets clamped to the widest content ever
        // measured and overflows the table, pushing earlier columns
        // off-screen (egui_extras max_used_widths feedback).
        .column(Column::remainder().clip(true))
        .header(height, |mut header| {
            header.col(|ui| {
                ui.add(egui::Label::new(egui::RichText::new(" ").weak()).truncate());
            });
            for column in SortColumn::all() {
                header.col(|ui| header_cell(ui, column, sort_state));
            }
        })
        .body(|body| {
            let entries = &state.entries;
            body.rows(height, visible.len(), |row| {
                // The visible list was just built; the row index is the
                // position in it.
                let visible_index = row.index();
                let mut row = row;
                entry_row(
                    &mut row,
                    tracks,
                    &entries[visible[visible_index]],
                    &terms,
                    &style,
                    selected_hashes,
                    actions,
                );
            });
        });
}

/// One virtualized table row: status glyph, six cells derived from the
/// entry — each carrying its own fuzzy-match highlights, duplicate
/// dimming over all, full-path tooltip, double-click add, middle-click
/// preview.
#[allow(clippy::too_many_arguments)]
fn entry_row(
    row: &mut egui_extras::TableRow<'_, '_>,
    tracks: Option<&crate::tracks::Tracks>,
    entry: &LibraryEntry,
    terms: &[filter::FilterTerm],
    style: &RowStyle,
    selected_hashes: Option<&[TrackHash]>,
    actions: &mut LibraryActions,
) {
    let duplicate = selected_hashes.is_some_and(|hashes| hashes.contains(&entry.hash));
    let spans = filter::spans(entry, terms);

    // The row gesture is the union of its cells' responses.
    let mut row_response: Option<egui::Response> = None;

    // Status glyph derived from the track database's analysis state —
    // same visual language as playlist rows (merged from main).
    {
        let analysis = tracks
            .and_then(|db| db.get(&entry.hash))
            .map(|r| &r.analysis);
        let state = analysis.cloned().unwrap_or(AnalysisState::Queued);
        let analyzing = matches!(state, AnalysisState::Analyzing);
        let (glyph, color) = analysis_glyph(state, style.main, style.metadata);
        let mut cell_response: Option<egui::Response> = None;
        row.col(|ui| {
            cell_response = Some(glyph_cell(ui, analyzing, glyph, color, style));
        });
        if let Some(r) = cell_response {
            row_response = unite_row_response(row_response, r);
        }
    }

    for (text, indices, tone) in [
        (
            entry.artist.clone(),
            spans.artist.iter().copied().collect(),
            style.main,
        ),
        (
            entry.title.clone(),
            spans.title.iter().copied().collect(),
            style.main,
        ),
        (sort::bpm_text(entry.bpm), HashSet::new(), style.metadata),
        (
            sort::key_text(entry.key.as_ref()),
            HashSet::new(),
            style.metadata,
        ),
        (
            sort::duration_text(entry.duration),
            HashSet::new(),
            style.metadata,
        ),
        (
            folder_display(entry),
            spans.path.iter().copied().collect(),
            style.metadata,
        ),
    ] {
        // Duplicate rows dim everything except the fuzzy highlights.
        let cell_tone = if duplicate { style.metadata } else { tone };
        // Union the CELL widget's response (click flags live there);
        // the `row.col` surface itself is hover-only.
        let mut cell_response: Option<egui::Response> = None;
        row.col(|ui| {
            cell_response = Some(text_cell(ui, text, indices, cell_tone, style, duplicate));
        });
        if let Some(r) = cell_response {
            row_response = unite_row_response(row_response, r);
        }
    }

    if let Some(response) = row_response {
        if let Some(action) = add_action_for_row(&entry.hash, response.double_clicked()) {
            actions.actions.push(action);
        }
        if let Some(action) = preview_action_for_row(
            &entry.hash,
            response.clicked_by(egui::PointerButton::Middle),
            response.dragged_by(egui::PointerButton::Middle),
        ) {
            actions.actions.push(action);
        }
        response.on_hover_text(entry.rel_path.display().to_string());
    }
}

/// Colors and metrics shared by every cell of the entries table;
/// resolved once per frame from the theme.
pub(crate) struct RowStyle {
    /// Artist/title text when not a duplicate.
    pub(crate) main: egui::Color32,
    /// BPM/key/duration/folder text.
    pub(crate) metadata: egui::Color32,
    /// Fuzzy-match glyph highlight.
    pub(crate) highlight: egui::Color32,
    /// Body font at the hand-painted rows' size.
    pub(crate) font: egui::FontId,
    /// Row height in points.
    pub(crate) height: f32,
}

impl RowStyle {
    /// Reads theme colors once; cells stay pure painters.
    pub(crate) fn resolve(ui: &egui::Ui) -> Self {
        Self {
            main: ui.visuals().strong_text_color(),
            metadata: ui.visuals().weak_text_color(),
            highlight: egui::Color32::from_rgb(255, 210, 60),
            font: egui::FontId::proportional(ui.text_style_height(&egui::TextStyle::Body) * 0.9),
            height: row_height(ui),
        }
    }
}

/// One clickable header cell: label + direction arrow; left click
/// sorts/toggles, right click restores the default ordering.
fn header_cell(ui: &mut egui::Ui, column: SortColumn, sort_state: &mut SortState) {
    let label = format!("{} {}", column.label(), sort_state.arrow(column));
    // Truncating: even an exact-width column can be narrower than the
    // label+arrow under a theme change, and a wrapping header row is
    // what breaks row alignment.
    let response = ui.add(
        egui::Label::new(egui::RichText::new(label).weak())
            .truncate()
            .sense(egui::Sense::click()),
    );
    if response.clicked() {
        *sort_state = sort_state.on_click(column);
    }
    if response.secondary_clicked() {
        *sort_state = sort_state.on_right_click();
    }
}

/// Converts one entry-row's unioned cell response into its add intent:
/// only a double click adds.
#[must_use]
pub(crate) fn add_action_for_row(hash: &TrackHash, double_clicked: bool) -> Option<LibraryAction> {
    double_clicked.then(|| LibraryAction::AddTrack { hash: hash.clone() })
}

/// Unions an accumulated row response with the next cell's.
pub(crate) fn unite_row_response(
    acc: Option<egui::Response>,
    next: egui::Response,
) -> Option<egui::Response> {
    Some(match acc {
        Some(prev) => prev.union(next),
        None => next,
    })
}

/// Row background tones.
/// Folder column display text (root-level files show nothing).
fn folder_display(entry: &LibraryEntry) -> String {
    entry
        .rel_path
        .parent()
        .map_or_else(String::new, |p| p.display().to_string())
}

/// Row height matched to the playlist rows' convention.
pub(crate) fn row_height(ui: &egui::Ui) -> f32 {
    ui.text_style_height(&egui::TextStyle::Body) * 1.4
}

/// Row background: duplicates sit quieter than their neighbours, hover
/// lifts whichever row is under the pointer.
fn row_background(ui: &egui::Ui, duplicate: bool, hovered: bool) -> egui::Color32 {
    if ui.visuals().dark_mode {
        match (duplicate, hovered) {
            (true, _) => egui::Color32::from_gray(22),
            (false, true) => egui::Color32::from_gray(45),
            (false, false) => egui::Color32::from_gray(30),
        }
    } else {
        match (duplicate, hovered) {
            (true, _) => egui::Color32::from_gray(252),
            (false, true) => egui::Color32::from_gray(232),
            (false, false) => egui::Color32::from_gray(248),
        }
    }
}

/// Converts one entry-row response into its preview intent: a middle
/// click (that didn't become a drag) previews; other gestures pass.
#[must_use]
fn preview_action_for_row(
    hash: &TrackHash,
    middle_clicked: bool,
    middle_dragged: bool,
) -> Option<LibraryAction> {
    (middle_clicked && !middle_dragged)
        .then_some(LibraryAction::PreviewTrack { hash: hash.clone() })
}

/// One clickable cell: background + vertically centered galley with
/// per-char match highlights.
pub(crate) fn text_cell(
    ui: &mut egui::Ui,
    text: String,
    highlight_indices: HashSet<usize>,
    tone: egui::Color32,
    style: &RowStyle,
    duplicate: bool,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), style.height),
        egui::Sense::click(),
    );
    let painter = ui.painter_at(rect);
    painter.rect_filled(
        rect.shrink(1.0),
        0.0,
        row_background(ui, duplicate, response.hovered()),
    );
    // Test-only: first-row title cell geometry for interaction tests.
    #[cfg(test)]
    if !ui.is_sizing_pass() && text == "lib song" && highlight_indices.is_empty() {
        crate::playlist::view::panel_capture::record("library_cell_title", rect);
    }
    let max_text_width = rect.width() - 2.0 * CELL_TEXT_MARGIN;
    let galley = truncated_galley(
        ui.ctx(),
        &text,
        &style.font,
        tone,
        style.highlight,
        highlight_indices,
        max_text_width,
    );
    painter.galley(
        egui::pos2(
            rect.left() + CELL_TEXT_MARGIN,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        tone,
    );
    response
}

/// Status-glyph cell (playlist/library shared visual language): a
/// spinner while analyzing, otherwise the state glyph.
pub(crate) fn glyph_cell(
    ui: &mut egui::Ui,
    analyzing: bool,
    glyph: &'static str,
    color: egui::Color32,
    style: &RowStyle,
) -> egui::Response {
    let (rect, response) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), style.height),
        egui::Sense::hover(),
    );
    let painter = ui.painter_at(rect);
    let center_y = rect.center().y;
    if analyzing {
        let icon_size = style.height.min(16.0);
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(rect.center().x, center_y),
            egui::vec2(icon_size, icon_size),
        );
        egui::Spinner::new().color(color).paint_at(ui, icon_rect);
    } else {
        let g = truncated_galley(
            ui.ctx(),
            glyph,
            &style.font,
            color,
            style.highlight,
            HashSet::new(),
            rect.width(),
        );
        painter.galley(
            egui::pos2(rect.left() + CELL_TEXT_MARGIN, center_y - g.size().y / 2.0),
            g,
            color,
        );
    }
    response
}

/// [`text_cell`] painting for a caller-owned rectangle. Pure painting:
/// no allocated widget, so it never competes for clicks with the
/// interaction surface the calling row provides.
pub(crate) fn painted_text_cell(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    text: String,
    tone: egui::Color32,
    style: &RowStyle,
) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect.shrink(1.0), 0.0, row_background(ui, false, false));
    let max_text_width = rect.width() - 2.0 * CELL_TEXT_MARGIN;
    let galley = truncated_galley(
        ui.ctx(),
        &text,
        &style.font,
        tone,
        style.highlight,
        HashSet::new(),
        max_text_width,
    );
    painter.galley(
        egui::pos2(
            rect.left() + CELL_TEXT_MARGIN,
            rect.center().y - galley.size().y / 2.0,
        ),
        galley,
        tone,
    );
}

/// Left inset of painted text inside a cell, in points.
pub(crate) const CELL_TEXT_MARGIN: f32 = 6.0;

/// Builds a galley with `highlight` colored chars at `indices` (char
/// positions into `text`), truncated to `max_width` points.
///
/// Truncation cuts at char granularity — never a byte offset — so the
/// highlight indices (which address chars) stay valid; a dropped suffix
/// simply drops its indices. The ellipsis is appended un-highlighted.
fn truncated_galley(
    ctx: &egui::Context,
    text: &str,
    font_id: &egui::FontId,
    base_color: egui::Color32,
    highlight: egui::Color32,
    indices: HashSet<usize>,
    max_width: f32,
) -> std::sync::Arc<egui::Galley> {
    if let Some(kept) = truncate_char_count(ctx, text, font_id, max_width) {
        let layout = highlighted_layout(
            text.chars().take(kept).collect::<String>().as_str(),
            font_id,
            base_color,
            highlight,
            &indices,
        );
        let mut layout = layout;
        layout.append(
            ELLIPSIS,
            0.0,
            egui::text::TextFormat {
                font_id: font_id.clone(),
                color: base_color,
                ..Default::default()
            },
        );
        return ctx.fonts(|f| f.layout_job(layout));
    }
    ctx.fonts(|f| {
        f.layout_job(highlighted_layout(
            text, font_id, base_color, highlight, &indices,
        ))
    })
}

const ELLIPSIS: &str = "\u{2026}";

/// Width of the ellipsis under `font_id`.
fn ellipsis_width(ctx: &egui::Context, font_id: &egui::FontId) -> f32 {
    let job = plain_layout(ELLIPSIS, font_id, egui::Color32::BLACK);
    ctx.fonts(|f| {
        // A one-clone allocation per frame per overlong cell; rows are
        // virtualized so this is bounded by the visible row count.
        let galley = f.layout_job(job);
        galley.size().x
    })
}

fn plain_layout(text: &str, font_id: &egui::FontId, color: egui::Color32) -> egui::text::LayoutJob {
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
    layout
}

/// Measures `text` laid out on one line under `font_id`.
pub(crate) fn single_line_width(ctx: &egui::Context, text: &str, font_id: &egui::FontId) -> f32 {
    let job = plain_layout(text, font_id, egui::Color32::BLACK);
    ctx.fonts(|f| f.layout_job(job).size().x)
}

/// The number of leading chars of `text` that fit in `max_width` with
/// an ellipsis appended, or [`None`] when the whole text fits.
fn truncate_char_count(
    ctx: &egui::Context,
    text: &str,
    font_id: &egui::FontId,
    max_width: f32,
) -> Option<usize> {
    let total = single_line_width(ctx, text, font_id);
    if total <= max_width || max_width <= ellipsis_width(ctx, font_id) {
        return None;
    }
    // Binary search over char count for the longest prefix whose width
    // plus the ellipsis stays within budget.
    let budget = max_width - ellipsis_width(ctx, font_id);
    let mut low = 0_usize;
    let mut high = text.chars().count();
    while low < high {
        let mid = (low + high).div_ceil(2);
        let prefix: String = text.chars().take(mid).collect();
        if single_line_width(ctx, &prefix, font_id) <= budget {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    Some(low)
}

/// Smallest exact-width for a metadata column: its header label plus an
/// arrow and breathing room can never wrap or squeeze siblings.
pub(crate) fn header_column_width(
    ctx: &egui::Context,
    label: &str,
    header_font: &egui::FontId,
) -> f32 {
    const ARROW_BUDGET: f32 = 20.0;
    const COLUMN_PADDING: f32 = 12.0;
    single_line_width(ctx, label, header_font) + ARROW_BUDGET + COLUMN_PADDING
}

fn highlighted_layout(
    text: &str,
    font_id: &egui::FontId,
    base_color: egui::Color32,
    highlight: egui::Color32,
    indices: &HashSet<usize>,
) -> egui::text::LayoutJob {
    let mut layout = egui::text::LayoutJob::default();
    for (char_index, ch) in text.chars().enumerate() {
        let color = if indices.contains(&char_index) {
            highlight
        } else {
            base_color
        };
        layout.append(
            &ch.to_string(),
            0.0,
            egui::text::TextFormat {
                font_id: font_id.clone(),
                color,
                ..Default::default()
            },
        );
    }
    layout
}

/// Whether a root exists at `id` (context-menu enablement helper).
#[must_use]
pub fn has_root(roots: &[LibraryRoot], id: i64) -> bool {
    roots.iter().any(|root| root.id == id)
}

/// The status-glyph vocabulary shared with playlist rows: blank (ready),
/// 🕓 pending, ⭕ while analyzing, red ! failed.
#[must_use]
fn analysis_glyph(
    state: AnalysisState,
    strong: egui::Color32,
    weak: egui::Color32,
) -> (&'static str, egui::Color32) {
    match state {
        AnalysisState::Ready(_) => (" ", strong),
        AnalysisState::Queued => ("🕓", weak),
        AnalysisState::Analyzing => ("⭕", weak),
        AnalysisState::Failed(_) => ("!", egui::Color32::RED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A context whose font atlas is initialized (egui builds fonts on
    /// the first completed pass).
    fn warm_context() -> egui::Context {
        let ctx = egui::Context::default();
        let _ = ctx.run(egui::RawInput::default(), |_| {});
        ctx
    }

    // Given an overlong text and a small width budget.
    // When deciding how many chars fit with an ellipsis appended.
    // Then some chars fit, and text where it fits whole yields None.
    #[test]
    fn truncate_char_count_halts_within_budget() {
        let ctx = warm_context();
        let font = egui::FontId::proportional(14.0);
        let long_text = "x".repeat(200);
        let budget = single_line_width(&ctx, "WWWW", &font);

        let kept = truncate_char_count(&ctx, &long_text, &font, budget)
            .expect("overlong text must truncate");

        assert!(kept > 0);
        assert!(kept < 200);
        // And the prefix plus ellipsis fits when laid out for real.
        let prefix: String = long_text.chars().take(kept).collect();
        let laid_out = single_line_width(&ctx, &format!("{prefix}{ELLIPSIS}"), &font);
        assert!(laid_out <= budget + f32::EPSILON);
    }

    // Given an overlong text whose fuzzy-match highlight sits past the
    // truncation point.
    // When laying out the truncated galley.
    // Then it lays out exactly one row (never wraps) and every kept
    // char range is addressable — indices beyond the cut are dropped.
    #[test]
    fn truncated_galley_stays_single_line_with_dropped_highlights() {
        let ctx = warm_context();
        let font = egui::FontId::proportional(14.0);
        // 40 chars, highlighted chars at 30..35 — all beyond any small
        // prefix that survives truncation.
        let text = "a".repeat(30) + &"h".repeat(5) + &"b".repeat(5);
        let mut indices = HashSet::new();
        (30..35).for_each(|i| {
            indices.insert(i);
        });
        let max_w = single_line_width(&ctx, "WWWWWWWW", &font);

        let galley = truncated_galley(
            &ctx,
            &text,
            &font,
            egui::Color32::WHITE,
            egui::Color32::YELLOW,
            indices,
            max_w,
        );

        assert!(
            galley.rows.len() <= 1,
            "cell galleys are single-line; got {} rows",
            galley.rows.len()
        );
        assert!(galley.size().x <= max_w + f32::EPSILON);
    }

    // Given a short text that fits its budget.
    // When deciding whether to truncate.
    // Then no truncation happens.
    #[test]
    fn truncate_char_count_passes_short_text_through() {
        let ctx = warm_context();
        let font = egui::FontId::proportional(14.0);

        let kept = truncate_char_count(&ctx, "short", &font, 10_000.0);

        assert_eq!(kept, None);
    }

    // Given each metadata column label under the body font.
    // When computing its exact column width.
    // Then the label fits with room to spare for the sort arrow.
    #[rstest::rstest]
    #[case(SortColumn::Bpm)]
    #[case(SortColumn::Key)]
    #[case(SortColumn::Duration)]
    fn header_column_width_fits_label_plus_arrow(#[case] column: SortColumn) {
        let ctx = warm_context();
        let font = egui::FontId::proportional(14.0);

        let width = header_column_width(&ctx, column.label(), &font);

        assert!(
            width > single_line_width(&ctx, column.label(), &font),
            "column must hold its label plus arrow room"
        );
    }

    // Given an entry row's response.
    // When classifying the gesture as a double click.
    // Then an AddTrack intent for its hash is emitted.
    #[test]
    fn double_click_emits_add_track_intent() {
        let hash = TrackHash("h1".to_owned());

        let action = add_action_for_row(&hash, true);

        assert_eq!(action, Some(LibraryAction::AddTrack { hash: hash.clone() }));
    }

    // Given an entry row's response.
    // When the gesture was a single click or hover.
    // Then no intent is emitted.
    #[test]
    fn single_click_emits_no_intent() {
        assert_eq!(add_action_for_row(&TrackHash("h1".to_owned()), false), None);
    }

    // Given an entry row's response.
    // When the gesture was a middle click.
    // Then a PreviewTrack intent for its hash is emitted.
    #[test]
    fn middle_click_emits_preview_track() {
        let hash = TrackHash("h2".to_owned());

        let action = preview_action_for_row(&hash, true, false);

        assert_eq!(action, Some(LibraryAction::PreviewTrack { hash }));
    }

    // Given an entry row's response.
    // When the gesture was not a middle click.
    // Then no preview intent is emitted.
    #[test]
    fn non_middle_click_emits_no_preview_track() {
        let hash = TrackHash("h2".to_owned());

        let action = preview_action_for_row(&hash, false, false);

        assert_eq!(action, None);
    }

    // Given a double-click that must keep adding (regression guard).
    // When both gesture classifiers run on the same responses.
    // Then the double click yields AddTrack and yields no PreviewTrack —
    // middle click would yield exactly the reverse, never both add and
    // preview from one classifier.
    #[rstest::rstest]
    #[case(true, false)]
    #[case(false, true)]
    fn double_click_still_adds_and_never_previews(
        #[case] double_clicked: bool,
        #[case] middle_clicked: bool,
    ) {
        let hash = TrackHash("h3".to_owned());

        let add = add_action_for_row(&hash, double_clicked);
        let preview = preview_action_for_row(&hash, middle_clicked, false);

        match (double_clicked, middle_clicked) {
            (true, false) => {
                assert!(matches!(add, Some(LibraryAction::AddTrack { .. })));
                assert!(preview.is_none());
            }
            (false, true) => {
                assert!(add.is_none());
                assert!(matches!(preview, Some(LibraryAction::PreviewTrack { .. })));
            }
            _ => {}
        }
    }

    // Given an entry whose hash is in the selected playlist.
    // When row tone is derived.
    // Then it dims (duplicate) — the derivation the view paints.
    #[test]
    fn duplicate_hash_derives_dimmed_tone() {
        let e = crate::library::sort::tests_support::sample_entry();
        let selected = [TrackHash("h1".to_owned())];
        let duplicate = selected.contains(&e.hash);
        assert!(duplicate, "selected membership dims the row");
        let other = [TrackHash("h9".to_owned())];
        assert!(!other.contains(&e.hash), "non-members do not dim");
    }

    // Given roots.
    // When checking membership.
    // Then only present ids answer true.
    #[test]
    fn has_root_checks_membership() {
        let roots = vec![LibraryRoot {
            id: 7,
            path: std::path::PathBuf::from("/music"),
        }];
        assert!(has_root(&roots, 7));
        assert!(!has_root(&roots, 8));
    }
}
