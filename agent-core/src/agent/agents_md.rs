//! `AGENTS.md` discovery: the global rules file and the session's launch
//! directory.
//!
//! The global `~/.may/AGENTS.md` and the launch directory's root
//! `AGENTS.md` are loaded into the agent's
//! stable system context; every other `AGENTS.md` in the tree is listed
//! by path relative to the launch directory so the agent reads the file
//! governing a subtree before working in it.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// File name scanned for, per the `AGENTS.md` convention.
const AGENTS_FILE_NAME: &str = "AGENTS.md";

/// Byte cap on the loaded root document. A runaway rules file must not
/// crowd out the rest of the context window; the rendered block notes
/// the truncation so the agent reads the file for the remainder.
const ROOT_DOCUMENT_CAP: usize = 32 * 1024;

/// Maximum nested `AGENTS.md` paths listed; further files fold into a
/// count so the block stays small on monorepo-scale trees.
const NESTED_LIST_CAP: usize = 64;

/// Maximum handoff-referenced `AGENTS.md` files loaded into the block.
const HANDOFF_DOC_CAP: usize = 8;

/// An `AGENTS.md` loaded because a compaction handoff linked to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffAgentDoc {
    /// Resolved path shown in the block.
    pub path: String,
    /// File content, capped at [`ROOT_DOCUMENT_CAP`].
    pub content: String,
    /// Whether `content` was truncated.
    pub truncated: bool,
}

/// `AGENTS.md` files found under a working directory.
#[derive(Debug, Clone, Default)]
pub struct AgentsIndex {
    /// Loaded `<workdir>/AGENTS.md` content, capped at
    /// [`ROOT_DOCUMENT_CAP`]; `None` when absent or empty.
    pub root_document: Option<String>,
    /// Whether `root_document` was truncated to fit the cap.
    pub root_truncated: bool,
    /// `AGENTS.md` files found below the workdir, as paths relative to
    /// its root, in walk order.
    pub nested: Vec<String>,
    /// Nested files found beyond [`NESTED_LIST_CAP`].
    pub extra_nested: usize,
}

/// Scan `workdir` for `AGENTS.md` files off the event loop.
pub async fn discover(workdir: &Path) -> AgentsIndex {
    let workdir = workdir.to_path_buf();
    blocking::unblock(move || discover_sync(&workdir)).await
}

/// A rules file loaded whole into the context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesDocument {
    /// File content, capped at [`ROOT_DOCUMENT_CAP`].
    pub content: String,
    /// Whether `content` was truncated.
    pub truncated: bool,
}

/// Load the global rules file (`~/.may/AGENTS.md`) off the event loop;
/// `None` when it is absent, empty or unreadable.
pub async fn load_global(path: &Path) -> Option<RulesDocument> {
    let path = path.to_path_buf();
    blocking::unblock(move || read_document(&path)).await
}

/// `path` capped to [`ROOT_DOCUMENT_CAP`]; `None` when absent or blank. A
/// read error other than absence is logged and treated as absent.
#[allow(
    clippy::disallowed_methods,
    reason = "synchronous read runs inside blocking::unblock"
)]
fn read_document(path: &Path) -> Option<RulesDocument> {
    match std::fs::read_to_string(path) {
        Ok(content) if !content.trim().is_empty() => {
            let (content, truncated) = cap_document(&content);
            Some(RulesDocument { content, truncated })
        }
        Ok(_) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "failed to read AGENTS.md");
            None
        }
    }
}

fn discover_sync(workdir: &Path) -> AgentsIndex {
    let mut index = AgentsIndex::default();
    if let Some(root) = read_document(&workdir.join(AGENTS_FILE_NAME)) {
        index.root_document = Some(root.content);
        index.root_truncated = root.truncated;
    }

    let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let found = std::sync::Mutex::new(Vec::<PathBuf>::new());
    ignore::WalkBuilder::new(workdir)
        // Dot-directories like `.github` still count.
        .hidden(false)
        // Honor ignore rules even outside a git repository.
        .require_git(false)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .threads(threads)
        .filter_entry(|entry| {
            if entry.depth() == 0 {
                return true;
            }
            if entry.file_name() == OsStr::new(".git") {
                return false;
            }
            // Linked worktrees and nested repositories carry their own `.git`
            // (file or directory) and can hold gigabytes of parallel builds.
            if entry.file_type().is_some_and(|kind| kind.is_dir())
                && entry.path().join(".git").exists()
            {
                return false;
            }
            true
        })
        .build_parallel()
        .run(|| {
            let found = &found;
            Box::new(move |entry| {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        tracing::warn!(%error, "skipped a path during AGENTS.md discovery");
                        return ignore::WalkState::Continue;
                    }
                };
                if !entry.file_type().is_some_and(|kind| kind.is_file())
                    || entry.file_name() != OsStr::new(AGENTS_FILE_NAME)
                {
                    return ignore::WalkState::Continue;
                }
                let relative = entry
                    .path()
                    .strip_prefix(workdir)
                    .unwrap_or_else(|_| entry.path());
                // The root file is loaded in full, not listed.
                if relative == Path::new(AGENTS_FILE_NAME) {
                    return ignore::WalkState::Continue;
                }
                found
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(relative.to_path_buf());
                ignore::WalkState::Continue
            })
        });
    let mut nested = found
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    // Deterministic component-wise order matches depth-first filename order
    // so the rendered block stays stable across agent rebuilds.
    nested.sort_by(|left, right| left.components().cmp(right.components()));
    if nested.len() > NESTED_LIST_CAP {
        index.extra_nested = nested.len() - NESTED_LIST_CAP;
        nested.truncate(NESTED_LIST_CAP);
    }
    index.nested = nested
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    index
}

/// `AGENTS.md` link targets found in `handoff_markdown`, in document
/// order and unvalidated: they may be relative, absolute, or `file://`
/// URLs, and may not exist.
pub fn referenced_agents_paths(handoff_markdown: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for event in pulldown_cmark::Parser::new(handoff_markdown) {
        let pulldown_cmark::Event::Start(pulldown_cmark::Tag::Link { dest_url, .. }) = event else {
            continue;
        };
        let target = dest_url.split(['#', '?']).next().unwrap_or_default();
        if Path::new(target).file_name() == Some(OsStr::new(AGENTS_FILE_NAME)) {
            paths.push(target.to_string());
        }
    }
    paths
}

/// Load every `AGENTS.md` linked from `handoff_markdown`: relative links
/// resolve against `base`, absolute ones stand alone; `file://` URLs are
/// unwrapped. Missing, unreadable and empty files are skipped, duplicate
/// targets load once. At most [`HANDOFF_DOC_CAP`] files, each capped at
/// [`ROOT_DOCUMENT_CAP`].
pub async fn load_referenced(handoff_markdown: &str, base: Option<&Path>) -> Vec<HandoffAgentDoc> {
    let base = base.map(Path::to_path_buf);
    let targets = referenced_agents_paths(handoff_markdown);
    blocking::unblock(move || {
        let mut seen = std::collections::HashSet::new();
        let mut docs = Vec::new();
        for target in targets {
            let target = target
                .strip_prefix("file://")
                .unwrap_or(&target)
                .to_string();
            let target_path = PathBuf::from(&target);
            let candidate = if target_path.is_relative() {
                let Some(base) = &base else { continue };
                base.join(&target_path)
            } else {
                target_path
            };
            let Ok(path) = candidate.canonicalize() else {
                continue;
            };
            if !seen.insert(path.clone()) {
                continue;
            }
            #[allow(
                clippy::disallowed_methods,
                reason = "synchronous read runs inside blocking::unblock"
            )]
            let Ok(content) = std::fs::read_to_string(&path) else {
                continue;
            };
            if content.trim().is_empty() {
                continue;
            }
            let (content, truncated) = cap_document(&content);
            docs.push(HandoffAgentDoc {
                path: path.display().to_string(),
                content,
                truncated,
            });
            if docs.len() == HANDOFF_DOC_CAP {
                break;
            }
        }
        docs
    })
    .await
}

/// `content` verbatim when within the cap, else the longest prefix
/// ending on a char boundary.
fn cap_document(content: &str) -> (String, bool) {
    if content.len() <= ROOT_DOCUMENT_CAP {
        return (content.to_string(), false);
    }
    let mut end = ROOT_DOCUMENT_CAP;
    while !content.is_char_boundary(end) {
        end -= 1;
    }
    (content[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, relative: &str, content: &str) {
        let path = dir.join(relative);
        std::fs::create_dir_all(path.parent().expect("file has a parent")).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn discover_loads_root_and_lists_nested() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "AGENTS.md", "root rules");
        write(dir.path(), "crates/foo/AGENTS.md", "foo rules");
        // Dot-directories are scanned.
        write(dir.path(), ".github/AGENTS.md", "ci rules");
        // `.git` internals never surface.
        write(dir.path(), ".git/hooks/AGENTS.md", "ignored");

        let index = discover(dir.path()).await;

        assert_eq!(index.root_document.as_deref(), Some("root rules"));
        assert!(!index.root_truncated);
        assert_eq!(
            index.nested,
            vec![
                ".github/AGENTS.md".to_string(),
                "crates/foo/AGENTS.md".to_string(),
            ]
        );
        assert_eq!(index.extra_nested, 0);
    }

    #[tokio::test]
    async fn discover_respects_ignore_rules_without_a_repo() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), ".gitignore", "vendor/\n");
        write(dir.path(), "vendor/AGENTS.md", "vendored");
        write(dir.path(), "src/AGENTS.md", "kept");

        let index = discover(dir.path()).await;

        assert_eq!(index.nested, vec!["src/AGENTS.md".to_string()]);
    }

    #[tokio::test]
    async fn discover_caps_the_root_document_on_a_char_boundary() {
        let dir = tempfile::tempdir().unwrap();
        // `é` is two bytes: it starts on the cap boundary, so the cap
        // must land one byte short rather than split it.
        let content = format!("{}é{}", "a".repeat(ROOT_DOCUMENT_CAP - 1), "b".repeat(64));
        write(dir.path(), "AGENTS.md", &content);

        let index = discover(dir.path()).await;

        let document = index.root_document.expect("document should load");
        assert_eq!(document.len(), ROOT_DOCUMENT_CAP - 1);
        assert!(document.is_char_boundary(document.len()));
        assert!(index.root_truncated);
    }

    #[tokio::test]
    async fn discover_caps_the_nested_list_and_counts_the_rest() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "AGENTS.md", "root");
        for index_ in 0..NESTED_LIST_CAP + 3 {
            write(dir.path(), &format!("m/{index_:03}/AGENTS.md"), "rules");
        }

        let index = discover(dir.path()).await;

        assert_eq!(index.nested.len(), NESTED_LIST_CAP);
        assert_eq!(index.extra_nested, 3);
    }

    #[tokio::test]
    async fn discover_is_empty_without_agents_files() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "src/main.rs", "fn main() {}");

        let index = discover(dir.path()).await;

        assert!(index.root_document.is_none());
        assert_eq!(index.nested, [] as [String; 0]);
        assert_eq!(index.extra_nested, 0);
    }

    #[test]
    fn referenced_agents_paths_extracts_only_agents_links() {
        let markdown = "See [crate rules](crates/foo/AGENTS.md), \
                        [docs](docs/README.md) and [ci](.github/AGENTS.md#jobs).";
        assert_eq!(
            referenced_agents_paths(markdown),
            vec![
                "crates/foo/AGENTS.md".to_string(),
                ".github/AGENTS.md".to_string(),
            ]
        );
        assert_eq!(referenced_agents_paths("no links here"), [] as [String; 0]);
    }

    #[tokio::test]
    async fn load_referenced_resolves_relative_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "crates/foo/AGENTS.md", "foo rules");
        write(dir.path(), "crates/bar/AGENTS.md", "");
        let markdown = format!(
            "[foo](crates/foo/AGENTS.md) [again](file://{0}/crates/foo/AGENTS.md) \
             [missing](crates/none/AGENTS.md) [empty](crates/bar/AGENTS.md)",
            dir.path().display()
        );

        let docs = load_referenced(&markdown, Some(dir.path())).await;

        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].content, "foo rules");
        assert!(docs[0].path.ends_with("crates/foo/AGENTS.md"));

        // Relative links need a base; without one they are skipped.
        assert_eq!(
            load_referenced("[x](sub/AGENTS.md)", None).await,
            [] as [HandoffAgentDoc; 0]
        );
    }

    #[tokio::test]
    async fn discover_skips_nested_git_checkouts_and_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "AGENTS.md", "root rules");
        write(dir.path(), "crates/foo/AGENTS.md", "foo rules");
        // Nested repository (`.git` directory) and linked worktree (`.git` file).
        write(
            dir.path(),
            ".worktrees/topic/.git",
            "gitdir: ../.git/worktrees/topic",
        );
        write(dir.path(), ".worktrees/topic/AGENTS.md", "worktree rules");
        write(
            dir.path(),
            ".worktrees/topic/sub/AGENTS.md",
            "nested worktree rules",
        );
        std::fs::create_dir_all(dir.path().join("vendor-repo/.git")).unwrap();
        write(dir.path(), "vendor-repo/AGENTS.md", "nested repo rules");

        let index = discover(dir.path()).await;

        assert_eq!(index.nested, vec!["crates/foo/AGENTS.md".to_string()]);
    }

    #[tokio::test]
    async fn parallel_discover_returns_deterministic_sorted_order() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "AGENTS.md", "root");
        for rel in [
            "z/AGENTS.md",
            "a/b/AGENTS.md",
            "a/AGENTS.md",
            "a-b/AGENTS.md",
            "中文/AGENTS.md",
            "日本語/AGENTS.md",
            "한국어/AGENTS.md",
        ] {
            write(dir.path(), rel, rel);
        }

        let index = discover(dir.path()).await;
        assert_eq!(
            index.nested,
            vec![
                "a/AGENTS.md",
                "a/b/AGENTS.md",
                "a-b/AGENTS.md",
                "z/AGENTS.md",
                "中文/AGENTS.md",
                "日本語/AGENTS.md",
                "한국어/AGENTS.md",
            ]
        );
    }
}
