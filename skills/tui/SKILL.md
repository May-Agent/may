---
name: tui
description: "Read before driving interactive terminal, PTY, or full-screen TUI applications (open, observe, read text, send keys, type, click, drag, scroll, resize, and stop them via the `tui` command)."
requires-command: tui
---

# Driving interactive TUI programs

Use `tui` for full-screen TUIs and TTY-gated interactive CLI programs. `tui --help` is the syntax reference.

1. Start a program with `tui open [--cols N] [--rows N] SCRIPT` (default `120x40`). It launches the command in a PTY under the session's sandbox and returns its real OS `pid` along with the initial settled view.
2. Inspect the screen with `tui observe PID` for layout and visual state (dimensions, title, active terminal modes, cursor position, detected panes, highlighted/selected elements with `<gen>:<idx>` references, plus a rasterized image), or `tui text PID` when plain trimmed screen text is enough.
3. Send keystrokes with `tui key PID KEY` or `tui key PID MOD+KEY`. Keys use W3C `KeyboardEvent.code` names (`Enter`, `Tab`, `Escape`, `Backspace`, `Delete`, `Insert`, `Space`, `ArrowUp`/`ArrowDown`/`ArrowLeft`/`ArrowRight`, `Home`, `End`, `PageUp`, `PageDown`, `F1`..`F12`) or a single letter or digit; modifiers are `ctrl` (`control`), `alt` (`option`), `shift`, and `cmd` (`command`, `meta`). Encoding automatically honors DECCKM (`?1`) and Kitty keyboard protocol flags.
4. Enter literal text with `tui type PID TEXT`. When the program has enabled bracketed paste (`?2004`), the text is automatically wrapped in paste markers.
5. Pointer actions require the target program to have enabled terminal mouse tracking (`?1000`/`?1002`/`?1003`); otherwise they are refused with a clear error and you should navigate by keyboard. When mouse tracking is enabled:
   - `tui click PID REF` clicks a listed `<gen>:<idx>` element from the latest `tui observe PID`.
   - `tui click PID TEXT` clicks the center of a unique visible substring on the current screen (refused if absent or ambiguous).
   - `tui click PID COL ROW`, `tui double-click PID COL ROW`, `tui right-click PID COL ROW`, `tui drag PID COL1 ROW1 COL2 ROW2`, and `tui scroll PID COL ROW DX DY` use 0-based cell coordinates from the top left.
6. Element references (`<gen>:<idx>`) expire as soon as the screen changes. If an action is refused as stale, run `tui observe PID` again and use a reference from the new listing.
7. Change terminal dimensions with `tui resize PID COLS ROWS`.
8. Close programs cleanly with their own quit key (such as `tui key PID q` or `tui key PID ctrl+c`), or terminate them via native `kill <pid>` in `terminal` when needed. Do not leave interactive `tui` sessions running after the task is done.
