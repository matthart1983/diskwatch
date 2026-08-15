# demo-v2.tape — the 2.0 view.
#
# Recorded against real disk traffic (a burst generator writing and syncing in
# the background), so every figure on screen is measured rather than staged.
# The long hidden warm-up is deliberate: the mirrored graph draws a 1 Hz ring
# two samples per column, and a graph that starts empty is the honest way to
# show a tool that has only just been launched — but it makes a poor first
# frame, so the recording starts once the ring has something to say.

Output demo-v2.gif

Set Shell "bash"
Set FontSize 14
Set Width 1240
Set Height 760
Set Theme "Catppuccin Mocha"
Set TypingSpeed 40ms
Set Framerate 18
Set Padding 10

# The warm-up is broken into chunks separated by a keystroke the view does
# not bind. VHS drives the terminal over a local socket, and a single
# uninterrupted 60s sleep lets that connection go idle and drop, which kills
# the recording at the next keypress. `z` is a no-op in this view, so the
# connection stays warm and the app sees nothing.
Hide
Type "./target/release/diskwatch --v2"
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
Show

# [1] The whole screen. Six boxes, zero chrome rows: the mirrored read/write
# graph over a shared time axis, the per-device table, the latency histogram,
# capacity, SMART, and the busiest files.
Sleep 8s

# [2] Sort the file list — the indicator in the border is written from the
# same value the sort uses, so the two can never disagree.
Type "s"
Sleep 3s
Type "s"
Sleep 3s
Type "s"
Sleep 2s

# [3] Filter by path.
Type "/"
Sleep 1s
Type "cache"
Sleep 3s
Escape
Sleep 2s

# [4] Move the selection.
Down@250ms 3
Sleep 2s

# [5] The settings overlay, reachable without leaving the view. The View row
# walks the same list `V` cycles.
Type ","
Sleep 2s
Down@200ms 8
Sleep 3s
Escape
Sleep 1s

# [6] `V` cycles full -> lite -> 2.0, netwatch's convention.
Type "V"
Sleep 5s
Type "V"
Sleep 5s
Type "V"
Sleep 6s

Type "q"
Sleep 500ms
