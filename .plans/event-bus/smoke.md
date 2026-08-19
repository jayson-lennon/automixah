# Manual smoke — event-bus task

Automated environment has no display; the interactive pass requires the user.
What the suite covers in lieu, per smoke item:

| Smoke item | Automated stand-in |
|---|---|
| create playlist | `create_playlist_appends_exactly_once` (single event, one entry) |
| switch playlists | `select_then_load_transitions_contents`, `stale_rows_loaded_is_dropped`, `rows_loaded_replaces_contents_for_selected_playlist` |
| add duplicate | `duplicate_add_returns_skip_not_failure` (skip, not Failed row) |
| add unanalyzed | `add_track_leaves_other_rows_untouched`, `queue_transitions_queued_analyzing_ready` |
| restart requeue | `reenqueued_incomplete_row_reaches_ready`, `rows_from_persisted_marks_incomplete_for_reenqueue` |
| seek while stopped | `paused_seek_updates_position` |
| pan at max zoom | `drag_view_advances_unclamped_beyond_scrub_max` |
| rename | store round-trip (sqlite + memory) + `PlaylistRenamed` apply |

## Outstanding for the user at the controls
1. New playlist appears exactly once, immediately (B1).
2. Adding an unanalyzed track to an open playlist: only the new row spins (B2).
3. Waveform drag 1:1 with cursor at zoom > 50; click-seek while stopped paints instantly (B3).
4. Rename via right-click; insertion line follows pointer halves of slots.
