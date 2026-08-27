//! Horizontal geometry for the bottom panel: two halves split by a
//! draggable divider, each half further split into its own columns.
//!
//! One pure function per concern turns the available width and the
//! divider position into rectangles; nothing here reads egui state, so
//! every decision is unit-testable and the renderer just imposes the
//! results. No child can influence another's rect — a half's content
//! is laid out inside the rectangle it was given, full stop.

/// The bottom panel's horizontal geometry in paint order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SectionRects {
    /// Library half (roots + entries columns).
    pub library: egui::Rect,
    /// Draggable divider between the halves.
    pub divider: egui::Rect,
    /// Playlist half (tracks + playlists columns).
    pub playlist: egui::Rect,
}

/// Widths of the library half's two columns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LibraryWidths {
    /// Folder roots column (fixed).
    pub roots: f32,
    /// Entries search + table column (everything left).
    pub entries: f32,
}

impl LibraryWidths {
    /// Fixed roots width. Chosen from the established production
    /// layout (~204 auto at 1280×720) so existing users see no jump.
    pub const ROOTS: f32 = 220.0;
    /// Roots never shrinks below this on narrow windows.
    pub const ROOTS_FLOOR: f32 = 140.0;

    /// Split one `side`-wide rect into roots + entries. Roots keep
    /// their preferred width while the pair holds both floors;
    /// below that everything scales proportionally instead of going
    /// negative.
    #[must_use]
    pub fn from_side(side: f32) -> Self {
        if !side.is_finite() || side < Self::ROOTS + Self::ROOTS_FLOOR {
            let total = Self::ROOTS + Self::ROOTS_FLOOR;
            return Self {
                roots: side / total * Self::ROOTS,
                entries: side / total * Self::ROOTS_FLOOR,
            };
        }
        Self {
            roots: Self::ROOTS,
            entries: side - Self::ROOTS,
        }
    }

    #[must_use]
    pub fn total(self) -> f32 {
        self.roots + self.entries
    }
}

/// Widths of the playlist half's two columns.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaylistWidths {
    /// Selected playlist's track rows column (the wide one).
    pub tracks: f32,
    /// Playlist list column (fixed-ish).
    pub playlists: f32,
}

impl PlaylistWidths {
    /// Preferred playlists-column width.
    pub const PLAYLISTS: f32 = 170.0;
    /// Playlists never shrinks below this on narrow windows.
    pub const PLAYLISTS_FLOOR: f32 = 90.0;

    /// Split one `side`-wide rect into tracks + playlists. Playlists
    /// keeps its preferred width while tracks holds its floor; below
    /// that everything scales proportionally.
    #[must_use]
    pub fn from_side(side: f32) -> Self {
        const TRACKS_FLOOR: f32 = 160.0;
        if !side.is_finite() || side < TRACKS_FLOOR + Self::PLAYLISTS_FLOOR {
            let total = TRACKS_FLOOR + Self::PLAYLISTS_FLOOR;
            return Self {
                tracks: side / total * TRACKS_FLOOR,
                playlists: side / total * Self::PLAYLISTS_FLOOR,
            };
        }
        // Playlists prefer their fixed width; a cramped half clamps them
        // down toward the floor while tracks keeps its floor.
        let playlists = Self::PLAYLISTS
            .min(side - TRACKS_FLOOR)
            .max(Self::PLAYLISTS_FLOOR);
        Self {
            tracks: side - playlists,
            playlists,
        }
    }

    #[must_use]
    pub fn total(self) -> f32 {
        self.tracks + self.playlists
    }
}

/// Divider hit-zone width and visual grip thickness.
pub const DIVIDER_WIDTH: f32 = 12.0;

/// Narrowest usable library half: roots preferred width plus enough
/// entries room to show the table.
pub const LIBRARY_MIN: f32 = LibraryWidths::ROOTS + 240.0;
/// Narrowest usable playlist half.
pub const PLAYLIST_MIN: f32 = 280.0;

/// Default fraction of the row given to the library half.
pub const DEFAULT_LIBRARY_FRACTION: f32 = 0.5;

/// Clamp a user-chosen library fraction against a viewport of
/// `viewport` px so both halves keep at least their minimums.
#[must_use]
pub fn clamp_fraction(fraction: f32, viewport: f32) -> f32 {
    if !fraction.is_finite() {
        return DEFAULT_LIBRARY_FRACTION;
    }
    if !viewport.is_finite() || viewport <= 0.0 {
        return DEFAULT_LIBRARY_FRACTION;
    }
    let min_fraction = (LIBRARY_MIN / viewport).min(1.0);
    let max_fraction = 1.0 - (PLAYLIST_MIN / viewport).min(1.0);
    // A window too small for both minimums: center the divider so
    // neither side collapses first.
    if min_fraction > max_fraction {
        return (min_fraction + max_fraction) * 0.5;
    }
    fraction.clamp(min_fraction, max_fraction)
}

/// Split a `row`-wide strip into the two halves plus the divider
/// between them, honoring `fraction` after clamping.
#[must_use]
pub fn split_row(row: egui::Rect, fraction: f32) -> SectionRects {
    // A degenerate row cannot afford the full divider; scale it rather
    // than overrun the row's right edge.
    let divider_width = DIVIDER_WIDTH.min(row.width() * 0.2);
    let fraction = clamp_fraction(fraction, row.width());
    let library_width = ((row.width() - divider_width) * fraction).round();
    let library = egui::Rect::from_min_size(row.min, egui::vec2(library_width, row.height()));
    let x = library.right();
    let divider = egui::Rect::from_min_size(
        egui::Pos2::new(x, row.top()),
        egui::vec2(divider_width, row.height()),
    );
    let playlist_x = divider.right();
    let playlist_width = (row.right() - playlist_x).max(0.0);
    let playlist = egui::Rect::from_min_size(
        egui::Pos2::new(playlist_x, row.top()),
        egui::vec2(playlist_width, row.height()),
    );
    SectionRects {
        library,
        divider,
        playlist,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row_at(width: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 300.0))
    }

    // Given the default fraction at a production viewport.
    // When the row splits.
    // Then the halves are equal with the divider exactly between them.
    #[test]
    fn defaults_to_even_halves_with_divider_between() {
        let rects = split_row(row_at(1280.0), DEFAULT_LIBRARY_FRACTION);

        assert_eq!(rects.library.width(), 634.0, "half minus half the divider");
        assert_eq!(rects.divider.left(), rects.library.right());
        assert_eq!(rects.playlist.left(), rects.divider.right());
        assert_eq!(rects.playlist.right(), 1280.0);
        assert!(
            (rects.library.width() - rects.playlist.width()).abs() <= DIVIDER_WIDTH,
            "halves are equal up to the divider"
        );
    }

    // Given clamping bounds for common viewports.
    // When fractions outside the bounds arrive.
    // Then they clamp so both halves hold their minimums.
    #[rstest::rstest]
    #[case(1280.0)]
    #[case(1920.0)]
    fn handles_clamp_to_minimums_not_crossing(#[case] viewport: f32) {
        assert_close(
            clamp_fraction(0.0, viewport),
            LIBRARY_MIN / viewport,
            "left floor",
        );
        assert_close(
            clamp_fraction(1.0, viewport),
            1.0 - PLAYLIST_MIN / viewport,
            "right floor",
        );
    }

    fn assert_close(a: f32, b: f32, what: &str) {
        assert!((a - b).abs() < 1e-3, "{what}: {a} vs {b} (delta {})", a - b);
    }

    // Given a degenerate viewport too small for both minimums.
    // When splitting or clamping.
    // Then the divider centers and every rect stays non-negative and
    // ordered library < divider < playlist.
    #[test]
    fn tiny_viewport_centers_divider_and_keeps_order() {
        let rects = split_row(row_at(200.0), 0.9);

        assert!(rects.library.width() > 0.0);
        assert!(rects.playlist.width() >= 0.0);
        assert!(rects.library.right() <= rects.playlist.left());
        assert_close(
            clamp_fraction(0.9, 200.0),
            (clamp_fraction(0.0, 200.0) + clamp_fraction(1.0, 200.0)) * 0.5,
            "centered",
        );
    }

    // Given every viewport from tiny to very wide.
    // When split.
    // Then rects tile the row exactly with no gaps or overlaps.
    #[test]
    fn sweep_tiles_row_exactly() {
        for w in (10..=3000).step_by(7) {
            let viewport = f32::from(u16::try_from(w).expect("in range"));
            let rects = split_row(row_at(viewport), 0.35);

            assert_close(rects.library.left(), 0.0, "library starts at row");
            assert_close(
                rects.divider.left(),
                rects.library.right(),
                "divider abuts library",
            );
            assert_close(
                rects.playlist.left(),
                rects.divider.right(),
                "playlist abuts divider",
            );
            assert_close(rects.playlist.right(), viewport, "playlist ends at row");
        }
    }

    // Given library-half widths across viewports.
    // When split.
    // Then roots hold their constant while there is room, and shrink
    // proportionally only below the aggregate floors.
    #[rstest::rstest]
    #[case(1280.0)]
    #[case(700.0)]
    fn library_split_holds_roots_then_scales(#[case] side: f32) {
        let widths = LibraryWidths::from_side(side);

        assert_close(widths.total(), side, "sum matches side");
        if side >= LibraryWidths::ROOTS + LibraryWidths::ROOTS_FLOOR {
            assert_eq!(widths.roots, LibraryWidths::ROOTS);
        } else {
            assert!(widths.roots < LibraryWidths::ROOTS);
        }
    }

    // Given playlist-half widths across viewports.
    // When split.
    // Then the playlists column keeps its preferred width while tracks
    // stays above its floor, and nothing goes negative when cramped.
    #[rstest::rstest]
    #[case(900.0)]
    #[case(200.0)]
    fn playlist_split_holds_playlists_then_scales(#[case] side: f32) {
        let widths = PlaylistWidths::from_side(side);

        assert_close(widths.total(), side, "sum matches side");
        assert!(widths.tracks >= 0.0 && widths.playlists >= 0.0);
        if side > 480.0 {
            assert_eq!(widths.playlists, PlaylistWidths::PLAYLISTS);
        }
    }
}
