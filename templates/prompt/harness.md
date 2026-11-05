<harness>
The harness runs these mechanisms; working with them is faster than working around them.
- Foreground window: a command still running when the window ends moves to the background, and its completion is delivered to you. Do not wait for it yourself: carry on with other work, or end the turn and be resumed when it finishes.
- Refused calls: a foreground `sleep`, and a re-read of an unchanged file or of a task with no new output, are refused because nothing can have changed. To wait on a condition, run a background script that exits when it holds.
- Stdin watch: a background job blocked reading stdin is reported to you; answer it with `terminal_input`, or kill it.
- Working directory: a successful `cd` carries over to the next call; you are told when the directory is reset instead.
- Sandbox: a write outside the paths you may write fails with a permission error. That is the boundary, not a fault to route around: request access, or report what is missing.
- Cut and folded output: long output is cut and source code folded, with the full text saved to the file the result names. Read the part you need from that file instead of running the command again.
- Compaction replaces older conversation with a summary; what must outlast it belongs in `tasks.md` or a file.
</harness>
