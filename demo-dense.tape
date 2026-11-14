# demo-dense.tape — the Dense view.
#
# Recorded against real disk traffic. scripts/demo-workload.py stages a small
# ~/work tree and keeps real programs busy in it: a server's access log, a VM
# image written and read at drifting rates, curl, tar and zstd, git and rsync.
# So every figure on screen is measured rather than staged, and Hot Files
# watches only that tree, not the recording machine's own home.
#
# The long hidden warm-up is deliberate: the mirrored graph holds two samples
# per column, and a graph that starts empty is the honest way to show a tool
# that has only just been launched — but it makes a poor first frame, so the
# recording starts once the graph is full.
#
# Colours: diskwatch runs on its default `terminal` theme, and `Set Theme`
# below is diskwatch's `dark` palette, as in netwatch's demos. The graph fade
# is built from the colours the terminal reports for that palette.
#
# Font: JetBrainsMono Nerd Font Mono draws braille dots large enough for the
# fill to read as an area. The fallback font draws them as specks.

# vhs writes 25 fps whatever `Set Framerate` says, about 10 MB here. The
# committed GIF is re-encoded to 12 fps on a 96-colour palette, which keeps
# the gradient and lands near 8 MB:
#
#   ffmpeg -i demo-dense.gif -vf "fps=12,split[a][b];[a]palettegen=max_colors=96:stats_mode=full[p];[b][p]paletteuse=dither=none:diff_mode=rectangle" out.gif

Output demo-dense.gif

Set Shell "bash"
Set FontSize 14
Set FontFamily "JetBrainsMono Nerd Font Mono"
Set Width 1250
Set Height 860
Set Theme {"name": "diskwatch dark", "black": "#0c1418", "red": "#ff7878", "green": "#5cd989", "yellow": "#f0c060", "blue": "#5fdcff", "magenta": "#d97aff", "cyan": "#5fdcff", "white": "#c5d1d6", "brightBlack": "#5f747d", "brightRed": "#ff7878", "brightGreen": "#9ae6b4", "brightYellow": "#f0c060", "brightBlue": "#86e6ff", "brightMagenta": "#d97aff", "brightCyan": "#86e6ff", "brightWhite": "#ffffff", "background": "#0c1418", "foreground": "#e6f0f2", "selection": "#1a3340", "cursor": "#c5d1d6"}
Set TypingSpeed 40ms
Set Framerate 18
Set Padding 10

# The warm-up is broken into chunks separated by a keystroke the view does
# not bind. VHS drives the terminal over a local socket, and a single
# uninterrupted 60s sleep lets that connection go idle and drop, which kills
# the recording at the next keypress. `z` is a no-op in this view, so the
# connection stays warm and the app sees nothing.
Hide
Type "python3 scripts/demo-workload.py ~/work --seconds 260 >/dev/null 2>&1 &"
Enter
Sleep 3s
Type "clear && ./target/release/diskwatch --no-config --watch ~/work --dense"
Enter
Sleep 12s
Type "z"
Sleep 12s
Type "z"
Sleep 12s
Type "z"
Sleep 12s
Type "z"
Sleep 12s
Type "z"
Sleep 12s
Type "z"
Sleep 12s
Type "z"
Sleep 12s
Type "z"
Sleep 12s
Type "z"
Sleep 12s
Type "z"
Sleep 12s
Show

# [1] The whole screen. Six boxes, zero chrome rows: the mirrored read/write
# graph over a shared time axis, the per-device table, the latency histogram,
# capacity, SMART, and the busiest files with the process writing each.
Sleep 8s

# [2] Zoom the io box. Every cell is coloured by its height in the graph: dim
# at the baseline, bright at the peak, the way btop draws it.
Type "1"
Sleep 7s
Type "1"
Sleep 2s

# [3] Sort the file list — the indicator in the border is written from the
# same value the sort uses, so the two can never disagree.
Type "s"
Sleep 2500ms
Type "s"
Sleep 1500ms
Type "s"
Sleep 1500ms

# [4] Filter by path.
Type "/"
Sleep 1s
Type "atlas"
Sleep 3s
Escape
Sleep 1500ms

# [5] Move the selection.
Down@250ms 3
Sleep 1500ms

# [6] The settings overlay, down to the graph rows: braille dots, fade on.
Type ","
Sleep 1s
Down@150ms 11
Sleep 3s
Escape
Sleep 1s

# [7] `V` cycles full -> lite -> dense, netwatch's convention.
Type "V"
Sleep 6s
Type "V"
Sleep 6s
Type "V"
Sleep 4s

Hide
Type "q"
Sleep 500ms
# Stop the workload; it removes ~/work on the way out.
Type "kill %1; wait"
Enter
Sleep 5s
