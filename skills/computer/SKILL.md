---
name: computer
description: Read before driving the session's virtual desktop with the `computer` command, to open applications, click, type and press keys, and for what to do when the user takes control.
requires-command: computer
---

# Driving the session desktop

The desktop is the session's own. `computer --help` is the syntax reference.

1. Run `computer observe` first. It returns the screen, its size and the actionable elements of its windows, one per line: a reference such as `0a3f91c2:12`, the role, the label or title, the value or placeholder, and the centre point. Coordinates are in the desktop's points, with the origin at the top left.
2. Open applications and URLs with `computer launch APP [URL ...]`, never the shell's `open`: it starts the application on the session desktop without taking the user's focus and reports the windows it placed there.
3. Act on a listed element by its reference: `computer click REF` presses it, and `computer type REF TEXT` focuses it and types TEXT there. Aim by coordinates (`computer click X Y`, `computer type TEXT` at the focused element) only for what the listing does not cover, taking them from the latest screenshot.
4. A reference holds until the next `computer observe`. When an action is refused because its reference is stale, unknown or gone, observe again and use a reference from the new listing; never reuse an old one.
5. Press keys and combinations with `computer key`, for example `computer key Enter` or `computer key cmd+l` to focus a browser's address bar. A key is named by its W3C `KeyboardEvent.code` value, exactly as written there: `Backspace`, `Delete`, `Escape`, `ArrowRight`, `PageDown`; a single letter or digit names its own key.
6. After each action that changes the screen, observe again and check the result before the next step: a field's value in the listing shows what was typed into it.
7. When an action is refused because the user has taken control of the desktop, stop acting. Do not retry in a loop; wait for the takeover to end, then observe again, since the screen has changed under you.
