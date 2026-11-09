//! Content-addressed GHCR references for the container images.
//!
//! `build.rs` includes this module to bake `MAY_BASE_IMAGE` and
//! `MAY_WORKSPACE_IMAGE` into the binary, and the `container_image` CI
//! example prints the same references and stages the workspace context.
//! Keeping it to `std` + `sha2` is what makes that sharing work.

use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

/// GHCR repository of the shared base image (`container/base`).
pub const BASE_IMAGE_REPOSITORY: &str = "ghcr.io/may-agent/may-workspace-base";
/// GHCR repository of the workspace image (`container/workspace`).
pub const WORKSPACE_IMAGE_REPOSITORY: &str = "ghcr.io/may-agent/may-workspace";

/// Top-level context entries CI generates for the workspace build. Their
/// content is identified by the hash's other inputs (the pinned tool
/// distribution, the `tasks` sources and the heel library version), so
/// staging rewrites must not move the reference.
const GENERATED_WORKSPACE_ENTRIES: &[&str] = &["heel", "tasks", "tools"];

/// The files, relative to the repository, the `tasks` binary the workspace
/// image carries is built from; a change to one moves the workspace
/// reference.
pub const TASKS_SOURCES: &[&str] = &[
    "src/bin/tasks/main.rs",
    "src/tasks_md/mod.rs",
    "src/tasks_md/edit.rs",
    "src/tasks_md/schema.rs",
    "templates/tasks_command_usage.txt",
    "templates/tasks_command_refused.txt",
    "templates/tasks_command_done.txt",
    "templates/tasks_violation.txt",
];

/// The `<repository>:<content tag>` reference of the base image whose build
/// context is `context_dir`: every file under the context, by sorted
/// relative path and content.
///
/// # Errors
/// Returns an IO error when the context cannot be read.
pub fn base_image_ref(context_dir: &Path) -> io::Result<String> {
    let mut hasher = Sha256::new();
    for relative in context_files(context_dir)? {
        feed_file(&mut hasher, context_dir, &relative)?;
    }
    Ok(format!("{BASE_IMAGE_REPOSITORY}:{}", finish_tag(hasher)))
}

/// The `<repository>:<content tag>` reference of the workspace image.
///
/// Named by the base reference it layers on, the `context_dir` files minus
/// the generated `heel`/`tools`/`tasks` entries, the tool distribution
/// manifest bytes, the sources the `tasks` binary is built from and the heel
/// library version pinning the staged binary.
///
/// # Errors
/// Returns an IO error when the context cannot be read.
pub fn workspace_image_ref(
    context_dir: &Path,
    base_ref: &str,
    tool_distribution: &[u8],
    repository: &Path,
    heel_lib_version: &str,
) -> io::Result<String> {
    let mut hasher = Sha256::new();
    feed(&mut hasher, b"base-ref");
    feed(&mut hasher, base_ref.as_bytes());
    for relative in context_files(context_dir)? {
        if GENERATED_WORKSPACE_ENTRIES.contains(&first_component(&relative)) {
            continue;
        }
        feed_file(&mut hasher, context_dir, &relative)?;
    }
    feed(&mut hasher, b"tool-distribution");
    feed(&mut hasher, tool_distribution);
    feed(&mut hasher, b"tasks-sources");
    for relative in TASKS_SOURCES {
        feed_file(&mut hasher, repository, Path::new(relative))?;
    }
    feed(&mut hasher, b"heel-lib-version");
    feed(&mut hasher, heel_lib_version.as_bytes());
    Ok(format!(
        "{WORKSPACE_IMAGE_REPOSITORY}:{}",
        finish_tag(hasher)
    ))
}

/// The workspace context entries whose changes must rebuild the reference,
/// as sorted paths relative to `context_dir`, generated entries excluded.
///
/// # Errors
/// Returns an IO error when the context cannot be read.
pub fn workspace_hash_inputs(context_dir: &Path) -> io::Result<Vec<PathBuf>> {
    Ok(context_files(context_dir)?
        .into_iter()
        .filter(|relative| !GENERATED_WORKSPACE_ENTRIES.contains(&first_component(relative)))
        .collect())
}

/// Every regular file under `context_dir`, sorted by relative path.
fn context_files(context_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    collect(context_dir, context_dir, &mut files)?;
    files.sort();
    Ok(files)
}

fn collect(root: &Path, dir: &Path, files: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            collect(root, &path, files)?;
        } else if path.is_file() {
            files.push(path.strip_prefix(root).unwrap_or(&path).to_path_buf());
        }
    }
    Ok(())
}

fn first_component(relative: &Path) -> &str {
    match relative.components().next() {
        Some(Component::Normal(name)) => name.to_str().unwrap_or_default(),
        _ => "",
    }
}

/// Feed one length-prefixed field so adjacent fields cannot smear together.
fn feed(hasher: &mut Sha256, field: &[u8]) {
    hasher.update((field.len() as u64).to_le_bytes());
    hasher.update(field);
}

/// Feed a file as `<relative path>` then its streamed bytes, which are
/// always the last field of their entry so they run unframed.
fn feed_file(hasher: &mut Sha256, context_dir: &Path, relative: &Path) -> io::Result<()> {
    feed(hasher, relative.to_string_lossy().as_bytes());
    let mut file = fs::File::open(context_dir.join(relative))?;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(())
}

/// The image tag: the first 16 hex chars of the digest.
fn finish_tag(hasher: Sha256) -> String {
    let digest = hasher.finalize();
    let mut tag = String::with_capacity(16);
    for byte in &digest[..8] {
        write!(tag, "{byte:02x}").expect("hex formatting into a String cannot fail");
    }
    tag
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_context(dir: &Path) {
        fs::write(dir.join("Dockerfile"), "FROM scratch\n").expect("Dockerfile");
        fs::write(dir.join("entrypoint.sh"), "#!/bin/sh\n").expect("entrypoint");
    }

    #[test]
    fn base_ref_changes_with_context_content() {
        let context = tempfile::tempdir().expect("context");
        write_context(context.path());
        let before = base_image_ref(context.path()).unwrap();
        assert!(before.starts_with(&format!("{BASE_IMAGE_REPOSITORY}:")));
        assert_eq!(before.len(), BASE_IMAGE_REPOSITORY.len() + 17);

        fs::write(
            context.path().join("Dockerfile"),
            "FROM scratch\nRUN echo change\n",
        )
        .unwrap();
        assert_ne!(base_image_ref(context.path()).unwrap(), before);

        fs::remove_file(context.path().join("entrypoint.sh")).unwrap();
        assert_ne!(base_image_ref(context.path()).unwrap(), before);
    }

    #[test]
    fn workspace_ref_follows_every_input_but_the_generated_entries() {
        let context = tempfile::tempdir().expect("context");
        write_context(context.path());
        let repository = tempfile::tempdir().expect("repository");
        for source in TASKS_SOURCES {
            let path = repository.path().join(source);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, "source").unwrap();
        }
        let fixture = || {
            workspace_image_ref(
                context.path(),
                "base:ref",
                b"manifest",
                repository.path(),
                "1.0.0",
            )
            .unwrap()
        };
        let before = fixture();
        assert!(before.starts_with(&format!("{WORKSPACE_IMAGE_REPOSITORY}:")));

        for (base, manifest, version) in [
            ("base:other", b"manifest".as_slice(), "1.0.0"),
            ("base:ref", b"manifest-v2".as_slice(), "1.0.0"),
            ("base:ref", b"manifest".as_slice(), "1.0.1"),
        ] {
            assert_ne!(
                workspace_image_ref(context.path(), base, manifest, repository.path(), version)
                    .unwrap(),
                before,
            );
        }

        // The sources of the `tasks` binary are inputs too.
        let source = repository.path().join(TASKS_SOURCES[1]);
        fs::write(&source, "edited").unwrap();
        assert_ne!(fixture(), before);
        fs::write(&source, "source").unwrap();
        assert_eq!(fixture(), before);

        fs::write(context.path().join("Dockerfile"), "FROM scratch\nRUN x\n").unwrap();
        assert_ne!(fixture(), before);
        fs::write(context.path().join("Dockerfile"), "FROM scratch\n").unwrap();

        // Generated staging output never moves the reference.
        fs::create_dir_all(context.path().join("tools").join("key")).unwrap();
        fs::write(context.path().join("heel"), "binary").unwrap();
        fs::write(context.path().join("tasks"), "binary").unwrap();
        fs::write(context.path().join("tools").join("key").join("rg"), "tool").unwrap();
        assert_eq!(fixture(), before);
        assert_eq!(
            workspace_hash_inputs(context.path()).unwrap(),
            vec![PathBuf::from("Dockerfile"), PathBuf::from("entrypoint.sh")]
        );
    }
}
