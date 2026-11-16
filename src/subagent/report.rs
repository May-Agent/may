//! What the `subagent` command tells the model, one typed template each.

use std::path::PathBuf;

use aither_sandbox::{CommitLedger, CommitLedgerError, CommitMark, SubagentCommit};
use askama::Template;

use crate::harness::subagent_file::Listed;

/// The commits listed for one run of a subagent holding a commit grant, or
/// why they could not be listed, and what could not be done to its
/// repositories before the run.
pub struct CommitListing {
    listed: Result<Vec<SubagentCommit>, CommitLedgerError>,
    setbacks: Vec<String>,
}

impl CommitListing {
    /// Lists the commits a run made; `None` when it held no commit grant.
    pub async fn of(ledger: Option<&CommitLedger>, mark: Option<&CommitMark>) -> Option<Self> {
        match (ledger, mark) {
            (Some(ledger), Some(mark)) => Some(Self {
                listed: ledger.since(mark).await,
                setbacks: mark.setbacks().iter().map(ToString::to_string).collect(),
            }),
            _ => None,
        }
    }
}

/// What the result text reports about a run.
pub enum SubagentReport<'a> {
    /// The run was cancelled.
    Cancelled,
    /// The run produced a result.
    Completed {
        /// The tier it ran on; `None` for an external agent.
        tier: Option<&'static str>,
        /// The model.
        model: Option<&'a str>,
        /// What it answered.
        result: &'a str,
        /// The id that continues it.
        id: &'a str,
        /// Whether the id expires with the prompt cache.
        cache_bound: bool,
        /// Set when it kept needing cache keep-alives.
        alert: Option<&'a str>,
    },
}

/// The result of a run.
#[derive(Template)]
#[template(path = "subagent/result.txt", escape = "none")]
pub struct SubagentResult<'a> {
    /// The subagent's name.
    pub subagent: &'a str,
    /// How the run ended.
    pub outcome: SubagentReport<'a>,
    /// The commits it made.
    pub commits: Option<&'a CommitListing>,
    /// The working copies its space was prepared with.
    pub working_copies: &'a [PathBuf],
}

/// One message of a failed run's history.
#[derive(Debug)]
pub struct HistoryEntry {
    /// Who said it.
    pub role: aither_core::llm::Role,
    /// What it said.
    pub content: String,
    /// The tools it called.
    pub tool_names: Vec<String>,
}

/// The error of a failed run.
#[derive(Template)]
#[template(path = "subagent/error.txt", escape = "none")]
pub struct SubagentError<'a> {
    /// The subagent's name.
    pub subagent: &'a str,
    /// What failed.
    pub error: &'a str,
    /// The commits made before it failed.
    pub commits: Option<&'a CommitListing>,
    /// The working copies its space was prepared with.
    pub working_copies: &'a [PathBuf],
    /// The tail of its conversation.
    pub history: &'a [HistoryEntry],
}

/// A fresh subagent's first prompt when its space holds working copies.
#[derive(Template)]
#[template(path = "subagent/brief.txt", escape = "none")]
pub struct SubagentBrief<'a> {
    /// The working copies.
    pub working_copies: &'a [PathBuf],
    /// The brief.
    pub brief: &'a str,
}

/// Why a fork was not started.
#[derive(Debug, Clone, PartialEq)]
pub enum ForkRefusal {
    /// The command is bound to no shell to launch from.
    NoShell,
    /// The main agent has published no request.
    NothingSent,
    /// The advanced tier is not the model the main agent runs on.
    OtherModel {
        /// The main agent's model.
        main: String,
        /// The advanced tier's model.
        fork: String,
    },
    /// The main agent's context is within 10% of its compaction point.
    NearCompaction {
        /// Window usage, in percent.
        usage: f32,
        /// Compaction point, in percent.
        compaction: f32,
    },
}

/// The refusal of a fork.
#[derive(Template)]
#[template(path = "subagent/fork_refused.txt", escape = "none")]
pub struct ForkRefused<'a> {
    /// Why.
    pub reason: &'a ForkRefusal,
}

/// What a call that does not parse is told it can run.
#[derive(Template)]
#[template(path = "subagent/usage.txt", escape = "none")]
pub struct SubagentUsage<'a> {
    /// The names that resolve.
    pub subagents: &'a [Listed],
    /// What `fork` is for.
    pub fork: &'a str,
    /// The connected agents, by name, with their descriptions.
    pub externals: Vec<(&'a str, &'a str)>,
}

/// The alert for a run that kept needing cache keep-alives.
#[derive(Template)]
#[template(path = "subagent/idle_alert.txt", escape = "none")]
pub struct IdleAlert<'a> {
    /// The subagent.
    pub subagent: &'a str,
    /// Keep-alives sent.
    pub sent: usize,
    /// Why it last idled.
    pub reason: &'a str,
}

/// How a background task ended.
pub enum JobEnd {
    /// It exited.
    Exited {
        /// Its exit code.
        code: i32,
    },
    /// It was killed.
    Killed,
    /// It failed.
    Failed {
        /// What went wrong.
        error: String,
    },
}

/// The message that wakes a subagent for a finished background task.
#[derive(Template)]
#[template(path = "subagent/job_ended.txt", escape = "none")]
pub struct JobEnded<'a> {
    /// The task.
    pub task_id: &'a str,
    /// How it ended.
    pub end: JobEnd,
    /// The tool that reads its output.
    pub read_tool: &'a str,
}

/// Renders `template`.
///
/// # Panics
/// Panics when it fails to render, which typed input never does.
pub fn render(template: &impl Template) -> String {
    template
        .render()
        .expect("a subagent template renders typed input")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fork_refusal_says_what_to_do_or_why() {
        let near = render(&ForkRefused {
            reason: &ForkRefusal::NearCompaction {
                usage: 71.0,
                compaction: 78.0,
            },
        });
        assert!(
            near.contains("71%") && near.contains("78%") && near.contains("`compact`"),
            "{near}"
        );
        let other = render(&ForkRefused {
            reason: &ForkRefusal::OtherModel {
                main: "a".into(),
                fork: "b".into(),
            },
        });
        assert!(other.contains("'a'") && other.contains("'b'"), "{other}");
        for reason in [ForkRefusal::NoShell, ForkRefusal::NothingSent] {
            assert_ne!(render(&ForkRefused { reason: &reason }), "");
        }
    }

    #[test]
    fn a_result_reports_its_id_model_and_working_copies() {
        let copies = [PathBuf::from("/space/repo")];
        let text = render(&SubagentResult {
            subagent: "audit",
            outcome: SubagentReport::Completed {
                tier: Some("balanced"),
                model: Some("m"),
                result: "Two findings.",
                id: "amber",
                cache_bound: true,
                alert: None,
            },
            commits: None,
            working_copies: &copies,
        });
        assert!(
            text.contains("Two findings.") && text.contains("amber"),
            "{text}"
        );
        assert!(text.contains("/space/repo"), "{text}");
    }

    #[test]
    fn a_background_task_end_names_how_to_read_it() {
        let text = render(&JobEnded {
            task_id: "ember",
            end: JobEnd::Exited { code: 2 },
            read_tool: "terminal_read",
        });
        assert!(
            text.contains("ember") && text.contains("code 2") && text.contains("terminal_read"),
            "{text}"
        );
    }
}
