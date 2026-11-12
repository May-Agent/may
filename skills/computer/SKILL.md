---
name: computer
description: Read before driving the session's virtual desktop with the `computer` command, to open applications, click, type and press keys, and for what to do when the user takes control.
requires-command: computer
---

# Driving the session desktop

The desktop is the session's own; you work it through screenshots, so every action starts from what the screen shows now. `computer --help` is the syntax reference.

1. Run `computer observe` first. It returns the screen and its size; coordinates are in the desktop's points, with the origin at the top left.
2. Open applications and URLs with `computer launch APP [URL ...]`, never the shell's `open`: it starts the application on the session desktop without taking the user's focus and reports the windows it placed there.
3. Take every coordinate from the latest screenshot. A screenshot from before a window moved, a page loaded or a dialog opened is stale; take a new one before clicking by it.
4. Click the field before typing: `computer type TEXT` types at the focused element. Press keys and combinations with `computer key`, for example `computer key Enter` or `computer key cmd+l` to focus a browser's address bar.
5. After each action that changes the screen, take a screenshot and check the result before the next step.
6. When an action is refused because the user has taken control of the desktop, stop acting. Do not retry in a loop; wait for the takeover to end, then observe again, since the screen has changed under you.
