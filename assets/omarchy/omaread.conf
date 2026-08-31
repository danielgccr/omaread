# Omaread — suggested Hyprland window rule and keybinding.
#
# Install:  source = ~/Projects/omaread/assets/omarchy/omaread.conf
# at the end of ~/.config/hypr/hyprland.conf (adjust the path).
#
# The rules key on app_id `omaread`, which the binary sets on its window.

# A book is a page, not a tiled column: float it at a page-ish aspect and
# centre it. Tiling a reader next to a terminal gives you a 40-column measure,
# which the two-column fallback then also refuses (CONTEXT.md §3).
windowrule = float, class:^(omaread)$
windowrule = size 900 1000, class:^(omaread)$
windowrule = center, class:^(omaread)$

# Reading is a full-attention activity; no shadow, no rounding, no blur behind
# 400 pages of prose.
windowrule = noshadow, class:^(omaread)$
windowrule = noblur, class:^(omaread)$

# Super + Shift + E opens the library. Change to taste; Omarchy keeps user
# bindings in ~/.config/hypr/bindings.conf if you would rather put it there.
bind = SUPER SHIFT, E, exec, omaread
