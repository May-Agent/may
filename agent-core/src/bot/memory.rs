//! Hierarchical Markdown filesystem memory for May Bots.
//!
//! Covers `memory.git` provenance commits, Rust structural invariant
//! verification, serial FIFO agentic curation, `LiveContent` resident index,
//! and deduplicated `repo-search` hybrid RAG.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use aither_agent::context::LiveContent;
use aither_agent::{Agent, AgentEvent, ExternalContent, RunOutcome};
use aither_core::LanguageModel;
use aither_core::llm::Message;
use aither_sandbox::{Grants, Interpreter, NetworkPolicyConfig};
use askama::Template;
use may_db::ChatDb;
use may_protocol::bot::{BotMemoryCommit, BotMemoryNode, SeenMemory, SeenMemoryLedger};
use may_protocol::session_layout::BotPaths;
use may_repo_search::{CollectionBackend, Document, RepoSearchError};

/// `LiveContent` descriptor for a bot's resident hierarchical `MEMORY.md` index.
pub const BOT_MEMORY_INDEX_CONTENT: LiveContent =
    LiveContent::new("Bot hierarchical MEMORY.md index", "bot_memory_index_diff");

/// Source label used for the bot's resident `ExternalContent` memory index block.
pub const BOT_MEMORY_INDEX_SOURCE: &str = "bot hierarchical memory index (MEMORY.md)";

/// Maximum number of verification feedback rounds sent to the warm curator session.
const MAX_CURATOR_RETRIES: usize = 3;

/// Errors returned by memory path validation, verification, git curation, or RAG.
#[derive(Debug, thiserror::Error)]
pub enum BotMemoryError {
    /// Invalid or forbidden leaf memory path.
    #[error("{0}")]
    InvalidPath(String),
    /// Requested leaf memory file does not exist.
    #[error("memory leaf `{0}` does not exist")]
    NotFound(String),
    /// Unknown or unauthorized `(source_agent, source_time)` citation in `store_fact`.
    #[error("{0}")]
    SourceResolution(String),
    /// Filesystem I/O failure.
    #[error("memory I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Git command failed on `memory.git`.
    #[error("memory.git error: {0}")]
    Git(String),
    /// Curator failed or could not satisfy structural invariants after retries.
    #[error("memory curation failed: {0}")]
    Curation(String),
    /// Database error.
    #[error(transparent)]
    Db(#[from] may_db::ChatDbError),
    /// `repo-search` backend error.
    #[error(transparent)]
    Search(#[from] RepoSearchError),
}

/// Normalizes and validates a leaf path passed to `recall(path)`.
///
/// - Rejects empty paths, absolute paths, `..` traversal, trailing slashes,
///   non-Markdown extensions, and `MEMORY.md` (at root or any subdirectory).
/// - Appends `.md` when no extension is present (e.g. `preference/food` -> `preference/food.md`).
///
/// # Errors
/// Returns [`BotMemoryError::InvalidPath`] when the path is empty, escapes the
/// tree, points to a directory, uses a non-`.md` extension, or names `MEMORY.md`.
///
/// # Panics
/// Never panics; internal non-empty slice checks are unreachable.
pub fn normalize_leaf_path(raw: &str) -> Result<String, BotMemoryError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(BotMemoryError::InvalidPath(
            "recall path must not be empty".to_string(),
        ));
    }
    if trimmed.ends_with('/') || trimmed.ends_with('\\') {
        return Err(BotMemoryError::InvalidPath(format!(
            "recall path `{trimmed}` must name a leaf Markdown file, not a directory"
        )));
    }
    let normalized_slashes = trimmed.replace('\\', "/");
    let path = Path::new(&normalized_slashes);
    if path.is_absolute() {
        return Err(BotMemoryError::InvalidPath(format!(
            "recall path `{trimmed}` must be relative to the bot memory root"
        )));
    }
    let mut parts: Vec<String> = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(seg) => {
                let s = seg.to_string_lossy();
                if s.is_empty() || s.starts_with('.') {
                    return Err(BotMemoryError::InvalidPath(format!(
                        "recall path `{trimmed}` contains a hidden or invalid segment"
                    )));
                }
                parts.push(s.into_owned());
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(BotMemoryError::InvalidPath(format!(
                    "recall path `{trimmed}` must stay inside the bot memory directory"
                )));
            }
        }
    }
    if parts.is_empty() {
        return Err(BotMemoryError::InvalidPath(
            "recall path must name a leaf file".to_string(),
        ));
    }
    let last = parts.last_mut().expect("non-empty parts");
    if last.eq_ignore_ascii_case("MEMORY.md") || last.eq_ignore_ascii_case("MEMORY") {
        return Err(BotMemoryError::InvalidPath(
            "MEMORY.md indexes are already resident in context; pass a concrete leaf file to recall"
                .to_string(),
        ));
    }
    if let Some(ext) = Path::new(last.as_str())
        .extension()
        .and_then(|e| e.to_str())
    {
        if !ext.eq_ignore_ascii_case("md") {
            return Err(BotMemoryError::InvalidPath(format!(
                "recall path `{trimmed}` must be a Markdown (.md) file"
            )));
        }
    } else {
        last.push_str(".md");
    }
    Ok(parts.join("/"))
}

/// Computes the `blake3` hex hash of UTF-8 text.
#[must_use]
pub fn blake3_hex(content: &str) -> String {
    blake3::hash(content.as_bytes()).to_hex().to_string()
}

/// Returns the per-bot `repo-search` collection name (`may-bot-memory-<bot_id>`).
#[must_use]
pub fn bot_memory_collection(bot_id: &str) -> String {
    format!("may-bot-memory-{bot_id}")
}

/// Ensures `~/.may/bots/<bot_id>/memory/` (with an initial `MEMORY.md`) and
/// `~/.may/bots/<bot_id>/memory.git` exist and are initialized.
///
/// # Errors
/// Returns [`BotMemoryError`] if directory creation or `git init` fails.
pub async fn ensure_bot_memory_initialized(paths: &BotPaths) -> Result<(), BotMemoryError> {
    let memory_dir = paths.memory_dir();
    let git_dir = paths.memory_git_dir();
    let staging_dir = paths.memory_staging_dir();
    async_fs::create_dir_all(&memory_dir).await?;
    async_fs::create_dir_all(&staging_dir).await?;
    let root_index = memory_dir.join("MEMORY.md");
    if !root_index.exists() {
        async_fs::write(&root_index, "# Memory Index\n\nNo facts stored yet.\n").await?;
    }
    if !git_dir.join("HEAD").exists() {
        let git_dir_clone = git_dir.clone();
        blocking::unblock(move || init_memory_git_repo(&git_dir_clone)).await?;
    }
    Ok(())
}

#[allow(clippy::disallowed_methods, reason = "runs inside blocking::unblock")]
fn memory_git_base() -> std::process::Command {
    let mut cmd = std::process::Command::new("git");
    // Isolate internal bot `memory.git` operations from user-global `~/.gitconfig`
    // hooks, signing requirements, and aliases.
    cmd.env("GIT_CONFIG_NOSYSTEM", "1").env(
        "GIT_CONFIG_GLOBAL",
        if cfg!(windows) { "NUL" } else { "/dev/null" },
    );
    cmd
}

#[allow(clippy::disallowed_methods, reason = "runs inside blocking::unblock")]
fn init_memory_git_repo(git_dir: &Path) -> Result<(), BotMemoryError> {
    std::fs::create_dir_all(git_dir)?;
    let status = memory_git_base()
        .arg("init")
        .arg("--bare")
        .arg(git_dir)
        .output()?;
    if !status.status.success() {
        return Err(BotMemoryError::Git(format!(
            "git init --bare failed: {}",
            String::from_utf8_lossy(&status.stderr).trim()
        )));
    }
    Ok(())
}

/// One structural invariant violation found in a bot memory tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryInvariantViolation {
    /// Human-readable description of the violation.
    pub message: String,
}

impl std::fmt::Display for MemoryInvariantViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// Verifies every structural invariant of a bot's Markdown memory tree.
///
/// 1. No symlinks anywhere in the tree (prevents escaping symlinks).
/// 2. Only valid UTF-8 `.md` files exist (no non-Markdown or binary files).
/// 3. The root directory and every subdirectory contain a non-empty `MEMORY.md`.
/// 4. Each `MEMORY.md` mentions every direct child file and subdirectory by name,
///    and contains no stale backtick/link/list references to missing children.
///
/// # Errors
/// Returns [`BotMemoryError::Io`] only if the directory tree cannot be walked.
#[allow(
    clippy::disallowed_methods,
    reason = "synchronous fs walk called directly or in unblock"
)]
pub fn verify_memory_tree(root: &Path) -> Result<Vec<MemoryInvariantViolation>, BotMemoryError> {
    let mut violations = Vec::new();
    if !root.exists() || !root.is_dir() {
        violations.push(MemoryInvariantViolation {
            message: format!("memory root `{}` does not exist", root.display()),
        });
        return Ok(violations);
    }
    verify_dir_recursive(root, root, &mut violations)?;
    Ok(violations)
}

#[allow(clippy::disallowed_methods, reason = "helper for verify_memory_tree")]
fn verify_dir_recursive(
    root: &Path,
    dir: &Path,
    violations: &mut Vec<MemoryInvariantViolation>,
) -> Result<(), BotMemoryError> {
    let rel_dir = dir
        .strip_prefix(root)
        .unwrap_or(dir)
        .to_string_lossy()
        .replace('\\', "/");
    let display_dir = if rel_dir.is_empty() {
        ".".to_string()
    } else {
        rel_dir
    };

    let mut direct_files: BTreeSet<String> = BTreeSet::new();
    let mut direct_dirs: BTreeSet<String> = BTreeSet::new();
    let mut has_memory_md = false;

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            violations.push(MemoryInvariantViolation {
                message: format!(
                    "symlink `{name}` in `{display_dir}` is forbidden; only regular UTF-8 .md files and directories are allowed"
                ),
            });
            continue;
        }
        if meta.is_dir() {
            if name == ".git" {
                violations.push(MemoryInvariantViolation {
                    message: format!(
                        "unexpected `.git` directory inside memory tree `{display_dir}`"
                    ),
                });
                continue;
            }
            direct_dirs.insert(name);
            verify_dir_recursive(root, &path, violations)?;
        } else if meta.is_file() {
            let is_md = Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("md"));
            if !is_md {
                violations.push(MemoryInvariantViolation {
                    message: format!(
                        "non-Markdown file `{name}` in `{display_dir}` is forbidden; only UTF-8 .md files are allowed"
                    ),
                });
                continue;
            }
            let bytes = std::fs::read(&path)?;
            if std::str::from_utf8(&bytes).is_err() {
                violations.push(MemoryInvariantViolation {
                    message: format!("file `{name}` in `{display_dir}` is not valid UTF-8"),
                });
                continue;
            }
            if name == "MEMORY.md" {
                has_memory_md = true;
            } else {
                direct_files.insert(name);
            }
        } else {
            violations.push(MemoryInvariantViolation {
                message: format!("unsupported entry `{name}` in `{display_dir}`"),
            });
        }
    }

    if !has_memory_md {
        violations.push(MemoryInvariantViolation {
            message: format!("directory `{display_dir}` is missing a required `MEMORY.md`"),
        });
        return Ok(());
    }

    let index_path = dir.join("MEMORY.md");
    let index_text = std::fs::read_to_string(&index_path)?;
    if index_text.trim().is_empty() {
        violations.push(MemoryInvariantViolation {
            message: format!("`{display_dir}/MEMORY.md` must not be empty"),
        });
        return Ok(());
    }

    // Check every direct child file and directory is named in MEMORY.md.
    for file in &direct_files {
        let stem = file.strip_suffix(".md").unwrap_or(file);
        if !index_mentions_entry(&index_text, file) && !index_mentions_entry(&index_text, stem) {
            violations.push(MemoryInvariantViolation {
                message: format!(
                    "`{display_dir}/MEMORY.md` does not list direct child file `{file}`"
                ),
            });
        }
    }
    for subdir in &direct_dirs {
        if !index_mentions_entry(&index_text, subdir) {
            violations.push(MemoryInvariantViolation {
                message: format!(
                    "`{display_dir}/MEMORY.md` does not list direct subdirectory `{subdir}/`"
                ),
            });
        }
    }

    // Check for stale references in bullet/link entries of MEMORY.md that name
    // direct `.md` files or `subdir/` entries that no longer exist.
    for referenced in extract_referenced_children(&index_text) {
        if referenced.eq_ignore_ascii_case("MEMORY.md") {
            continue;
        }
        if let Some(dir_name) = referenced.strip_suffix('/') {
            if !direct_dirs.contains(dir_name) {
                violations.push(MemoryInvariantViolation {
                    message: format!(
                        "`{display_dir}/MEMORY.md` has stale reference to missing directory `{referenced}`"
                    ),
                });
            }
        } else if Path::new(&referenced)
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
            && !direct_files.contains(&referenced)
        {
            violations.push(MemoryInvariantViolation {
                message: format!(
                    "`{display_dir}/MEMORY.md` has stale reference to missing file `{referenced}`"
                ),
            });
        }
    }

    Ok(())
}

/// Checks whether `index_text` mentions `entry` as an identifier/token.
fn index_mentions_entry(index_text: &str, entry: &str) -> bool {
    index_text.contains(entry)
}

/// Extracts direct child references written in Markdown links `[...](child.md)`,
/// `[...](subdir/)`, or inline code `` `child.md` `` / `` `subdir/` ``.
fn extract_referenced_children(index_text: &str) -> Vec<String> {
    let mut found = Vec::new();
    // 1. Backtick-quoted names like `food.md` or `preference/`
    let mut rest = index_text;
    while let Some(start) = rest.find('`') {
        let after = &rest[start + 1..];
        let Some(end) = after.find('`') else {
            break;
        };
        let token = after[..end].trim();
        if is_direct_child_ref(token) {
            found.push(token.to_string());
        }
        rest = &after[end + 1..];
    }
    // 2. Markdown link targets `](target)`
    let mut link_rest = index_text;
    while let Some(start) = link_rest.find("](") {
        let after = &link_rest[start + 2..];
        let Some(end) = after.find(')') else {
            break;
        };
        let token = after[..end].trim().trim_start_matches("./");
        if is_direct_child_ref(token) {
            found.push(token.to_string());
        }
        link_rest = &after[end + 1..];
    }
    found.sort();
    found.dedup();
    found
}

fn is_direct_child_ref(token: &str) -> bool {
    if token.is_empty() || token.contains("://") || token.contains(' ') || token.starts_with('/') {
        return false;
    }
    token.strip_suffix('/').map_or_else(
        || {
            Path::new(token)
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
                && !token.contains('/')
        },
        |dir| !dir.is_empty() && !dir.contains('/'),
    )
}

/// Renders all `MEMORY.md` files across `memory_root` into a single deterministic
/// hierarchical index suitable for the bot's resident `ExternalSlot`.
///
/// # Errors
/// Returns [`BotMemoryError::Io`] on read failure.
#[allow(
    clippy::disallowed_methods,
    reason = "called inside blocking::unblock or sync helpers"
)]
pub fn render_hierarchical_memory_index(memory_root: &Path) -> Result<String, BotMemoryError> {
    if !memory_root.exists() {
        return Ok("# Memory Index\n\nNo facts stored yet.\n".to_string());
    }
    let mut indexes: BTreeMap<String, String> = BTreeMap::new();
    collect_memory_indexes(memory_root, memory_root, &mut indexes)?;
    if indexes.is_empty() {
        return Ok("# Memory Index\n\nNo facts stored yet.\n".to_string());
    }
    let mut sections = Vec::with_capacity(indexes.len());
    for (rel_path, content) in indexes {
        sections.push(format!("## {rel_path}\n\n{}", content.trim()));
    }
    Ok(sections.join("\n\n"))
}

/// Builds the `ExternalContent` block carrying the rendered hierarchical index.
///
/// # Errors
/// Returns [`BotMemoryError`] if the memory tree cannot be read.
pub async fn load_bot_memory_index_block(
    memory_root: PathBuf,
) -> Result<ExternalContent, BotMemoryError> {
    let rendered =
        blocking::unblock(move || render_hierarchical_memory_index(&memory_root)).await?;
    Ok(ExternalContent::data(BOT_MEMORY_INDEX_SOURCE, rendered))
}

#[allow(
    clippy::disallowed_methods,
    reason = "helper for render_hierarchical_memory_index"
)]
fn collect_memory_indexes(
    root: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, String>,
) -> Result<(), BotMemoryError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            collect_memory_indexes(root, &path, out)?;
        } else if entry.file_name() == "MEMORY.md" {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let content = std::fs::read_to_string(&path)?;
            out.insert(rel, content);
        }
    }
    Ok(())
}

/// Reads all leaf `.md` files (excluding `MEMORY.md`) into a map of
/// normalized relative path -> `(blake3_hex, content)`.
///
/// # Errors
/// Returns [`BotMemoryError::Io`] on filesystem failure.
#[allow(
    clippy::disallowed_methods,
    reason = "runs inside blocking::unblock or tests"
)]
pub fn collect_leaf_files(
    root: &Path,
) -> Result<BTreeMap<String, (String, String)>, BotMemoryError> {
    let mut out = BTreeMap::new();
    if !root.exists() {
        return Ok(out);
    }
    collect_leaves_recursive(root, root, &mut out)?;
    Ok(out)
}

#[allow(clippy::disallowed_methods, reason = "helper for collect_leaf_files")]
fn collect_leaves_recursive(
    root: &Path,
    dir: &Path,
    out: &mut BTreeMap<String, (String, String)>,
) -> Result<(), BotMemoryError> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            collect_leaves_recursive(root, &path, out)?;
        } else if meta.is_file() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "MEMORY.md" {
                continue;
            }
            if Path::new(&name)
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("md"))
            {
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let content = std::fs::read_to_string(&path)?;
                let hash = blake3_hex(&content);
                out.insert(rel, (hash, content));
            }
        }
    }
    Ok(())
}

/// Reads the full memory tree into a UI-friendly [`BotMemoryNode`] hierarchy.
///
/// # Errors
/// Returns [`BotMemoryError::Io`] on filesystem failure.
#[allow(clippy::disallowed_methods, reason = "runs inside blocking::unblock")]
pub fn read_memory_tree(root: &Path) -> Result<BotMemoryNode, BotMemoryError> {
    if !root.exists() {
        return Ok(BotMemoryNode {
            path: String::new(),
            name: "memory".to_string(),
            is_dir: true,
            content: None,
            children: Vec::new(),
        });
    }
    read_node_recursive(root, root)
}

#[allow(clippy::disallowed_methods, reason = "helper for read_memory_tree")]
fn read_node_recursive(root: &Path, current: &Path) -> Result<BotMemoryNode, BotMemoryError> {
    let rel = current
        .strip_prefix(root)
        .unwrap_or(current)
        .to_string_lossy()
        .replace('\\', "/");
    let name = if rel.is_empty() {
        "memory".to_string()
    } else {
        current
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
    };
    let meta = std::fs::symlink_metadata(current)?;
    if meta.is_dir() {
        let mut children = Vec::new();
        for entry in std::fs::read_dir(current)? {
            let entry = entry?;
            let path = entry.path();
            if std::fs::symlink_metadata(&path)?.file_type().is_symlink() {
                continue;
            }
            children.push(read_node_recursive(root, &path)?);
        }
        children.sort_by(|a, b| {
            // Put MEMORY.md first, then directories, then files alphabetically.
            let rank = |n: &BotMemoryNode| -> u8 {
                if !n.is_dir && n.name == "MEMORY.md" {
                    0
                } else if n.is_dir {
                    1
                } else {
                    2
                }
            };
            rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(&b.name))
        });
        Ok(BotMemoryNode {
            path: rel,
            name,
            is_dir: true,
            content: None,
            children,
        })
    } else {
        let content = std::fs::read_to_string(current).ok();
        Ok(BotMemoryNode {
            path: rel,
            name,
            is_dir: false,
            content,
            children: Vec::new(),
        })
    }
}

/// Reads a single leaf file from `memory_root` after validating `raw_path`.
///
/// # Errors
/// Returns [`BotMemoryError::InvalidPath`] or [`BotMemoryError::NotFound`].
pub async fn recall_leaf(
    memory_root: &Path,
    raw_path: &str,
) -> Result<(String, String, String), BotMemoryError> {
    let normalized = normalize_leaf_path(raw_path)?;
    let full = memory_root.join(&normalized);
    let meta = async_fs::symlink_metadata(&full).await.map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            BotMemoryError::NotFound(normalized.clone())
        } else {
            BotMemoryError::Io(error)
        }
    })?;
    if meta.file_type().is_symlink() || meta.is_dir() {
        return Err(BotMemoryError::InvalidPath(format!(
            "recall path `{normalized}` must refer to a regular leaf Markdown file"
        )));
    }
    let content = async_fs::read_to_string(&full).await?;
    let hash = blake3_hex(&content);
    Ok((normalized, content, hash))
}

/// Copies a directory tree cleanly into `dst` (replacing `dst` contents first).
///
/// # Errors
/// Returns [`BotMemoryError::Io`] on filesystem failure.
#[allow(clippy::disallowed_methods, reason = "runs inside blocking::unblock")]
pub fn mirror_dir_sync(src: &Path, dst: &Path) -> Result<(), BotMemoryError> {
    if dst.exists() {
        std::fs::remove_dir_all(dst)?;
    }
    std::fs::create_dir_all(dst)?;
    copy_dir_contents(src, dst)
}

#[allow(clippy::disallowed_methods, reason = "helper for mirror_dir_sync")]
fn copy_dir_contents(src: &Path, dst: &Path) -> Result<(), BotMemoryError> {
    if !src.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            continue;
        }
        let target = dst.join(entry.file_name());
        if meta.is_dir() {
            std::fs::create_dir_all(&target)?;
            copy_dir_contents(&path, &target)?;
        } else if meta.is_file() {
            std::fs::copy(&path, &target)?;
        }
    }
    Ok(())
}

/// Atomically publishes `staging_dir` into `live_dir` by staging into a sibling
/// directory and swapping names so concurrent `recall` readers never see a half-written tree.
///
/// # Errors
/// Returns [`BotMemoryError::Io`] on filesystem failure.
#[allow(clippy::disallowed_methods, reason = "runs inside blocking::unblock")]
pub fn publish_staging_atomically(
    staging_dir: &Path,
    live_dir: &Path,
) -> Result<(), BotMemoryError> {
    let parent = live_dir
        .parent()
        .ok_or_else(|| BotMemoryError::Io(std::io::Error::other("memory dir has no parent")))?;
    let suffix = crate::id::new();
    let incoming = parent.join(format!(".memory.incoming.{suffix}"));
    let backup = parent.join(format!(".memory.prev.{suffix}"));
    mirror_dir_sync(staging_dir, &incoming)?;
    if live_dir.exists() {
        std::fs::rename(live_dir, &backup)?;
    }
    if let Err(err) = std::fs::rename(&incoming, live_dir) {
        if backup.exists() {
            let _ = std::fs::rename(&backup, live_dir);
        }
        return Err(BotMemoryError::Io(err));
    }
    if backup.exists() {
        let _ = std::fs::remove_dir_all(&backup);
    }
    Ok(())
}

#[derive(Template)]
#[template(path = "bot/memory_commit.txt", escape = "none")]
struct MemoryCommitTemplate<'a> {
    subject: &'a str,
    fact: &'a str,
    source_agent: &'a str,
    source_sender: &'a str,
    source_time: &'a str,
    verbatim_quote: &'a str,
}

/// Provenance resolved from SQLite for one `store_fact` call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedFactSource {
    /// Agent id that owns the originating message (bot id, delegated `agent_id`, or supervised `session_id`).
    pub source_agent: String,
    /// Human-readable sender label.
    pub source_sender: String,
    /// Exact UTC timestamp string.
    pub source_time: String,
    /// Verbatim original message text.
    pub verbatim_quote: String,
}

/// Resolves `(source_agent, source_time)` against SQLite message stores.
///
/// Checks `bot_inbox` (when omitted or naming this bot), `bot_delegate_messages`
/// (for a worker owned by `bot_id`), or `messages` (for a session supervised by
/// `bot_id`). Fails fast on unknown timestamps or unauthorized targets.
///
/// # Errors
/// Returns [`BotMemoryError::SourceResolution`] when the target is unauthorized or
/// no message matches `source_time`.
///
/// # Panics
/// Never panics; internal non-empty target check is guaranteed by the branch above.
pub async fn resolve_store_fact_source(
    db: &ChatDb,
    bot_id: &str,
    bot_agent_id: &str,
    source_time: &str,
    source_agent: Option<&str>,
) -> Result<ResolvedFactSource, BotMemoryError> {
    let trimmed_time = source_time.trim();
    if trimmed_time.is_empty() {
        return Err(BotMemoryError::SourceResolution(
            "store_fact requires a non-empty UTC source_time".to_string(),
        ));
    }
    let target = source_agent.map(str::trim).filter(|s| !s.is_empty());
    let is_self = target.is_none() || target == Some(bot_id) || target == Some(bot_agent_id);
    if is_self {
        let Some(entry) = db.find_bot_inbox_by_time(bot_id, trimmed_time).await? else {
            return Err(BotMemoryError::SourceResolution(format!(
                "no message in bot `{bot_agent_id}` inbox matches UTC timestamp `{trimmed_time}`"
            )));
        };
        let sender = entry
            .sender_name
            .or(entry.sender_id)
            .unwrap_or_else(|| entry.kind.as_str().to_string());
        return Ok(ResolvedFactSource {
            source_agent: bot_agent_id.to_string(),
            source_sender: sender,
            source_time: entry.ts_utc,
            verbatim_quote: entry.body,
        });
    }

    let target_id = target.expect("checked non-empty");

    // 1. Check if `target_id` is a delegated worker owned by this bot.
    if let Some(delegate) = db.get_bot_delegate(target_id).await? {
        if delegate.bot_id != bot_id {
            return Err(BotMemoryError::SourceResolution(format!(
                "delegated worker `{target_id}` does not belong to bot `{bot_agent_id}`"
            )));
        }
        let Some(msg) = db
            .find_bot_delegate_message_by_time(bot_id, target_id, trimmed_time)
            .await?
        else {
            return Err(BotMemoryError::SourceResolution(format!(
                "no message in delegated worker `{target_id}` matches UTC timestamp `{trimmed_time}`"
            )));
        };
        return Ok(ResolvedFactSource {
            source_agent: delegate.agent_id,
            source_sender: msg.role,
            source_time: msg.ts_utc,
            verbatim_quote: msg.content,
        });
    }

    // 2. Check if `target_id` is a session supervised by this bot.
    let supervisor = db.supervising_bot_for_session(target_id).await?;
    if supervisor.as_deref() != Some(bot_id) {
        return Err(BotMemoryError::SourceResolution(format!(
            "source_agent `{target_id}` is neither this bot, one of its delegated workers, nor a supervised session"
        )));
    }
    let messages = db.get_messages(target_id).await?;
    let target_ms = may_protocol::bot::parse_utc_to_ms(trimmed_time).ok();
    let matched = messages.into_iter().find(|msg| {
        msg.timestamp.as_deref().is_some_and(|ts| {
            ts == trimmed_time
                || target_ms
                    .zip(may_protocol::bot::parse_utc_to_ms(ts).ok())
                    .is_some_and(|(a, b)| a == b)
        })
    });
    let Some(msg) = matched else {
        return Err(BotMemoryError::SourceResolution(format!(
            "no message in supervised session `{target_id}` matches UTC timestamp `{trimmed_time}`"
        )));
    };
    Ok(ResolvedFactSource {
        source_agent: target_id.to_string(),
        source_sender: msg.role,
        source_time: msg.timestamp.unwrap_or_else(|| trimmed_time.to_string()),
        verbatim_quote: msg.content,
    })
}

/// Commits the verified `staging_dir` working tree into separated `memory.git`
/// (`--git-dir`/`--work-tree`) with one provenance commit and returns the commit SHA.
///
/// # Errors
/// Returns [`BotMemoryError::Git`] when git staging or commit fails.
///
/// # Panics
/// Panics if the compiled Askama commit-message template fails to render.
#[allow(clippy::disallowed_methods, reason = "runs inside blocking::unblock")]
pub fn commit_memory_staging_sync(
    git_dir: &Path,
    staging_dir: &Path,
    fact: &str,
    source: &ResolvedFactSource,
) -> Result<String, BotMemoryError> {
    init_if_needed(git_dir)?;
    let add_out = memory_git_base()
        .arg("--git-dir")
        .arg(git_dir)
        .arg("--work-tree")
        .arg(staging_dir)
        .args(["add", "--all"])
        .output()?;
    if !add_out.status.success() {
        return Err(BotMemoryError::Git(format!(
            "git add failed: {}",
            String::from_utf8_lossy(&add_out.stderr).trim()
        )));
    }
    let subject = fact
        .lines()
        .next()
        .unwrap_or("update memory")
        .trim()
        .chars()
        .take(72)
        .collect::<String>();
    let commit_msg = MemoryCommitTemplate {
        subject: if subject.is_empty() {
            "update memory"
        } else {
            &subject
        },
        fact,
        source_agent: &source.source_agent,
        source_sender: &source.source_sender,
        source_time: &source.source_time,
        verbatim_quote: &source.verbatim_quote,
    }
    .render()
    .expect("memory commit template must render");

    let commit_out = memory_git_base()
        .arg("--git-dir")
        .arg(git_dir)
        .arg("--work-tree")
        .arg(staging_dir)
        .args([
            "-c",
            "user.name=May Bot Memory Curator",
            "-c",
            "user.email=bot@may.local",
            "commit",
            "--allow-empty",
            "-m",
            &commit_msg,
        ])
        .output()?;
    if !commit_out.status.success() {
        return Err(BotMemoryError::Git(format!(
            "git commit failed: {}",
            String::from_utf8_lossy(&commit_out.stderr).trim()
        )));
    }
    let rev_out = memory_git_base()
        .arg("--git-dir")
        .arg(git_dir)
        .args(["rev-parse", "HEAD"])
        .output()?;
    if !rev_out.status.success() {
        return Err(BotMemoryError::Git(format!(
            "git rev-parse HEAD failed: {}",
            String::from_utf8_lossy(&rev_out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&rev_out.stdout).trim().to_string())
}

#[allow(
    clippy::disallowed_methods,
    reason = "helper for commit_memory_staging_sync"
)]
fn init_if_needed(git_dir: &Path) -> Result<(), BotMemoryError> {
    if !git_dir.join("HEAD").exists() {
        init_memory_git_repo(git_dir)?;
    }
    Ok(())
}

/// Lists recent commits from `memory.git` (newest first).
///
/// # Errors
/// Returns [`BotMemoryError::Git`] on git error.
#[allow(clippy::disallowed_methods, reason = "runs inside blocking::unblock")]
pub fn list_memory_commits_sync(
    git_dir: &Path,
    limit: usize,
) -> Result<Vec<BotMemoryCommit>, BotMemoryError> {
    if !git_dir.join("HEAD").exists() {
        return Ok(Vec::new());
    }
    // Check whether HEAD has at least one commit yet.
    let head = memory_git_base()
        .arg("--git-dir")
        .arg(git_dir)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()?;
    if !head.status.success() {
        return Ok(Vec::new());
    }
    let sep = "\x1f";
    let rec = "\x1e";
    let format = format!("%H{sep}%s{sep}%an{sep}%aI{sep}%B{rec}");
    let out = memory_git_base()
        .arg("--git-dir")
        .arg(git_dir)
        .args(["log", &format!("-n{limit}"), &format!("--format={format}")])
        .output()?;
    if !out.status.success() {
        return Err(BotMemoryError::Git(format!(
            "git log failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut commits = Vec::new();
    for raw in text.split(rec) {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            continue;
        }
        let mut parts = trimmed.splitn(5, sep);
        let sha = parts.next().unwrap_or_default().trim().to_string();
        let subject = parts.next().unwrap_or_default().trim().to_string();
        let author = parts.next().unwrap_or_default().trim().to_string();
        let timestamp = parts.next().unwrap_or_default().trim().to_string();
        let message = parts.next().unwrap_or_default().trim().to_string();
        if !sha.is_empty() {
            commits.push(BotMemoryCommit {
                sha,
                subject,
                message,
                author,
                timestamp,
            });
        }
    }
    Ok(commits)
}

// ── Hybrid RAG & Context-Ledger Deduplication ───────────────────

/// One rendered memory hit inside `<memory-recall>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecalledMemoryHit {
    /// Normalized leaf path (e.g. `preference/food.md`).
    pub path: String,
    /// `true` when this leaf was already seen earlier in the uncompacted window
    /// but its `blake3` hash changed after a new `store_fact` commit.
    pub updated: bool,
    /// Leaf Markdown content (or excerpt).
    pub content: String,
}

#[derive(Template)]
#[template(path = "bot/memory_recall.txt", escape = "none")]
struct MemoryRecallTemplate<'a> {
    hits: &'a [RecalledMemoryHit],
}

/// Hybrid `repo-search` RAG manager for a bot's leaf memory files with
/// context-ledger deduplication (`seen_in_window`).
#[derive(Debug, Clone)]
pub struct BotMemoryRag<B: CollectionBackend> {
    backend: B,
}

impl<B: CollectionBackend> BotMemoryRag<B> {
    /// Creates a RAG manager backed by `backend`.
    #[must_use]
    pub const fn new(backend: B) -> Self {
        Self { backend }
    }

    /// Incrementally upserts added/modified leaf `.md` files and deletes removed
    /// ones from `may-bot-memory-<bot_id>` (never indexing `MEMORY.md`).
    ///
    /// # Errors
    /// Returns [`BotMemoryError`] if reading files or updating `repo-search` fails.
    pub async fn sync_incremental(
        &self,
        bot_id: &str,
        previous_hashes: &HashMap<String, String>,
        memory_root: &Path,
    ) -> Result<HashMap<String, String>, BotMemoryError> {
        let root = memory_root.to_path_buf();
        let current = blocking::unblock(move || collect_leaf_files(&root)).await?;
        let collection = bot_memory_collection(bot_id);

        let mut to_upsert = Vec::new();
        let mut next_hashes = HashMap::with_capacity(current.len());
        for (rel_path, (hash, content)) in current {
            if previous_hashes.get(&rel_path) != Some(&hash) {
                to_upsert.push(Document {
                    id: rel_path.clone(),
                    title: rel_path.clone(),
                    text: content,
                });
            }
            next_hashes.insert(rel_path, hash);
        }

        let to_delete: Vec<String> = previous_hashes
            .keys()
            .filter(|rel| !next_hashes.contains_key(*rel))
            .cloned()
            .collect();

        if !to_delete.is_empty() {
            self.backend.delete(&collection, to_delete).await?;
        }
        if !to_upsert.is_empty() {
            self.backend.upsert(&collection, to_upsert).await?;
        }
        Ok(next_hashes)
    }

    /// Queries `may-bot-memory-<bot_id>` (`WireMode::Hybrid`) for `query`.
    ///
    /// Deduplicates against `ledger.seen_in_window` by normalized path and
    /// `blake3` hash, updates `ledger`, and renders `<memory-recall>` if any
    /// new or updated leaf hits remain.
    ///
    /// # Errors
    /// Returns [`BotMemoryError`] when the search backend fails.
    ///
    /// # Panics
    /// Panics if the compiled Askama `<memory-recall>` template fails to render.
    pub async fn recall_for_event(
        &self,
        bot_id: &str,
        query: &str,
        memory_root: &Path,
        ledger: &mut SeenMemoryLedger,
        max_hits: usize,
    ) -> Result<Option<String>, BotMemoryError> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        let collection = bot_memory_collection(bot_id);
        let hits = self.backend.search(&collection, trimmed).await?;
        let mut selected = Vec::new();

        for hit in hits {
            if selected.len() >= max_hits {
                break;
            }
            let Ok(rel_path) = normalize_leaf_path(&hit.id) else {
                continue;
            };
            let full = memory_root.join(&rel_path);
            let Ok(content) = async_fs::read_to_string(&full).await else {
                continue;
            };
            let file_hash = blake3_hex(&content);
            let chunk_text = hit.excerpt.as_deref().unwrap_or(&content);
            let chunk_hash = blake3_hex(chunk_text);

            let updated = match ledger.seen_in_window.get(&rel_path) {
                Some(seen) if seen.file_hash == file_hash => {
                    // Already read by `recall` or injected in this uncompacted window.
                    continue;
                }
                Some(_) => true,
                None => false,
            };

            ledger.seen_in_window.insert(
                rel_path.clone(),
                SeenMemory {
                    file_hash,
                    chunk_hashes: vec![chunk_hash],
                },
            );
            selected.push(RecalledMemoryHit {
                path: rel_path,
                updated,
                content,
            });
        }

        if selected.is_empty() {
            return Ok(None);
        }
        let rendered = MemoryRecallTemplate { hits: &selected }
            .render()
            .expect("memory recall template must render");
        Ok(Some(rendered))
    }
}

/// Records that `recall(path)` showed `rel_path` with `file_hash` in the active
/// uncompacted window, preventing redundant RAG injection of the same revision.
pub fn record_leaf_seen(ledger: &mut SeenMemoryLedger, rel_path: &str, file_hash: String) {
    ledger.seen_in_window.insert(
        rel_path.to_string(),
        SeenMemory {
            file_hash,
            chunk_hashes: Vec::new(),
        },
    );
}

/// Resets `seen_in_window` after compaction so post-compaction turns can recall
/// relevant leaf files afresh.
pub fn reset_seen_window(ledger: &mut SeenMemoryLedger) {
    ledger.seen_in_window.clear();
}

// ── Confined Curator Session & Serial FIFO Pipeline ─────────────

#[derive(Template)]
#[template(path = "bot/curator_prompt.md", escape = "none")]
struct CuratorPromptTemplate<'a> {
    bot_id: &'a str,
    fact: &'a str,
    source_agent: &'a str,
    source_sender: &'a str,
    source_time: &'a str,
    verbatim_quote: &'a str,
}

#[derive(Template)]
#[template(path = "bot/curator_violations.txt", escape = "none")]
struct CuratorViolationsTemplate<'a> {
    violations: &'a [String],
}

/// Warm curator session bound to `staging_dir`.
///
/// The curator starts on the initial `store_fact` prompt and stays warm so any
/// structural violations detected by [`verify_memory_tree`] are fed back into
/// the same session before committing.
pub trait CuratorSession: Send {
    /// Sends `prompt` to the curator and waits for it to finish editing `staging_dir`.
    fn step<'a>(
        &'a mut self,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BotMemoryError>> + Send + 'a>>;
}

/// Future returned by [`CuratorFactory::open_session`].
pub type CuratorSessionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Box<dyn CuratorSession>, BotMemoryError>> + Send + 'a>>;

/// Factory that creates a confined [`CuratorSession`] over `staging_dir`.
pub trait CuratorFactory: Send + Sync + 'static {
    /// Creates a new confined curator session rooted strictly at `staging_dir`.
    fn open_session(&self, bot_id: &str, staging_dir: PathBuf) -> CuratorSessionFuture<'_>;
}

/// Runs one `store_fact` item end-to-end.
///
/// 1. Resolves `(source_agent, source_time)` in SQLite (failing fast if invalid/unauthorized).
/// 2. Mirrors `memory/` -> `memory.staging/`.
/// 3. Runs the confined curator session on `memory.staging/`.
/// 4. Runs `verify_memory_tree(memory.staging/)`; on any violation, feeds the
///    rendered violation prompt back into the SAME warm curator session (up to
///    `MAX_CURATOR_RETRIES`).
/// 5. Commits `memory.staging/` to `memory.git` with the provenance template.
/// 6. Atomically publishes `memory.staging/` -> `memory/`.
/// 7. Incrementally syncs leaf `.md` files into `repo-search` (`may-bot-memory-<bot_id>`).
///
/// # Errors
/// Returns [`BotMemoryError`] if resolution, curation, invariant verification,
/// git commit, atomic publish, or RAG sync fails.
///
/// # Panics
/// Panics if a compiled Askama curator prompt template fails to render.
#[allow(clippy::too_many_arguments, clippy::implicit_hasher)]
pub async fn run_one_store_fact<B: CollectionBackend>(
    db: &ChatDb,
    bot_id: &str,
    bot_agent_id: &str,
    paths: &BotPaths,
    fact: &str,
    source_time: &str,
    source_agent: Option<&str>,
    curator_factory: &dyn CuratorFactory,
    rag: &BotMemoryRag<B>,
    previous_leaf_hashes: &mut HashMap<String, String>,
) -> Result<String, BotMemoryError> {
    ensure_bot_memory_initialized(paths).await?;
    let source =
        resolve_store_fact_source(db, bot_id, bot_agent_id, source_time, source_agent).await?;

    let live_dir = paths.memory_dir();
    let staging_dir = paths.memory_staging_dir();
    let git_dir = paths.memory_git_dir();

    // Mirror current live tree into isolated staging copy.
    {
        let src = live_dir.clone();
        let dst = staging_dir.clone();
        blocking::unblock(move || mirror_dir_sync(&src, &dst)).await?;
    }

    let initial_prompt = CuratorPromptTemplate {
        bot_id,
        fact,
        source_agent: &source.source_agent,
        source_sender: &source.source_sender,
        source_time: &source.source_time,
        verbatim_quote: &source.verbatim_quote,
    }
    .render()
    .expect("curator prompt template must render");

    let mut session = curator_factory
        .open_session(bot_id, staging_dir.clone())
        .await?;
    session.step(&initial_prompt).await?;

    for attempt in 0..=MAX_CURATOR_RETRIES {
        let check_dir = staging_dir.clone();
        let violations = blocking::unblock(move || verify_memory_tree(&check_dir)).await?;
        if violations.is_empty() {
            break;
        }
        if attempt == MAX_CURATOR_RETRIES {
            let summary = violations
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ");
            return Err(BotMemoryError::Curation(format!(
                "memory tree still violates invariants after {MAX_CURATOR_RETRIES} retries: {summary}"
            )));
        }
        let messages: Vec<String> = violations.into_iter().map(|v| v.message).collect();
        let feedback = CuratorViolationsTemplate {
            violations: &messages,
        }
        .render()
        .expect("curator violations template must render");
        session.step(&feedback).await?;
    }

    // Commit verified staging tree to separated memory.git.
    let commit_sha = {
        let git_dir = git_dir.clone();
        let staging_dir = staging_dir.clone();
        let fact = fact.to_string();
        let source = source.clone();
        blocking::unblock(move || {
            commit_memory_staging_sync(&git_dir, &staging_dir, &fact, &source)
        })
        .await?
    };

    // Atomically publish staging -> live memory/.
    {
        let staging_dir = staging_dir.clone();
        let live_dir = live_dir.clone();
        blocking::unblock(move || publish_staging_atomically(&staging_dir, &live_dir)).await?;
    }

    // Incrementally sync leaf .md files to repo-search (excluding MEMORY.md).
    let next_hashes = rag
        .sync_incremental(bot_id, previous_leaf_hashes, &live_dir)
        .await?;
    *previous_leaf_hashes = next_hashes;

    Ok(commit_sha)
}

/// Real bash-first confined curator session backed by `aither_agent::Agent`.
///
/// Confines `aither_sandbox::Shell` strictly to `staging_dir` with restricted
/// offline network policy and read-only subagent permissions (no `unsafe`).
pub struct ConfinedShellCuratorSession<LLM: LanguageModel + 'static> {
    agent: Agent<LLM, (), LLM>,
}

impl<LLM: LanguageModel + 'static> std::fmt::Debug for ConfinedShellCuratorSession<LLM> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfinedShellCuratorSession")
            .finish_non_exhaustive()
    }
}

fn curator_audit_log_path(staging_dir: &Path) -> PathBuf {
    staging_dir
        .parent()
        .map_or_else(std::env::temp_dir, Path::to_path_buf)
        .join(".curator-network-audit.log")
}

impl<LLM: LanguageModel + 'static> ConfinedShellCuratorSession<LLM> {
    /// Builds a confined bash-first curator agent over `staging_dir` with no
    /// network access and no `unsafe` escalation.
    ///
    /// # Errors
    /// Returns [`BotMemoryError::Curation`] if the confined sandbox cannot start.
    pub async fn build(
        llm: LLM,
        parameters: aither_core::llm::model::Parameters,
        staging_dir: PathBuf,
        tools: crate::tools::ToolEnvironments,
    ) -> Result<Self, BotMemoryError> {
        let restricted_policy = NetworkPolicyConfig {
            mode: crate::network_policy::NetworkPolicyMode::Restricted,
            hosts: Vec::new(),
        }
        .compile()
        .map_err(|error| BotMemoryError::Curation(error.to_string()))?;
        let permissions = crate::terminal_permission_handler::TerminalPermissionHandler::for_agent(
            None,
            restricted_policy,
            None,
        )
        .for_read_only_subagent();

        let grants = Grants::new();
        grants.add_writable(&staging_dir);
        let budgets = crate::shell::ShellBudgets::of(&crate::config::ShellSettings::default());
        let audit_log =
            heel::NetworkAuditLog::file(curator_audit_log_path(&staging_dir)).map_err(|error| {
                BotMemoryError::Curation(format!("curator audit log failed: {error}"))
            })?;
        let assembly = crate::shell::ShellAssembly {
            tools,
            container: None,
            working_dir: staging_dir.clone(),
            grants,
            priority: heel::ProcessPriority::default(),
            audit_log,
            permissions,
            env_source: None,
            budgets,
            interpreters: vec![Interpreter::Shell],
            policies: crate::command_policy::PolicySet {
                managed_bash: false,
                refused: crate::command_policy::RefusedReads::default(),
                host_command_notices: crate::command_policy::HostCommandNotices::default(),
                read_epochs: crate::command_policy::FileReadEpochs::default(),
                checkout_reads: None,
                write_lease: None,
            },
            slot: crate::ipc::ShellSlot::new(),
            ssh_servers: Vec::new(),
            ids: None,
            spaces: crate::agent::subagent_spaces::SessionSubagentSpaces::without_clones(),
            pool: None,
            connectivity: None,
            jobs: None,
        };
        let registry = crate::ipc::CommandRegistryBuilder::new().build();
        let shell = assembly
            .build(registry)
            .await
            .map_err(|error| BotMemoryError::Curation(format!("curator shell failed: {error}")))?;
        let terminal = shell.tool();
        let agent = Agent::builder(llm)
            .request_parameters(parameters)
            .max_iterations(24)
            .tool(terminal)
            .build();
        Ok(Self { agent })
    }
}

impl<LLM: LanguageModel + 'static> CuratorSession for ConfinedShellCuratorSession<LLM> {
    fn step<'a>(
        &'a mut self,
        prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<(), BotMemoryError>> + Send + 'a>> {
        Box::pin(async move {
            let mut outcome = None;
            self.agent.push_message(Message::user(prompt));
            let run = self.agent.resume();
            let _ = run
                .drive((), std::future::pending::<()>(), |(), timed| {
                    if let AgentEvent::RunEnded { outcome: ended, .. } = timed.event {
                        outcome = Some(ended);
                    }
                    std::future::ready(std::ops::ControlFlow::<(), _>::Continue(()))
                })
                .await
                .map_err(|error| BotMemoryError::Curation(error.to_string()))?;
            match outcome {
                Some(RunOutcome::Failed { error }) => {
                    Err(BotMemoryError::Curation(error.to_string()))
                }
                Some(RunOutcome::Cancelled) => Err(BotMemoryError::Curation(
                    "curator run cancelled".to_string(),
                )),
                _ => Ok(()),
            }
        })
    }
}

/// Serial per-bot FIFO `store_fact` worker handle.
#[derive(Clone)]
pub struct BotMemoryCuratorQueue {
    wake_tx: async_channel::Sender<()>,
}

impl std::fmt::Debug for BotMemoryCuratorQueue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BotMemoryCuratorQueue")
            .finish_non_exhaustive()
    }
}

impl BotMemoryCuratorQueue {
    /// Spawns a serial FIFO background task for `bot_id` that drains
    /// `bot_store_fact_queue` one item at a time (`1 store_fact = 1 curator run = 1 git commit`).
    pub fn spawn<B, OnCommit>(
        db: ChatDb,
        bot_id: String,
        bot_agent_id: String,
        paths: BotPaths,
        curator_factory: Arc<dyn CuratorFactory>,
        rag: BotMemoryRag<B>,
        on_commit: OnCommit,
    ) -> Self
    where
        B: CollectionBackend + 'static,
        OnCommit: Fn(String, Option<String>) + Send + Sync + 'static,
    {
        let (wake_tx, wake_rx) = async_channel::unbounded();
        let on_commit = Arc::new(on_commit);
        tokio::spawn(async move {
            let mut leaf_hashes = {
                let dir = paths.memory_dir();
                blocking::unblock(move || collect_leaf_files(&dir))
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(k, (h, _))| (k, h))
                    .collect::<HashMap<String, String>>()
            };
            loop {
                loop {
                    let next = match db.next_pending_bot_store_fact(&bot_id).await {
                        Ok(Some(item)) => item,
                        Ok(None) => break,
                        Err(error) => {
                            tracing::warn!(bot_id, %error, "failed to poll bot store_fact queue");
                            break;
                        }
                    };
                    match run_one_store_fact(
                        &db,
                        &bot_id,
                        &bot_agent_id,
                        &paths,
                        &next.fact,
                        &next.source_time,
                        next.source_agent.as_deref(),
                        curator_factory.as_ref(),
                        &rag,
                        &mut leaf_hashes,
                    )
                    .await
                    {
                        Ok(sha) => {
                            let _ = db.finish_bot_store_fact(next.id, Some(&sha), None).await;
                            on_commit(bot_id.clone(), Some(sha));
                        }
                        Err(error) => {
                            tracing::warn!(bot_id, queue_id = next.id, %error, "bot store_fact curation failed");
                            let _ = db
                                .finish_bot_store_fact(next.id, None, Some(&error.to_string()))
                                .await;
                        }
                    }
                }
                if wake_rx.recv().await.is_err() {
                    break;
                }
            }
        });
        Self { wake_tx }
    }

    /// Nudges the serial curator queue to process newly enqueued `store_fact` rows.
    pub fn wake(&self) {
        let _ = self.wake_tx.try_send(());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use may_db::{NewBotInboxEntry, NewBotRow};
    use may_protocol::bot::{BotApprovalPolicy, BotInboxKind};
    use may_repo_search::backend::testing::MemoryCollections;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    #[test]
    fn leaf_path_normalization_enforces_leaf_only_invariants() {
        assert_eq!(
            normalize_leaf_path("preference/food").expect("ok"),
            "preference/food.md"
        );
        assert_eq!(
            normalize_leaf_path("preference/food.md").expect("ok"),
            "preference/food.md"
        );
        assert!(normalize_leaf_path("").is_err());
        assert!(normalize_leaf_path("   ").is_err());
        assert!(normalize_leaf_path("MEMORY.md").is_err());
        assert!(normalize_leaf_path("preference/MEMORY.md").is_err());
        assert!(normalize_leaf_path("preference/MEMORY").is_err());
        assert!(normalize_leaf_path("preference/").is_err());
        assert!(normalize_leaf_path("../escape.md").is_err());
        assert!(normalize_leaf_path("/etc/passwd").is_err());
        assert!(normalize_leaf_path("preference/food.json").is_err());
    }

    #[test]
    fn verify_memory_tree_catches_missing_stale_non_md_and_symlinks() {
        let dir = tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::create_dir_all(root.join("preference")).expect("mkdir");
        std::fs::write(
            root.join("MEMORY.md"),
            "# Root\n- `preference/`\n- `ghost.md`\n",
        )
        .expect("write");
        std::fs::write(
            root.join("preference/MEMORY.md"),
            "# Prefs\n- `food.md`: food preferences\n",
        )
        .expect("write");
        std::fs::write(
            root.join("preference/food.md"),
            "Likes 四川火锅, ラーメン, 김치찌개.\n",
        )
        .expect("write");
        std::fs::write(root.join("preference/unlisted.md"), "Unlisted\n").expect("write");
        std::fs::write(root.join("preference/bad.txt"), "not markdown").expect("write");

        let violations = verify_memory_tree(root).expect("verify");
        let joined = violations
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            joined.contains("stale reference to missing file `ghost.md`"),
            "{joined}"
        );
        assert!(
            joined.contains("does not list direct child file `unlisted.md`"),
            "{joined}"
        );
        assert!(joined.contains("non-Markdown file `bad.txt`"), "{joined}");

        // Fix all violations and verify clean pass.
        std::fs::remove_file(root.join("preference/bad.txt")).expect("rm");
        std::fs::write(
            root.join("MEMORY.md"),
            "# Root\n- `preference/`: user preferences\n",
        )
        .expect("write");
        std::fs::write(
            root.join("preference/MEMORY.md"),
            "# Prefs\n- `food.md`: CJK food\n- `unlisted.md`: misc\n",
        )
        .expect("write");
        let clean = verify_memory_tree(root).expect("verify clean");
        assert!(clean.is_empty(), "expected clean tree, got {clean:?}");
    }

    struct ScriptedCurator {
        staging: PathBuf,
        calls: Arc<AtomicUsize>,
        broken_first: bool,
    }

    impl CuratorSession for ScriptedCurator {
        fn step<'a>(
            &'a mut self,
            prompt: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<(), BotMemoryError>> + Send + 'a>> {
            let prompt = prompt.to_string();
            Box::pin(async move {
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                let pref_dir = self.staging.join("preference");
                std::fs::create_dir_all(&pref_dir)?;
                if self.broken_first && n == 0 {
                    // Deliberately omit preference/MEMORY.md on first step to test feedback loop.
                    std::fs::write(
                        pref_dir.join("food.md"),
                        "User loves 麻辣烫, 寿司, 비빔밥.\n",
                    )?;
                    return Ok(());
                }
                if self.broken_first && n == 1 {
                    assert!(
                        prompt.contains("MEMORY.md"),
                        "violation feedback must be fed into warm curator: {prompt}"
                    );
                }
                std::fs::write(
                    self.staging.join("MEMORY.md"),
                    "# Root\n- `preference/`: user preferences\n",
                )?;
                std::fs::write(
                    pref_dir.join("MEMORY.md"),
                    "# Preference\n- `food.md`: favorite CJK foods\n",
                )?;
                let content = if prompt.contains("ramen update") {
                    "User loves 麻辣烫, 味噌ラーメン, 비빔밥.\n"
                } else {
                    "User loves 麻辣烫, 寿司, 비빔밥.\n"
                };
                std::fs::write(pref_dir.join("food.md"), content)?;
                Ok(())
            })
        }
    }

    struct ScriptedCuratorFactory {
        calls: Arc<AtomicUsize>,
        broken_first: bool,
    }

    impl CuratorFactory for ScriptedCuratorFactory {
        fn open_session(
            &self,
            _bot_id: &str,
            staging_dir: PathBuf,
        ) -> Pin<
            Box<dyn Future<Output = Result<Box<dyn CuratorSession>, BotMemoryError>> + Send + '_>,
        > {
            let calls = self.calls.clone();
            let broken_first = self.broken_first;
            Box::pin(async move {
                Ok(Box::new(ScriptedCurator {
                    staging: staging_dir,
                    calls,
                    broken_first,
                }) as Box<dyn CuratorSession>)
            })
        }
    }

    #[tokio::test]
    async fn store_fact_verifies_retries_commits_and_deduplicates_rag_recall() {
        let dir = tempdir().expect("tempdir");
        let db = ChatDb::open(&dir.path().join("history.db"))
            .await
            .expect("db");
        let policy = BotApprovalPolicy::default();
        db.create_bot(NewBotRow {
            id: "bot-1",
            agent_id: "calm-coral-owl",
            name: "Bridge",
            persona: None,
            model: None,
            profile_id: None,
            enabled: true,
            approval_policy: &policy,
        })
        .await
        .expect("bot");

        let t0: jiff::Timestamp = "2026-06-20T09:00:00Z".parse().expect("ts");
        let msg = db
            .enqueue_bot_inbox_at(
                NewBotInboxEntry {
                    bot_id: "bot-1",
                    kind: BotInboxKind::UserMessage,
                    sender_id: Some("user"),
                    sender_name: Some("Lexo"),
                    body: "I love 麻辣烫, 寿司, and 비빔밥!",
                    meta_json: None,
                },
                t0,
            )
            .await
            .expect("inbox");

        let paths = BotPaths::new(dir.path().join("bots/bot-1"));
        let backend = MemoryCollections::default();
        let rag = BotMemoryRag::new(backend.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let factory = ScriptedCuratorFactory {
            calls: calls.clone(),
            broken_first: true,
        };
        let mut leaf_hashes = HashMap::new();

        let sha1 = run_one_store_fact(
            &db,
            "bot-1",
            "calm-coral-owl",
            &paths,
            "Record CJK food preferences",
            &msg.ts_utc,
            None,
            &factory,
            &rag,
            &mut leaf_hashes,
        )
        .await
        .expect("store_fact 1");
        assert_ne!(sha1, "");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "warm curator must receive violation feedback and retry"
        );

        // Check git provenance commit in separated memory.git.
        let commits = list_memory_commits_sync(&paths.memory_git_dir(), 10).expect("commits");
        assert_eq!(commits.len(), 1);
        assert!(commits[0].message.contains("Record CJK food preferences"));
        assert!(
            commits[0]
                .message
                .contains("I love 麻辣烫, 寿司, and 비빔밥!")
        );
        assert!(commits[0].message.contains("calm-coral-owl"));
        assert!(commits[0].message.contains(&msg.ts_utc));

        // Verify MEMORY.md is NOT indexed in repo-search, only leaf files.
        let indexed_ids = backend.ids(&bot_memory_collection("bot-1"));
        assert_eq!(indexed_ids, vec!["preference/food.md".to_string()]);

        // First RAG query injects `<memory-recall>` without [UPDATED].
        let mut ledger = SeenMemoryLedger::default();
        let recalled1 = rag
            .recall_for_event("bot-1", "麻辣烫", &paths.memory_dir(), &mut ledger, 5)
            .await
            .expect("rag 1")
            .expect("should recall");
        assert!(recalled1.contains("<memory-recall>"));
        assert!(recalled1.contains("preference/food.md"));
        assert!(!recalled1.contains("[UPDATED]"));

        // Second identical query in the same uncompacted window -> ZERO duplicate injection!
        let recalled2 = rag
            .recall_for_event("bot-1", "麻辣烫", &paths.memory_dir(), &mut ledger, 5)
            .await
            .expect("rag 2");
        assert!(
            recalled2.is_none(),
            "must not duplicate in uncompacted window"
        );

        // New store_fact updates food.md -> hash changes -> re-injected with [UPDATED]!
        let factory2 = ScriptedCuratorFactory {
            calls: Arc::new(AtomicUsize::new(0)),
            broken_first: false,
        };
        let _sha2 = run_one_store_fact(
            &db,
            "bot-1",
            "calm-coral-owl",
            &paths,
            "ramen update: prefer miso ramen",
            &msg.ts_utc,
            None,
            &factory2,
            &rag,
            &mut leaf_hashes,
        )
        .await
        .expect("store_fact 2");
        let recalled3 = rag
            .recall_for_event("bot-1", "麻辣烫", &paths.memory_dir(), &mut ledger, 5)
            .await
            .expect("rag 3")
            .expect("updated leaf must re-inject");
        assert!(recalled3.contains("[UPDATED]"), "{recalled3}");

        // After compaction reset, same leaf is recalled fresh again.
        reset_seen_window(&mut ledger);
        let recalled4 = rag
            .recall_for_event("bot-1", "麻辣烫", &paths.memory_dir(), &mut ledger, 5)
            .await
            .expect("rag 4")
            .expect("recalled after compaction reset");
        assert!(!recalled4.contains("[UPDATED]"), "{recalled4}");
    }

    #[tokio::test]
    async fn curator_audit_log_lives_outside_staging_directory() {
        let dir = tempdir().expect("tempdir");
        let paths = BotPaths::new(dir.path().join("bots/bot-audit"));
        ensure_bot_memory_initialized(&paths).await.expect("init");
        let staging = paths.memory_staging_dir();
        mirror_dir_sync(&paths.memory_dir(), &staging).expect("mirror");

        let audit_path = curator_audit_log_path(&staging);
        assert_eq!(audit_path, paths.root().join(".curator-network-audit.log"));
        let _audit = heel::NetworkAuditLog::file(audit_path.clone()).expect("open audit log");

        assert!(audit_path.exists());
        assert!(
            !staging.join(".curator-network-audit.log").exists(),
            "curator network audit log must never pollute memory.staging"
        );
        let violations = verify_memory_tree(&staging).expect("verify staging");
        assert!(
            violations.is_empty(),
            "staging tree must remain free of violations: {violations:?}"
        );
    }
}
