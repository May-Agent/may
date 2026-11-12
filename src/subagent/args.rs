//! The arguments of the `subagent` command and what a call asks for.

use std::path::{Path, PathBuf};

use aither_sandbox::SubagentGrant;
use schemars::JsonSchema;
use serde::Deserialize;

/// Run a subagent: an autonomous agent with a context window of its own that
/// returns only its final result.
///
/// `subagent <name> --prompt-file <path>` runs the subagent `<name>` names;
/// run `subagent` alone to list the names that resolve. Its definition fixes
/// its role prompt, its model and what it may write: every subagent reads the
/// whole workspace and writes its own working directory, and one whose
/// definition allows it also writes the worktrees you grant. Three names are
/// reserved: `fork` continues your own context, `resume` continues a
/// finished subagent, and `external` runs a connected agent.
///
/// The brief is always read from a file: write it to a file first, then pass
/// its path. Inline prompt text is not a file path and is rejected.
#[derive(Debug, Clone, JsonSchema, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentArgs {
    /// The subagent to run, or `fork`, `resume` or `external`.
    pub name: String,
    /// Path to a file holding the brief, or for `resume` the follow-up. The
    /// file must exist and be non-empty.
    pub prompt_file: String,
    /// `resume` only: the id from a finished subagent's result, a single
    /// word. Ids expire with the provider's prompt cache.
    #[serde(default)]
    pub id: Option<String>,
    /// `external` only: the connected agent to run.
    #[serde(default)]
    pub agent: Option<String>,
    /// Directories, usually linked git worktrees you created, the subagent
    /// may write besides its own space, when its definition lets it write
    /// worktrees. Each must be inside what you can write. It cannot commit
    /// in these.
    #[serde(default)]
    pub worktrees: Vec<String>,
    /// Linked git worktrees the subagent may write and commit in, when its
    /// definition lets it write worktrees. Each must be on a branch
    /// `subagent/<id>/<name>` whose namespace `subagent/<id>/` holds no other
    /// branch. Every commit it makes is listed in its result.
    #[serde(default)]
    pub commit_worktrees: Vec<String>,
}

/// What a call asks for, checked against the reserved names.
#[derive(Debug)]
pub enum Call<'a> {
    /// Runs the subagent a name resolves to.
    Named {
        /// The name.
        name: &'a str,
        /// The directories it is granted.
        grant: Requested<'a>,
    },
    /// Continues the calling agent's context.
    Fork {
        /// The directories it is granted.
        grant: Requested<'a>,
    },
    /// Runs a connected agent.
    External {
        /// The connected agent.
        agent: &'a str,
        /// The directories it is granted.
        grant: Requested<'a>,
    },
    /// Continues a finished subagent.
    Resume {
        /// Its id.
        id: &'a str,
    },
}

/// The directories a call asks to grant, as written.
#[derive(Debug, Clone, Copy)]
pub struct Requested<'a> {
    worktrees: &'a [String],
    commit_worktrees: &'a [String],
}

impl Requested<'_> {
    /// Whether nothing is granted.
    pub const fn is_empty(&self) -> bool {
        self.worktrees.is_empty() && self.commit_worktrees.is_empty()
    }

    /// The writable and the committing directories, relative paths resolved
    /// against `working_dir`.
    fn paths(&self, working_dir: Option<&Path>) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let resolve = |path: &String| {
            let path = Path::new(path);
            match working_dir {
                Some(base) if path.is_relative() => base.join(path),
                _ => path.to_path_buf(),
            }
        };
        (
            self.worktrees.iter().map(resolve).collect(),
            self.commit_worktrees.iter().map(resolve).collect(),
        )
    }

    /// The grant beyond the subagent's own space.
    pub fn grant(&self, working_dir: Option<&Path>) -> SubagentGrant {
        let (worktrees, commit_worktrees) = self.paths(working_dir);
        let grant = worktrees
            .into_iter()
            .fold(SubagentGrant::new(), SubagentGrant::with_worktree);
        commit_worktrees
            .into_iter()
            .fold(grant, SubagentGrant::with_committing_worktree)
    }
}

impl SubagentArgs {
    /// The subagent a call runs, by the name its result reports; `None` for
    /// `resume`.
    #[must_use]
    pub fn subagent(&self) -> Option<&str> {
        match self.name.as_str() {
            "resume" => None,
            "external" => self.agent.as_deref(),
            name => Some(name),
        }
    }

    /// What the call asks for, refusing an option its name does not take.
    ///
    /// # Errors
    /// Returns the refusal the model reads.
    pub fn call(&self) -> anyhow::Result<Call<'_>> {
        let grant = Requested {
            worktrees: &self.worktrees,
            commit_worktrees: &self.commit_worktrees,
        };
        let name = self.name.as_str();
        if name != "resume" && self.id.is_some() {
            anyhow::bail!("`--id` continues a finished subagent: use it with `subagent resume`");
        }
        if name != "external" && self.agent.is_some() {
            anyhow::bail!("`--agent` names a connected agent: use it with `subagent external`");
        }
        Ok(match name {
            "resume" => {
                let Some(id) = self.id.as_deref() else {
                    anyhow::bail!(
                        "`subagent resume` takes the `--id` of the finished subagent to continue"
                    );
                };
                if !grant.is_empty() {
                    anyhow::bail!(
                        "a continued subagent keeps what it was granted: `subagent resume` takes no `--worktrees` or `--commit-worktrees`"
                    );
                }
                Call::Resume { id }
            }
            "fork" => Call::Fork { grant },
            "external" => {
                let Some(agent) = self.agent.as_deref() else {
                    anyhow::bail!("`subagent external` takes the `--agent` to run");
                };
                Call::External { agent, grant }
            }
            name => Call::Named { name, grant },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(name: &str) -> SubagentArgs {
        serde_json::from_value(serde_json::json!({"name": name, "prompt_file": "brief.md"}))
            .expect("arguments parse")
    }

    #[test]
    fn reserved_names_take_their_own_options() {
        assert!(matches!(args("fork").call(), Ok(Call::Fork { .. })));
        assert!(args("resume").call().is_err());
        assert!(args("external").call().is_err());
        let mut named = args("explore");
        named.id = Some("amber".into());
        assert!(named.call().is_err());
        named.id = None;
        named.agent = Some("devin".into());
        assert!(named.call().is_err());
    }

    #[test]
    fn a_resume_takes_no_grant() {
        let mut resume = args("resume");
        resume.id = Some("amber".into());
        assert!(matches!(resume.call(), Ok(Call::Resume { id: "amber" })));
        resume.worktrees.push("w".into());
        assert!(resume.call().is_err());
    }
}
