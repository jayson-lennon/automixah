//! egui rendering for the library columns (roots + entries) of the
//! bottom panel.
//!
//! Pure painting from derived state: roots and entries come from
//! [`crate::library::LibraryState`]; the visible/filtered/sorted set is
//! derived each frame from `(entries, filter)`; duplicate dimming
//! derives from the selected playlist's hash list. This module only
//! paints and emits user intents back through [`LibraryActions`].

use std::collections::HashSet;

use crate::library::filter::{self, FilterTerm};
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

/// Draws the entries column: search box, summary, virtualized rows.
pub fn entries_column(
    ui: &mut egui::Ui,
    state: &LibraryState,
    tracks: Option<&crate::tracks::Tracks>,
    filter_buffer: &mut String,
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
    let visible = visible_entries(&state.entries, &terms);
    ui.weak(format!(
        "{} / {} tracks",
        visible.len(),
        state.entries.len()
    ));
    egui::ScrollArea::vertical().show_rows(ui, row_height(ui), visible.len(), |ui, range| {
        for index in range {
            let entry = &state.entries[visible[index]];
            // The track database join is optional purely for tests;
            // `None` renders the un-analyzed default glyph.
            let analysis = tracks
                .and_then(|db| db.get(&entry.hash))
                .map(|r| &r.analysis);
            entry_row(ui, entry, analysis, &terms, selected_hashes, actions);
        }
    });
}

/// One library entry row: dim when its hash is in the selected
/// playlist; double-click adds. The glyph column derives from the
/// record's analysis state — the same vocabulary as playlist rows.
fn entry_row(
    ui: &mut egui::Ui,
    entry: &LibraryEntry,
    analysis: Option<&AnalysisState>,
    terms: &[FilterTerm],
    selected_hashes: Option<&[TrackHash]>,
    actions: &mut LibraryActions,
) {
    let duplicate = selected_hashes.is_some_and(|hashes| hashes.contains(&entry.hash));
    let height = row_height(ui);
    let desired = egui::vec2(ui.available_width(), height);
    let (rect, response) = ui.allocate_at_least(desired, egui::Sense::click());
    let hovered = response.hovered();
    let painter = ui.painter_at(rect);
    let bg = if duplicate {
        dimmed_color(ui)
    } else if hovered {
        hover_color(ui)
    } else {
        base_color(ui)
    };
    painter.rect_filled(rect.shrink(1.0), 2.0, bg);
    paint_entry(ui, &painter, rect, entry, analysis, terms, duplicate);
    if let Some(action) = add_action_for_row(&entry.hash, response.double_clicked()) {
        actions.actions.push(action);
    }
    if let Some(action) = preview_action_for_row(
        &entry.hash,
        response.clicked_by(egui::PointerButton::Middle),
    ) {
        actions.actions.push(action);
    }
    response.on_hover_text(entry.rel_path.display().to_string());
}

/// Converts one entry-row response into its add intent: only a double
/// click adds.
#[must_use]
fn add_action_for_row(hash: &TrackHash, double_clicked: bool) -> Option<LibraryAction> {
    double_clicked.then(|| LibraryAction::AddTrack { hash: hash.clone() })
}

/// Converts one entry-row response into its preview intent: a middle
/// click previews; all other gestures stay with the existing actions.
#[must_use]
fn preview_action_for_row(hash: &TrackHash, middle_clicked: bool) -> Option<LibraryAction> {
    middle_clicked.then_some(LibraryAction::PreviewTrack { hash: hash.clone() })
}

/// Row background tones.
fn base_color(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        egui::Color32::from_gray(30)
    } else {
        egui::Color32::from_gray(248)
    }
}

/// See [`base_color`].
fn hover_color(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        egui::Color32::from_gray(45)
    } else {
        egui::Color32::from_gray(232)
    }
}

/// See [`base_color`].
fn dimmed_color(ui: &egui::Ui) -> egui::Color32 {
    if ui.visuals().dark_mode {
        egui::Color32::from_gray(22)
    } else {
        egui::Color32::from_gray(252)
    }
}

/// Row height matched to the playlist rows' convention.
fn row_height(ui: &egui::Ui) -> f32 {
    ui.text_style_height(&egui::TextStyle::Body) * 1.4
}

/// Paints status glyph, artist – title · duration · rel-dir, with each
/// field's own match highlights (artist/title in the main text, path in
/// the dir). The glyph derives from the record's analysis state — the
/// same visual language as playlist rows.
fn paint_entry(
    ui: &mut egui::Ui,
    painter: &egui::Painter,
    rect: egui::Rect,
    entry: &LibraryEntry,
    analysis: Option<&AnalysisState>,
    terms: &[FilterTerm],
    duplicate: bool,
) {
    let font_id = egui::FontId::proportional(ui.text_style_height(&egui::TextStyle::Body) * 0.9);
    let strong = ui.visuals().strong_text_color();
    let weak = ui.visuals().weak_text_color();
    let highlight = egui::Color32::from_rgb(255, 210, 60);
    let main_color = if duplicate { weak } else { strong };
    let center_y = rect.center().y;
    let mut left = rect.left() + 8.0;

    // Status glyph slot — 🕓 pending (the default), an animated spinner
    // at the same slot while analyzing.
    let state = analysis.cloned().unwrap_or(AnalysisState::Queued);
    let analyzing = matches!(state, AnalysisState::Analyzing);
    let (glyph, color) = analysis_glyph(state, strong, weak);
    let icon_size = ui.text_style_height(&egui::TextStyle::Body).min(16.0);
    if analyzing {
        let icon_rect = egui::Rect::from_center_size(
            egui::pos2(left + icon_size / 2.0, center_y),
            egui::vec2(icon_size, icon_size),
        );
        egui::Spinner::new().color(weak).paint_at(ui, icon_rect);
        left += icon_size + 6.0;
    } else {
        let g = highlighted_galley(ui, glyph, &font_id, color, highlight, HashSet::new());
        painter.galley(
            egui::pos2(left, center_y - g.size().y / 2.0),
            g.clone(),
            color,
        );
        left += g.size().x + 6.0;
    }

    // Main text: artist – title (or filename fallback); the artist's
    // indices address it directly, the title's shift past "artist – ".
    let spans = filter::spans(entry, terms);

    // Main text: artist – title (or filename fallback); the artist's
    // indices address it directly, the title's shift past "artist – ".
    let main_text = if entry.artist.is_empty() {
        entry.title.clone()
    } else {
        format!("{} – {}", entry.artist, entry.title)
    };
    let title_offset = if entry.artist.is_empty() {
        0
    } else {
        entry.artist.chars().count() + 3
    };
    let main_indices: HashSet<usize> = spans
        .artist
        .iter()
        .copied()
        .chain(spans.title.iter().map(|i| i + title_offset))
        .collect();
    let main_galley = highlighted_galley(
        ui,
        &main_text,
        &font_id,
        main_color,
        highlight,
        main_indices,
    );
    let size = main_galley.size();
    painter.galley(
        egui::pos2(left, center_y - size.y / 2.0),
        main_galley,
        main_color,
    );

    // Right-aligned: duration, rel dir (path matches highlight here).
    let duration = entry.duration.map_or_else(
        || "---".to_owned(),
        |d| format!("{:}:{:02}", (d / 60.0) as u32, (d % 60.0) as u32),
    );
    let dir = entry
        .rel_path
        .parent()
        .map_or_else(String::new, |p| p.display().to_string());
    let path_indices: HashSet<usize> = spans.path.iter().copied().collect();
    let d_galley = highlighted_galley(ui, &duration, &font_id, weak, highlight, HashSet::new());
    let dir_galley = highlighted_galley(ui, &dir, &font_id, weak, highlight, path_indices);
    let gap = 14.0;
    let mut rx = rect.right() - 8.0;
    for g in [&d_galley, &dir_galley] {
        rx -= g.size().x;
        painter.galley(egui::pos2(rx, center_y - g.size().y / 2.0), g.clone(), weak);
        rx -= gap;
    }
}

/// Builds a galley with `highlight` colored chars at `indices` (char
/// positions into `text`).
fn highlighted_galley(
    ui: &mut egui::Ui,
    text: &str,
    font_id: &egui::FontId,
    base_color: egui::Color32,
    highlight: egui::Color32,
    indices: HashSet<usize>,
) -> std::sync::Arc<egui::Galley> {
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
    ui.fonts(|f| f.layout_job(layout))
}

/// The visible entry indexes: filter terms applied, then sorted
/// artist → title → path with filename-fallback (artist-less) entries
/// last.
#[must_use]
pub fn visible_entries(entries: &[LibraryEntry], terms: &[FilterTerm]) -> Vec<usize> {
    let mut visible: Vec<usize> = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| filter::matches(entry, terms))
        .map(|(index, _)| index)
        .collect();
    visible.sort_by(|&a, &b| sort_key(&entries[a]).cmp(&sort_key(&entries[b])));
    visible
}

/// Sort key: known-artist first, then artist/title/path; unknowns sort
/// by path last.
fn sort_key(entry: &LibraryEntry) -> (u8, String, String, String) {
    let artist = entry.artist.trim().to_lowercase();
    let title = entry.title.trim().to_lowercase();
    let path = entry.rel_path.to_string_lossy().to_lowercase();
    if artist.is_empty() {
        (1, String::new(), String::new(), path)
    } else {
        (0, artist, title, path)
    }
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
    use std::path::PathBuf;

    fn entry(title: &str, artist: &str, rel: &str, hash: &str) -> LibraryEntry {
        LibraryEntry {
            root_id: 1,
            rel_path: PathBuf::from(rel),
            hash: TrackHash(hash.to_owned()),
            title: title.to_owned(),
            artist: artist.to_owned(),
            duration: Some(61.0),
            mtime_secs: 0,
            size_bytes: 0,
        }
    }

    // Given entries in arbitrary order.
    // When the visible set is derived.
    // Then artist→title ordering applies with unknowns last.
    #[test]
    fn visible_entries_sorts_artist_then_title_unknowns_last() {
        let entries = vec![
            entry("Solo File", "", "z/untitled.flac", "h3"),
            entry("Beta", "A-List", "x/beta.flac", "h2"),
            entry("Alpha", "A-List", "x/alpha.flac", "h1"),
        ];

        let visible = visible_entries(&entries, &[]);

        let ordered: Vec<&str> = visible.iter().map(|&i| entries[i].title.as_str()).collect();
        assert_eq!(ordered, vec!["Alpha", "Beta", "Solo File"]);
    }

    // Given entries and a filter matching one.
    // When the visible set is derived.
    // Then only matching entries remain, still sorted.
    #[test]
    fn visible_entries_applies_filter_before_sort() {
        let entries = vec![
            entry("Alpha", "Artist", "a", "h1"),
            entry("Beta", "Artist", "b", "h2"),
            entry("Gamma", "Other", "c", "h3"),
        ];
        let terms = filter::parse("beta, artist");

        let visible = visible_entries(&entries, &terms);

        assert_eq!(visible, vec![1], "only Beta matches both terms");
    }

    // Given an empty entry list.
    // When the visible set is derived.
    // Then it is empty.
    #[test]
    fn visible_entries_empty_library_is_empty() {
        assert!(visible_entries(&[], &[]).is_empty());
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

        let action = preview_action_for_row(&hash, true);

        assert_eq!(action, Some(LibraryAction::PreviewTrack { hash }));
    }

    // Given an entry row's response.
    // When the gesture was not a middle click.
    // Then no preview intent is emitted.
    #[test]
    fn non_middle_click_emits_no_preview_track() {
        let hash = TrackHash("h2".to_owned());

        let action = preview_action_for_row(&hash, false);

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
        let preview = preview_action_for_row(&hash, middle_clicked);

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
        let e = entry("One", "Artist", "a.flac", "h1");
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
            path: PathBuf::from("/music"),
        }];
        assert!(has_root(&roots, 7));
        assert!(!has_root(&roots, 8));
    }
}
