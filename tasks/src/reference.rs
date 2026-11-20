//! References to tracked work, and the words of a row's text.

use std::fmt;
use std::hash::{Hash, Hasher};

/// What a [`WorkRef`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum WorkKind {
    /// An issue.
    Issue,
    /// A pull request.
    PullRequest,
}

/// A reference to an issue or a pull request.
///
/// [`parse_all`](Self::parse_all) reads `#12` or `owner/repo#12` as an
/// issue, and `PR #12`, `PR owner/repo#12`, `!12` or `group/sub/repo!12` as
/// a pull request. `MR` works everywhere `PR` does. The repository
/// qualifier is two or more `/`-separated segments: `owner/repo`, a GitLab
/// subgroup path such as `group/sub/repo`, or a host-prefixed coordinate
/// such as `gitlab.example.com/group/sub/repo`. An unqualified reference
/// means the session's own repository; what that is, and where the item
/// lives, is the tracker's business.
///
/// Repository names compare without regard to ASCII case, as forges treat
/// them; the written spelling is kept for display.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WorkRef {
    kind: WorkKind,
    repository: Option<String>,
    number: u64,
    /// Whether a pull request is written `!12`, GitLab style, rather than
    /// `PR #12`. Display only, never part of the reference's identity, so
    /// `PR #12` and `!12` compare equal and hash alike.
    #[serde(default)]
    bang: bool,
}

impl PartialEq for WorkRef {
    fn eq(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.number == other.number
            && match (&self.repository, &other.repository) {
                (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
                (None, None) => true,
                _ => false,
            }
    }
}

impl Eq for WorkRef {}

impl Hash for WorkRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.kind.hash(state);
        self.number.hash(state);
        match &self.repository {
            Some(repository) => {
                state.write_u8(1);
                for byte in repository.bytes() {
                    state.write_u8(byte.to_ascii_lowercase());
                }
            }
            None => state.write_u8(0),
        }
    }
}

impl WorkRef {
    /// Issue `number` of the session's repository.
    #[must_use]
    pub const fn issue(number: u64) -> Self {
        Self {
            kind: WorkKind::Issue,
            repository: None,
            number,
            bang: false,
        }
    }

    /// Pull request `number` of the session's repository.
    #[must_use]
    pub const fn pull_request(number: u64) -> Self {
        Self {
            kind: WorkKind::PullRequest,
            repository: None,
            number,
            bang: false,
        }
    }

    /// Merge request `number` of the session's repository: a pull request
    /// written `!N`, the way GitLab spells it.
    #[must_use]
    pub const fn merge_request(number: u64) -> Self {
        Self {
            kind: WorkKind::PullRequest,
            repository: None,
            number,
            bang: true,
        }
    }

    /// The same reference, qualified by `repository` (`owner/repo`, or a
    /// longer path such as `group/sub/repo`).
    #[must_use]
    pub fn in_repository(mut self, repository: impl Into<String>) -> Self {
        self.repository = Some(repository.into());
        self
    }

    /// Whether this names an issue or a pull request.
    #[must_use]
    pub const fn kind(&self) -> WorkKind {
        self.kind
    }

    /// The repository it is qualified by; `None` for the session's own.
    #[must_use]
    pub fn repository(&self) -> Option<&str> {
        self.repository.as_deref()
    }

    /// The item's number.
    #[must_use]
    pub const fn number(&self) -> u64 {
        self.number
    }

    /// Whether a pull request is written `!N` rather than `PR #N`: display
    /// only, so equality, hashing and [`matches`](Self::matches) ignore it.
    #[must_use]
    pub const fn uses_bang(&self) -> bool {
        self.bang
    }

    /// Whether `self` and `other` can name the same item: the same kind and
    /// number, in the same repository or with either one unqualified.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        self.kind == other.kind
            && self.number == other.number
            && match (&self.repository, &other.repository) {
                (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
                _ => true,
            }
    }

    /// Every reference written in `text`, in order.
    #[must_use]
    pub fn parse_all(text: &str) -> Vec<Self> {
        let mut references = Vec::new();
        let mut previous: Option<&str> = None;
        for token in words(text) {
            let pull_request = previous.is_some_and(|word| {
                word.eq_ignore_ascii_case("PR") || word.eq_ignore_ascii_case("MR")
            });
            if let Some(reference) = parse_token(token, pull_request) {
                references.push(reference);
            }
            previous = Some(token);
        }
        references
    }
}

impl fmt::Display for WorkRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.bang {
            if let Some(repository) = &self.repository {
                f.write_str(repository)?;
            }
            return write!(f, "!{}", self.number);
        }
        if self.kind == WorkKind::PullRequest {
            f.write_str("PR ")?;
        }
        if let Some(repository) = &self.repository {
            f.write_str(repository)?;
        }
        write!(f, "#{}", self.number)
    }
}

/// The words of `text`, split at whitespace and at the punctuation that
/// surrounds a reference in prose or markdown.
///
/// A row's ids are matched against them, and a mark's payload is read with
/// them, so markdown
/// and punctuation around a word do not hide it.
pub fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                ',' | ';' | ':' | '(' | ')' | '[' | ']' | '{' | '}' | '`' | '"' | '\'' | '*'
            )
    })
    .map(|token| token.trim_end_matches(['.', '!', '?']))
    .filter(|token| !token.is_empty())
}

/// Parses one token; `pull_request` when the word before it was `PR` or
/// `MR`.
fn parse_token(token: &str, pull_request: bool) -> Option<WorkRef> {
    // The sigil is the first `#` or `!`, both ASCII, so `at + 1` is a char
    // boundary.
    let at = token.find(['#', '!'])?;
    let (scope, rest) = token.split_at(at);
    let bang = rest.starts_with('!');
    let rest = &rest[1..];
    // The number ends at the first non-digit, which must not continue a
    // word: `#8。` is issue 8, `#8th` is no reference.
    let (digits, tail) = rest
        .find(|c: char| !c.is_ascii_digit())
        .map_or((rest, ""), |end| rest.split_at(end));
    if digits.is_empty() || tail.chars().next().is_some_and(char::is_alphanumeric) {
        return None;
    }
    let number = digits.parse().ok()?;
    let (kind, repository) = if scope.is_empty() {
        (None, None)
    } else if scope.eq_ignore_ascii_case("PR") || scope.eq_ignore_ascii_case("MR") {
        (Some(WorkKind::PullRequest), None)
    } else if is_repository(scope) {
        (None, Some(scope.to_string()))
    } else {
        return None;
    };
    let kind = kind.unwrap_or(if bang || pull_request {
        WorkKind::PullRequest
    } else {
        WorkKind::Issue
    });
    Some(WorkRef {
        kind,
        repository,
        number,
        bang,
    })
}

/// Whether `scope` is a repository path: two or more `/`-separated
/// segments, each a forge path segment, so `owner/repo`, a GitLab subgroup
/// path `group/sub/repo` and a host-prefixed `gitlab.example.com/a/b`
/// qualify while a URL path (`//host/a/b`, whose first segment is empty)
/// or a stray slash does not.
fn is_repository(scope: &str) -> bool {
    scope.contains('/') && scope.split('/').all(is_repository_segment)
}

/// Whether `segment` is one forge path segment: `[A-Za-z0-9_.-]` only,
/// starting and ending in a letter or digit, so `.`, `..`, an empty part
/// and a punctuation edge are no segment.
fn is_repository_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_written_form_parses() {
        let refs = WorkRef::parse_all(
            "- #12 ship it (PR #3, owner/repo#4, PR other/repo#5); see `#6`. PR#7",
        );
        assert_eq!(
            refs,
            vec![
                WorkRef::issue(12),
                WorkRef::pull_request(3),
                WorkRef::issue(4).in_repository("owner/repo"),
                WorkRef::pull_request(5).in_repository("other/repo"),
                WorkRef::issue(6),
                WorkRef::pull_request(7),
            ]
        );
    }

    #[test]
    fn multi_segment_and_host_prefixed_paths_parse() {
        let refs = WorkRef::parse_all(
            "a/b/c#12 gitlab.example.com/org/team/sub/repo!99 \
             deep/group/a/b/c/repo#3 git.corp.example.com/team/sub/repo!4",
        );
        assert_eq!(
            refs,
            vec![
                WorkRef::issue(12).in_repository("a/b/c"),
                WorkRef::merge_request(99).in_repository("gitlab.example.com/org/team/sub/repo"),
                WorkRef::issue(3).in_repository("deep/group/a/b/c/repo"),
                WorkRef::merge_request(4).in_repository("git.corp.example.com/team/sub/repo"),
            ]
        );
    }

    #[test]
    fn a_merge_request_bang_parses_bare_qualified_and_introduced() {
        let refs = WorkRef::parse_all(
            "!42, group/repo!7, MR !8, PR !9, PR group/sub/repo!10, MR group/sub/repo!11",
        );
        assert_eq!(
            refs,
            vec![
                WorkRef::merge_request(42),
                WorkRef::merge_request(7).in_repository("group/repo"),
                WorkRef::merge_request(8),
                WorkRef::merge_request(9),
                WorkRef::merge_request(10).in_repository("group/sub/repo"),
                WorkRef::merge_request(11).in_repository("group/sub/repo"),
            ]
        );
        assert!(refs.iter().all(WorkRef::uses_bang));
    }

    #[test]
    fn pr_and_mr_words_mark_a_hash_reference_a_pull_request() {
        let refs = WorkRef::parse_all("PR group/sub/repo#42, MR #43, MR group/repo#44, MR#45");
        assert_eq!(
            refs,
            vec![
                WorkRef::pull_request(42).in_repository("group/sub/repo"),
                WorkRef::pull_request(43),
                WorkRef::pull_request(44).in_repository("group/repo"),
                WorkRef::pull_request(45),
            ]
        );
        assert!(refs.iter().all(|reference| !reference.uses_bang()));
    }

    #[test]
    fn the_two_pull_request_spellings_are_one_reference() {
        let bang = WorkRef::merge_request(42).in_repository("Group/Sub/Repo");
        let hash = WorkRef::pull_request(42).in_repository("group/sub/repo");
        assert_eq!(bang, hash, "the sigil is display only");
        assert!(bang.matches(&hash));
        assert!(bang.matches(&WorkRef::pull_request(42)));
        assert!(WorkRef::merge_request(42).matches(&hash));
        assert!(
            !bang.matches(&WorkRef::issue(42)),
            "an issue is not a pull request"
        );
        let set: std::collections::HashSet<_> =
            [bang.clone(), hash.clone(), WorkRef::merge_request(42)].into();
        assert_eq!(set.len(), 2, "equal references hash alike");
        assert!(bang.uses_bang());
        assert!(!hash.uses_bang());
        assert_ne!(bang.to_string(), hash.to_string());
    }

    #[test]
    fn anchors_headings_and_words_are_not_references() {
        assert_eq!(
            WorkRef::parse_all("## Roots page.html#3 #abc # 12 C#"),
            Vec::new()
        );
    }

    #[test]
    fn macros_and_comparisons_are_not_merge_requests() {
        assert_eq!(
            WorkRef::parse_all(
                "vec!12 println!(\"x\") foo!12 x!=5 a != b not!=7 y ! = 3 \
                 wow!42 50!9 fix!!11",
            ),
            Vec::new()
        );
    }

    #[test]
    fn urls_and_broken_paths_are_not_references() {
        assert_eq!(
            WorkRef::parse_all(
                "https://github.com/owner/repo#12 https://gitlab.com/a/b!3 \
                 https://gitlab.com/a/b/-/merge_requests/12 file:///a/b#4 \
                 /a/b#5 a//b#6 a/..#7 a/.#8 a/b/#9 a/-b#10 a/b-#11 ../a#13 ./a#14",
            ),
            Vec::new()
        );
    }

    #[test]
    fn non_ascii_text_around_a_reference_is_kept_apart() {
        assert_eq!(
            WorkRef::parse_all("修复 #8。和 PR #9 #10th"),
            vec![WorkRef::issue(8), WorkRef::pull_request(9)]
        );
    }

    #[test]
    fn cjk_prose_around_a_reference_is_kept_apart() {
        let reference = || WorkRef::merge_request(42).in_repository("group/sub/repo");
        for text in [
            "修复 group/sub/repo!42 的问题",
            "已修复 group/sub/repo!42。",
            "group/sub/repo!42 を修正した。",
            "group/sub/repo!42 수정 완료",
        ] {
            assert_eq!(WorkRef::parse_all(text), vec![reference()], "{text}");
        }
        assert_eq!(
            WorkRef::parse_all("group/sub/repo!42修正"),
            Vec::new(),
            "a word continues past the number"
        );
    }

    #[test]
    fn display_writes_the_sigil_the_reference_was_made_with() {
        for (reference, display) in [
            (WorkRef::issue(12), "#12"),
            (
                WorkRef::issue(4).in_repository("owner/repo"),
                "owner/repo#4",
            ),
            (WorkRef::pull_request(3), "PR #3"),
            (WorkRef::pull_request(5).in_repository("o/r"), "PR o/r#5"),
            (WorkRef::merge_request(9), "!9"),
            (
                WorkRef::merge_request(2).in_repository("group/sub/repo"),
                "group/sub/repo!2",
            ),
        ] {
            assert_eq!(reference.to_string(), display);
        }
    }

    #[test]
    fn display_writes_the_form_it_parses_from() {
        for text in [
            "#12",
            "owner/repo#4",
            "a/b/c#6",
            "PR #3",
            "PR owner/repo#5",
            "!9",
            "group/sub/repo!2",
            "gitlab.example.com/org/repo!1",
        ] {
            let parsed = WorkRef::parse_all(text);
            assert_eq!(parsed.len(), 1, "{text}");
            assert_eq!(parsed[0].to_string(), text, "{text}");
        }
    }

    #[test]
    fn display_writes_a_spelled_out_word_in_its_canonical_form() {
        for (text, display) in [
            ("PR !6", "!6"),
            ("MR !7", "!7"),
            ("MR #8", "PR #8"),
            ("PR#10", "PR #10"),
            ("MR group/repo#11", "PR group/repo#11"),
        ] {
            let parsed = WorkRef::parse_all(text);
            assert_eq!(parsed.len(), 1, "{text}");
            assert_eq!(parsed[0].to_string(), display, "{text}");
        }
    }

    #[test]
    fn an_unqualified_reference_matches_any_repository() {
        let qualified = WorkRef::issue(4).in_repository("owner/repo");
        assert!(WorkRef::issue(4).matches(&qualified));
        assert!(!WorkRef::issue(4).in_repository("x/y").matches(&qualified));
        assert!(!WorkRef::pull_request(4).matches(&qualified));
    }

    #[test]
    fn repository_names_compare_without_case() {
        let written = WorkRef::issue(4).in_repository("May-Agent/may");
        let canonical = WorkRef::issue(4).in_repository("may-agent/MAY");
        assert!(written.matches(&canonical));
        assert_eq!(written, canonical);
        let set: std::collections::HashSet<_> = [written.clone(), canonical].into();
        assert_eq!(set.len(), 1, "equal references hash alike");
        assert_ne!(written, WorkRef::issue(4), "qualified is not unqualified");
        assert_eq!(written.to_string(), "May-Agent/may#4");
    }
}
