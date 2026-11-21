//! Resolves `N*` block anchors to concrete line spans.
//!
//! A block op anchors the opening line of a multi-line construct and resolves
//! the whole construct — a function, a class, an `if`, a Markdown section —
//! so the patch language never asks the model to count closing braces. Code
//! files resolve through the file's grammar; Markdown resolves by heading
//! level through nested sections.

use std::path::Path;
use std::str::FromStr as _;

use ast_grep_core::{AstGrep, Node};
use ast_grep_language::SupportLang;

/// Resolves the block beginning on a 1-indexed line.
pub trait BlockResolver {
    /// The 1-indexed inclusive `(start, end)` span of the block beginning on
    /// `line`, or `None` when no multi-line construct begins there.
    fn block_span(&self, path: &Path, text: &str, line: usize) -> Option<(usize, usize)>;
}

/// The resolver `edit` uses: grammar-aware for known languages, Markdown
/// sections for `.md`/`.markdown`, `None` for the rest.
#[derive(Debug, Default, Clone, Copy)]
pub struct AstResolver;

impl BlockResolver for AstResolver {
    fn block_span(&self, path: &Path, text: &str, line: usize) -> Option<(usize, usize)> {
        let ext = path.extension()?.to_str()?;
        if matches!(ext.to_ascii_lowercase().as_str(), "md" | "markdown" | "mdx") {
            return markdown_section(text, line);
        }
        let lang = SupportLang::from_str(ext)
            .ok()
            .or_else(|| lang_by_extension(ext))?;
        let root = AstGrep::str(text, lang);
        code_block(&root, line - 1)
    }
}

/// Maps a file extension to its language when it is not an accepted alias.
fn lang_by_extension(ext: &str) -> Option<SupportLang> {
    const MAP: &[(&[&str], &str)] = &[
        (&["cc", "cpp", "cxx", "hh", "hpp", "hxx"], "cpp"),
        (&["h"], "c"),
        (&["cs"], "csharp"),
        (&["mjs", "cjs", "jsx"], "javascript"),
        (&["mts", "cts"], "typescript"),
        (&["yml"], "yaml"),
        (&["pyi"], "python"),
        (&["kts"], "kotlin"),
        (&["ex", "exs"], "elixir"),
        (&["sh", "bash"], "bash"),
        (&["htm"], "html"),
        (&["sc"], "scala"),
    ];
    MAP.iter()
        .find(|(exts, _)| exts.contains(&ext))
        .and_then(|(_, name)| SupportLang::from_str(name).ok())
}

/// The outermost named node that opens on `line` (0-indexed) and spans more
/// than one line — the construct beginning there — never the file root.
fn code_block<S: ast_grep_core::Doc>(root: &AstGrep<S>, line: usize) -> Option<(usize, usize)> {
    let root_node = root.root();
    let root_range = root_node.range();
    let mut best: Option<Node<S>> = None;
    let mut stack = vec![root_node];
    while let Some(node) = stack.pop() {
        let start = node.start_pos().line();
        let end = node.end_pos().line();
        if node.is_named()
            && start == line
            && end > line
            && node.range() != root_range
            && best.as_ref().is_none_or(|b: &Node<S>| {
                node.range().end - node.range().start > b.range().end - b.range().start
            })
        {
            best = Some(node.clone());
        }
        stack.extend(node.children());
    }
    best.map(|node| (node.start_pos().line() + 1, node.end_pos().line() + 1))
}

/// The Markdown section a heading at `line` opens: through deeper nested
/// headings, up to the next same-or-higher heading or end of file. Fenced
/// code blocks never carry headings.
fn markdown_section(text: &str, line: usize) -> Option<(usize, usize)> {
    let lines: Vec<&str> = text.split('\n').collect();
    let level = {
        let mut in_fence = false;
        let mut level = None;
        for (index, content) in lines.iter().enumerate().take(line) {
            if content.trim_start().starts_with("```") {
                in_fence = !in_fence;
                continue;
            }
            if index + 1 == line && !in_fence {
                level = heading_level(content);
            }
        }
        level
    }?;
    let mut in_fence = false;
    let mut end = lines.len();
    for (index, content) in lines.iter().enumerate().skip(line) {
        if content.trim_start().starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if !in_fence && heading_level(content).is_some_and(|next| next <= level) {
            end = index;
            break;
        }
    }
    Some((line, end.max(line)))
}

/// The `#`-level of a Markdown heading line, `None` when it is not one.
fn heading_level(line: &str) -> Option<usize> {
    let trimmed = line.trim_start();
    let hashes = trimmed.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &trimmed[hashes..];
    if rest.is_empty() || rest.starts_with([' ', '\t']) {
        Some(hashes)
    } else {
        None
    }
}
