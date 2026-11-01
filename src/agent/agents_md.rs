//! `AGENTS.md` discovery under the session's launch directory.
//!
//! The launch directory's root `AGENTS.md` is loaded into the agent's
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

fn discover_sync(workdir: &Path) -> AgentsIndex {
    let mut index = AgentsIndex::default();
    let root_path = workdir.join(AGENTS_FILE_NAME);
    match std::fs::read_to_string(&root_path) {
        Ok(content) if !content.trim().is_empty() => {
            let (document, truncated) = cap_document(&content);
            index.root_document = Some(document);
            index.root_truncated = truncated;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %root_path.display(), %error, "failed to read AGENTS.md");
        }
    }

    let walker = ignore::WalkBuilder::new(workdir)
        // Dot-directories like `.github` still count.
        .hidden(false)
        // Honor ignore rules even outside a git repository.
        .require_git(false)
        .ignore(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        // Deterministic order keeps the rendered block stable across
        // agent rebuilds.
        .sort_by_file_name(std::cmp::Ord::cmp)
        .filter_entry(|entry| entry.depth() == 0 || entry.file_name() != OsStr::new(".git"))
        .build();
    for entry in walker {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, "skipped a path during AGENTS.md discovery");
                continue;
            }
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file())
            || entry.file_name() != OsStr::new(AGENTS_FILE_NAME)
        {
            continue;
        }
        let relative = entry
            .path()
            .strip_prefix(workdir)
            .unwrap_or_else(|_| entry.path());
        // The root file is loaded in full, not listed.
        if relative == Path::new(AGENTS_FILE_NAME) {
            continue;
        }
        if index.nested.len() < NESTED_LIST_CAP {
            index.nested.push(relative.to_string_lossy().into_owned());
        } else {
            index.extra_nested += 1;
        }
    }
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
}
