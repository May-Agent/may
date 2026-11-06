//! Importing another agent's global rules and skills: `/import-claude` and
//! `/import-codex`.
//!
//! The daemon copies the agent's skills into the profile's skills, then opens
//! a main session whose first message is the [`RulesImportBrief`]. That
//! session's agent compares the source rules file with its own system prompt
//! and installs `~/.may/AGENTS.md` through the `global-rules` command, which
//! only that session has: `keep` copies the source byte for byte, and `write`
//! installs a rewrite only once the user has answered one of the session's
//! `ask_user` questions.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use aither_core::llm::{Tool, ToolContext, ToolResult};
use askama::Template;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::session::import::ForeignAgent;
use crate::skill::{SkillError, SkillManager};

/// `POST /api/rules/import` body.
#[derive(Debug, Clone, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RulesImportRequest {
    /// The agent to import from: `claude` or `codex`.
    pub source: String,
    /// Profile whose skills receive the copies and whose session imports the
    /// rules; the default profile when absent.
    #[serde(default)]
    pub profile_id: Option<String>,
}

/// `POST /api/rules/import` answer.
#[derive(Debug, Clone, Serialize, Deserialize, skyzen::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct RulesImportResponse {
    /// The session importing the rules; `None` when the agent has no global
    /// rules file, so only skills were copied.
    pub session_id: Option<String>,
    /// The agent's global rules file.
    pub rules_file: String,
    /// Skills copied into May.
    pub imported_skills: Vec<String>,
    /// Skills left out because May already has one by that name.
    pub skipped_skills: Vec<String>,
}

/// The rules one import session installs, and where.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RulesImport {
    source: ForeignAgent,
    rules_file: PathBuf,
    global_rules: PathBuf,
}

impl RulesImport {
    /// The import of `source`'s rules from the user's home `home` into the
    /// May home `may_dir`.
    #[must_use]
    pub fn new(source: ForeignAgent, home: &Path, may_dir: &Path) -> Self {
        Self {
            source,
            rules_file: source.rules_file(home),
            global_rules: crate::session::MayHomePaths::new(may_dir.to_path_buf())
                .global_agents_md(),
        }
    }

    /// The agent the rules come from.
    #[must_use]
    pub const fn source(&self) -> ForeignAgent {
        self.source
    }

    /// The source rules file.
    #[must_use]
    pub fn rules_file(&self) -> &Path {
        &self.rules_file
    }

    /// The session's first message.
    ///
    /// # Panics
    /// Panics if the compiled-in brief template fails to render.
    #[must_use]
    pub fn brief(&self) -> String {
        RulesImportBrief {
            product_name: self.source.product_name(),
            rules_file: self.rules_file.display().to_string(),
        }
        .render()
        .expect("the rules import brief renders")
    }
}

/// The import session's first message.
#[derive(Template)]
#[template(path = "rules_import_brief.md", escape = "none")]
struct RulesImportBrief {
    product_name: &'static str,
    rules_file: String,
}

/// What [`copy_skills`] did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SkillCopyReport {
    /// Skills copied, by directory name.
    pub imported: Vec<String>,
    /// Skills left out because one by that name exists.
    pub skipped: Vec<String>,
}

/// Copies every skill (a directory holding `SKILL.md`) under `source_dir`
/// into `manager`'s skills, in name order. A missing `source_dir` has no
/// skills; a skill whose name May already has is skipped.
///
/// # Errors
/// Returns [`SkillError`] when `source_dir` cannot be read or a skill fails to
/// copy or parse; the skills copied before it stay.
pub async fn copy_skills(
    source_dir: &Path,
    manager: &SkillManager,
) -> Result<SkillCopyReport, SkillError> {
    let dir = source_dir.to_path_buf();
    let skills = blocking::unblock(move || -> Result<Vec<PathBuf>, std::io::Error> {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut skills = Vec::new();
        for entry in entries {
            let path = entry?.path();
            // `is_file` follows links, so a linked skill directory counts.
            if path.join("SKILL.md").is_file() {
                skills.push(path);
            }
        }
        skills.sort();
        Ok(skills)
    })
    .await?;
    let mut report = SkillCopyReport::default();
    for skill in skills {
        match manager.register_from_local_path(&skill).await {
            Ok(info) => report.imported.push(info.dir_name),
            Err(SkillError::AlreadyExists(name)) => report.skipped.push(name),
            Err(error) => return Err(error),
        }
    }
    Ok(report)
}

/// Installs the imported global rules as ~/.may/AGENTS.md: `keep` installs the
/// source rules file unchanged; `write <file>` installs your rewrite, once the
/// user has answered your question about the changes.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum GlobalRulesArgs {
    /// Install the source rules file unchanged.
    Keep,
    /// Install `file`, the rules with the changes the user approved.
    Write {
        /// The rewritten rules file.
        file: String,
    },
}

/// Why `global-rules` failed.
#[derive(Debug, thiserror::Error)]
pub enum GlobalRulesError {
    /// The file to install could not be located.
    #[error(transparent)]
    Path(#[from] crate::agent_link::AgentLinkError),
    /// Reading the history failed.
    #[error("failed to read the session's answers: {0}")]
    Db(#[from] crate::db::ChatDbError),
    /// Copying or writing the rules failed.
    #[error("failed to install {} as {}: {source}", .from.display(), .to.display())]
    Install {
        /// The file installed.
        from: PathBuf,
        /// `~/.may/AGENTS.md`.
        to: PathBuf,
        /// The failure.
        #[source]
        source: std::io::Error,
    },
}

#[derive(Template)]
#[template(path = "global_rules_installed.txt", escape = "none")]
struct InstalledText<'a> {
    unchanged: bool,
    source: &'a str,
    written: &'a str,
    destination: &'a str,
}

#[derive(Template)]
#[template(path = "global_rules_unanswered.txt", escape = "none")]
struct UnansweredText;

/// The `global-rules` command of a rules import session.
#[derive(Debug, Clone)]
pub struct GlobalRulesTool {
    import: RulesImport,
    session_id: String,
    db: crate::db::ChatDb,
    sandbox_dir: PathBuf,
    workspace_mount: Option<aither_sandbox::MountRoot>,
}

impl GlobalRulesTool {
    /// The command for the session `session_id`, whose sandbox is
    /// `sandbox_dir` (mounted at `workspace_mount` in a container session).
    #[must_use]
    pub const fn new(
        import: RulesImport,
        session_id: String,
        db: crate::db::ChatDb,
        sandbox_dir: PathBuf,
        workspace_mount: Option<aither_sandbox::MountRoot>,
    ) -> Self {
        Self {
            import,
            session_id,
            db,
            sandbox_dir,
            workspace_mount,
        }
    }

    /// Runs `args`, returning the text the model reads.
    ///
    /// # Errors
    /// Returns [`GlobalRulesError`] when the file cannot be located, the
    /// history cannot be read or the rules cannot be installed.
    ///
    /// # Panics
    /// Panics if a compiled-in result template fails to render.
    pub async fn run(&self, args: GlobalRulesArgs) -> Result<String, GlobalRulesError> {
        let destination = &self.import.global_rules;
        let installed = match args {
            GlobalRulesArgs::Keep => {
                let from = self.import.rules_file.clone();
                async_fs::copy(&from, destination).await.map_err(|source| {
                    GlobalRulesError::Install {
                        from: from.clone(),
                        to: destination.clone(),
                        source,
                    }
                })?;
                InstalledText {
                    unchanged: true,
                    source: &from.display().to_string(),
                    written: "",
                    destination: &destination.display().to_string(),
                }
                .render()
            }
            GlobalRulesArgs::Write { file } => {
                if !self
                    .db
                    .session_has_ask_user_answer(&self.session_id)
                    .await?
                {
                    return Ok(UnansweredText
                        .render()
                        .expect("the unanswered refusal renders"));
                }
                let from = crate::agent_link::resolve_prompt_path(
                    &self.sandbox_dir,
                    self.workspace_mount.as_ref(),
                    &file,
                )?;
                async_fs::copy(&from, destination).await.map_err(|source| {
                    GlobalRulesError::Install {
                        from: from.clone(),
                        to: destination.clone(),
                        source,
                    }
                })?;
                InstalledText {
                    unchanged: false,
                    source: "",
                    written: &file,
                    destination: &destination.display().to_string(),
                }
                .render()
            }
        };
        Ok(installed.expect("the installed notice renders"))
    }
}

impl Tool for GlobalRulesTool {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("global-rules")
    }

    type Arguments = GlobalRulesArgs;
    type Res = ToolResult;

    async fn call(
        &self,
        args: Self::Arguments,
        _cx: ToolContext,
    ) -> aither_core::Result<Self::Res> {
        Ok(ToolResult::text(self.run(args).await?))
    }
}

#[cfg(test)]
#[path = "rules_import_tests.rs"]
mod tests;
