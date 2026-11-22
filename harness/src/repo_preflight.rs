//! The checkout-state preflight of a session that starts in a GitHub
//! repository's checkout.
//!
//! A checkout that is clean and holds the remote's latest work proceeds
//! without a question. A dirty one, or one that is behind or has diverged
//! from the remote, asks how the session should start: on the latest clean
//! remote, on this checkout as it stands, or not at all.

use std::path::Path;

use askama::Template;

use crate::ask_user::{AnswerValue, Question, QuestionAnswer};
use crate::host_git::{HostGit, HostGitError};

/// Where a checkout stands against the remote's default branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remote {
    /// The checkout holds the remote's latest commit (it may also be ahead).
    Current,
    /// The remote has commits the checkout lacks.
    Behind,
    /// Both sides have commits the other lacks.
    Diverged,
}

/// What the preflight found in a checkout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckoutState {
    /// Uncommitted changes or untracked files exist.
    pub dirty: bool,
    /// Where the checkout stands against the remote.
    pub remote: Remote,
}

impl CheckoutState {
    /// Whether the session start must ask: anything but clean and current.
    #[must_use]
    pub const fn needs_choice(self) -> bool {
        self.dirty || self.stale()
    }

    /// The remote has work the checkout lacks.
    #[must_use]
    pub const fn stale(self) -> bool {
        !matches!(self.remote, Remote::Current)
    }

    /// Basing on the checkout loses the GitHub issue and PR integration:
    /// the checkout is both stale and dirty.
    #[must_use]
    pub const fn loses_github(self) -> bool {
        self.dirty && self.stale()
    }
}

/// The ways a session can start from a checkout that needs a choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Base on the latest clean remote; local state is ignored entirely. The
    /// session is GitHub-bound.
    Remote,
    /// Base on this checkout, work in progress included. The session is
    /// local-bound.
    Checkout,
    /// The session does not work in this directory.
    Exit,
}

const REMOTE: &str = "remote";
const CHECKOUT: &str = "checkout";
const EXIT: &str = "exit";

impl Choice {
    /// The choice a question answer selects; anything unrecognized, a skipped
    /// question included, decides nothing.
    #[must_use]
    pub fn of_answer(answer: &QuestionAnswer) -> Option<Self> {
        let text = match &answer.answer {
            AnswerValue::Selected(values) => values.first()?.as_str(),
            AnswerValue::Custom(text) => text.as_str(),
            AnswerValue::Skipped => return None,
        };
        match text.split(" —").next().unwrap_or(text).trim() {
            REMOTE => Some(Self::Remote),
            CHECKOUT => Some(Self::Checkout),
            EXIT => Some(Self::Exit),
            _ => None,
        }
    }
}

/// Why a checkout could not be inspected.
#[derive(Debug, thiserror::Error)]
pub enum PreflightError {
    /// A git command failed.
    #[error(transparent)]
    Git(#[from] HostGitError),
    /// The remote named no commit for its default branch.
    #[error("the remote of {0} reported no HEAD")]
    NoRemoteHead(String),
    /// The remote could not be read: offline, or no access.
    #[error("the remote cannot be reached: {0}")]
    RemoteUnreachable(HostGitError),
}

/// Inspect `checkout` against the `origin` remote. Reads the remote with
/// `ls-remote` and never changes the checkout; blocking, so callers run it
/// off the event loop.
///
/// # Errors
/// Fails when git cannot read the checkout or the remote.
pub fn inspect(host: &HostGit, checkout: &Path) -> Result<CheckoutState, PreflightError> {
    inspect_against(host, checkout, "origin")
}

/// [`inspect`] against `remote`, a remote name or URL.
///
/// # Errors
/// Fails when git cannot read the checkout or the remote.
pub fn inspect_against(
    host: &HostGit,
    checkout: &Path,
    remote: &str,
) -> Result<CheckoutState, PreflightError> {
    let dirty = is_dirty(host, checkout)?;
    let listing = host
        .output(Some(checkout), &["ls-remote", remote, "HEAD"])
        .map_err(PreflightError::RemoteUnreachable)?;
    let remote_head = listing
        .split_whitespace()
        .next()
        .ok_or_else(|| PreflightError::NoRemoteHead(checkout.display().to_string()))?;
    let head = host.output(Some(checkout), &["rev-parse", "HEAD"])?;
    let remote = if head.trim() == remote_head {
        Remote::Current
    } else if !known_commit(host, checkout, remote_head) {
        Remote::Behind
    } else if is_ancestor(host, checkout, remote_head, "HEAD")? {
        Remote::Current
    } else if is_ancestor(host, checkout, "HEAD", remote_head)? {
        Remote::Behind
    } else {
        Remote::Diverged
    };
    Ok(CheckoutState { dirty, remote })
}

/// Whether `checkout` has uncommitted changes or untracked files.
///
/// # Errors
/// Fails when git cannot read the checkout.
pub fn is_dirty(host: &HostGit, checkout: &Path) -> Result<bool, PreflightError> {
    Ok(!host
        .output(Some(checkout), &["status", "--porcelain"])?
        .trim()
        .is_empty())
}

fn known_commit(host: &HostGit, dir: &Path, sha: &str) -> bool {
    let commit = format!("{sha}^{{commit}}");
    host.output(Some(dir), &["cat-file", "-e", &commit]).is_ok()
}

fn is_ancestor(
    host: &HostGit,
    dir: &Path,
    ancestor: &str,
    descendant: &str,
) -> Result<bool, PreflightError> {
    let status = host
        .command()
        .current_dir(dir)
        .args(["merge-base", "--is-ancestor", ancestor, descendant])
        .status()
        .map_err(HostGitError::from)?;
    Ok(status.success())
}

#[derive(Template)]
#[template(path = "repo_preflight_question.txt", escape = "none")]
struct QuestionText<'a> {
    checkout: &'a str,
    owner_repo: &'a str,
    dirty: bool,
    remote: Remote,
}

#[derive(Template)]
#[template(path = "repo_preflight_checkout_option.txt", escape = "none")]
struct CheckoutOption {
    loses_github: bool,
}

#[derive(Template)]
#[template(path = "repo_preflight_offline_question.txt", escape = "none")]
struct OfflineQuestionText<'a> {
    checkout: &'a str,
    owner_repo: &'a str,
    dirty: bool,
}

#[derive(Template)]
#[template(path = "repo_preflight_offline_option.txt", escape = "none")]
struct OfflineOption {
    dirty: bool,
}

/// The question a session start asks when `checkout`'s remote cannot be
/// reached: continue on the checkout as it is, or not at all. The answer is
/// read with [`Choice::of_answer`].
///
/// # Panics
/// Never: the templates render for every input.
#[must_use]
pub fn offline_question(checkout: &Path, owner_repo: &str, dirty: bool) -> Question {
    let checkout = checkout.display().to_string();
    Question {
        section: "checkout-state".to_string(),
        question: OfflineQuestionText {
            checkout: &checkout,
            owner_repo,
            dirty,
        }
        .render()
        .expect("the offline question renders")
        .trim()
        .to_string(),
        option: vec![
            OfflineOption { dirty }
                .render()
                .expect("the offline option renders")
                .trim()
                .to_string(),
            "exit — Do not work in this directory".to_string(),
        ],
        multi_select: false,
    }
}

/// The question a session start asks about `checkout`.
///
/// # Panics
/// Never: the templates render for every input.
#[must_use]
pub fn question(checkout: &Path, owner_repo: &str, state: CheckoutState) -> Question {
    let checkout = checkout.display().to_string();
    Question {
        section: "checkout-state".to_string(),
        question: QuestionText {
            checkout: &checkout,
            owner_repo,
            dirty: state.dirty,
            remote: state.remote,
        }
        .render()
        .expect("the preflight question renders")
        .trim()
        .to_string(),
        option: vec![
            "remote — Base on the latest clean remote; this checkout's state is ignored"
                .to_string(),
            CheckoutOption {
                loses_github: state.loses_github(),
            }
            .render()
            .expect("the preflight option renders")
            .trim()
            .to_string(),
            "exit — Do not work in this directory".to_string(),
        ],
        multi_select: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git");
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A bare `origin` and a checkout cloned from it, with one commit each.
    fn fixture() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let origin = tmp.path().join("origin.git");
        let seed = tmp.path().join("seed");
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(&seed).unwrap();
        git(&seed, &["init", "-q", "-b", "main"]);
        std::fs::write(seed.join("a.txt"), "a").unwrap();
        git(&seed, &["add", "."]);
        git(&seed, &["commit", "-qm", "init"]);
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        );
        git(
            tmp.path(),
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ],
        );
        (tmp, origin, checkout)
    }

    fn push_from_elsewhere(tmp: &Path, origin: &Path) {
        let other = tmp.join("other");
        git(
            tmp,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                other.to_str().unwrap(),
            ],
        );
        std::fs::write(other.join("b.txt"), "b").unwrap();
        git(&other, &["add", "."]);
        git(&other, &["commit", "-qm", "remote work"]);
        git(&other, &["push", "-q", "origin", "main"]);
    }

    #[test]
    fn a_clean_current_checkout_asks_nothing() {
        let (_tmp, _origin, checkout) = fixture();
        let state = inspect(&HostGit::from_file("/dev/null"), &checkout).unwrap();
        assert_eq!(
            state,
            CheckoutState {
                dirty: false,
                remote: Remote::Current
            }
        );
        assert!(!state.needs_choice());
    }

    #[test]
    fn a_local_commit_ahead_of_the_remote_is_still_current() {
        let (_tmp, _origin, checkout) = fixture();
        std::fs::write(checkout.join("c.txt"), "c").unwrap();
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-qm", "local"]);
        let state = inspect(&HostGit::from_file("/dev/null"), &checkout).unwrap();
        assert!(!state.needs_choice());
    }

    #[test]
    fn uncommitted_and_untracked_files_make_a_checkout_dirty() {
        let (_tmp, _origin, checkout) = fixture();
        std::fs::write(checkout.join("new.txt"), "x").unwrap();
        let state = inspect(&HostGit::from_file("/dev/null"), &checkout).unwrap();
        assert!(state.dirty && state.needs_choice());
        assert!(!state.loses_github());
    }

    #[test]
    fn a_checkout_behind_the_remote_is_stale() {
        let (tmp, origin, checkout) = fixture();
        push_from_elsewhere(tmp.path(), &origin);
        let state = inspect(&HostGit::from_file("/dev/null"), &checkout).unwrap();
        assert_eq!(state.remote, Remote::Behind);
        assert!(state.needs_choice() && !state.dirty);
    }

    #[test]
    fn a_checkout_with_its_own_commits_beside_new_remote_work_has_diverged() {
        let (tmp, origin, checkout) = fixture();
        std::fs::write(checkout.join("c.txt"), "c").unwrap();
        git(&checkout, &["add", "."]);
        git(&checkout, &["commit", "-qm", "local"]);
        push_from_elsewhere(tmp.path(), &origin);
        git(&checkout, &["fetch", "-q", "origin"]);
        let state = inspect(&HostGit::from_file("/dev/null"), &checkout).unwrap();
        assert_eq!(state.remote, Remote::Diverged);
    }

    #[test]
    fn stale_and_dirty_loses_the_github_integration() {
        let (tmp, origin, checkout) = fixture();
        push_from_elsewhere(tmp.path(), &origin);
        std::fs::write(checkout.join("new.txt"), "x").unwrap();
        let state = inspect(&HostGit::from_file("/dev/null"), &checkout).unwrap();
        assert!(state.loses_github());
        let question = question(&checkout, "acme/app", state);
        assert!(
            question.option[1].contains("May's own"),
            "{:?}",
            question.option
        );
    }

    #[test]
    fn answers_map_to_choices() {
        let answer = |text: &str| QuestionAnswer {
            section: "checkout-state".to_string(),
            answer: AnswerValue::Selected(vec![text.to_string()]),
        };
        assert_eq!(
            Choice::of_answer(&answer("remote — Base on the latest clean remote")),
            Some(Choice::Remote)
        );
        assert_eq!(
            Choice::of_answer(&answer("checkout — x")),
            Some(Choice::Checkout)
        );
        assert_eq!(Choice::of_answer(&answer("exit — x")), Some(Choice::Exit));
        assert_eq!(Choice::of_answer(&answer("maybe")), None);
    }
}
