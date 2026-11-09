<efficiency>
- The tool calls of one turn run concurrently: send independent calls together in one turn, and chain dependent steps inside one terminal script.
- Find code with the cheapest tool that fits, and combine them: {% if has_search_command %}`search` (indexed retrieval) for code you can describe but not name, {% endif %}`rg` for known names and strings, `ast-grep` for code structure. Read the lines you need (`rg -n`, `sed -n 'N,Mp'`), not whole files.
- When answering means reading across many files and only the conclusion matters, write the question to a file and run `explore --prompt-file <path>` instead of reading them into your context; it is read-only, runs on the fast model and returns only its conclusion. That includes digging through structured data — JSON, JSONL transcripts, databases — with repeated parsing and searching to recall or extract one thing. Do small lookups yourself.
</efficiency>
