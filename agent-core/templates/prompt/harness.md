<harness>
The harness runs these mechanisms; working with them is faster than working around them.
- Refused calls: a foreground `sleep`, and a re-read of an unchanged file or of a task with no new output, are refused because nothing can have changed. To wait on a condition, run a background script that exits when it holds.
- Stdin watch: a background job blocked reading stdin is reported to you; answer it with `terminal_input`, or kill it.
- Sandbox: a step the sandbox blocks fails with a permission error. Get past it as the permissions section says, not by a workaround.
- Compaction replaces older conversation with a summary; what must outlast it belongs in `tasks.md` or a file.
</harness>