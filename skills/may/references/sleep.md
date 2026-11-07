# Sleep

Checked against `src/sleep.rs`, `src/daemon/sleep.rs`, `templates/alert_sleep.txt`, `src/config.rs` (`SleepConfig`) and `web-ui/src/lib/api/sleep.ts`.

`/sleep` tells every session the user is going to sleep: one state for the whole daemon, shared by all clients and kept across daemon restarts. Run it from the TUI palette or by sending exactly `/sleep` in the web UI.

## What it does

- Marks the user asleep and delivers `/sleep` as a turn to the session it was run in (a busy session takes it as its running turn can). Running it while already asleep only records it.
- Every session's agent is told once, at the start of its next turn, that the user will not answer for hours. It keeps driving its tasks on its own; anything needing the user's explicit permission goes out through `ask_user` and waits there, and the agent carries on with the rest.
- The agent in the session where `/sleep` was run additionally wishes the user good night, says what it is working on and will keep pushing, and sends every question it was about to ask through `ask_user` in one go while the user is still there.
- Clients show a notice when the state changes: the web UI toasts "You are asleep: agents keep working and collect their questions. Send /sleep off to end it."

## Ending it

- `/sleep off` (palette item in the TUI, exact text in the web UI) ends it for every session; each agent is told once that the user woke.
- A user message sent an hour or more after it began also ends it. A message sooner than that leaves the state on.
- Only a sleep that lasted at least an hour is recorded (at the time it began); a shorter one leaves no record.

## Health reminder

The daemon may remind the user to sleep. It fires when all hold: the user is awake; the recorded sleeps cover at least 5 distinct nights (a night runs noon to noon); their times of day stay within 60 minutes of their median; the clock is within 30 minutes after the median time; and no reminder came in the last 12 hours. The reminder reads "It is about when you usually go to sleep. Run /sleep and get some rest." It is shown to the user (TUI/web notice, web push to subscribed browsers) and never enters an agent's context. `[sleep] reminder = false` turns it off; the setting is read on every check, so it applies live.
