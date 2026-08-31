/* Omaread chrome palette, rendered by Omarchy on a theme change.
 *
 * Install:  cp omaread.css.tpl ~/.config/omarchy/themed/
 * then switch theme once (omarchy-theme-set) to render it.
 *
 * Omarchy writes the result to
 *   ~/.local/state/omarchy/current/theme/omaread.css
 * which is where Omaread reads it. Press F5 in the library to pick up a new
 * theme without restarting.
 *
 * Only the app chrome follows the system theme — library, contents, HUD. The
 * reading surface keeps its own White/Sepia/Grey/Night, because a palette tuned
 * for a terminal is not tuned for 400 pages of prose (CONTEXT.md §11).
 *
 * Omaread reads exactly these four properties and ignores everything else, so
 * it is safe to edit the mappings to taste.
 */

:root {
  --bg: {{ background }};
  --fg: {{ foreground }};
  --subtle: {{ muted }};
  --panel: {{ selection }};
}
