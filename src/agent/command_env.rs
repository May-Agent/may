//! The environment every command of a session gets: the session's secrets
//! and the path of its `tasks.md`.

use aither_sandbox::{EnvSource, LaunchInfo};

use crate::secrets::SessionEnv;

/// What a spawned command's environment holds beyond the sandbox's own.
///
/// `MAY_TASKS` names the session's `tasks.md` as the command sees it, so the
/// agent and the `tasks` command never build a relative path.
#[derive(Debug, Clone)]
pub struct CommandEnv {
    secrets: SessionEnv,
    tasks_md: String,
}

impl CommandEnv {
    /// The environment of a session with `secrets`, whose `tasks.md` is
    /// `tasks_md` inside the home environment the commands run in.
    #[must_use]
    pub fn new(secrets: SessionEnv, tasks_md: impl Into<String>) -> Self {
        Self {
            secrets,
            tasks_md: tasks_md.into(),
        }
    }
}

impl EnvSource for CommandEnv {
    fn env(&self, _launch: &LaunchInfo) -> Vec<(String, String)> {
        // `EnvSource` cannot fail a launch. A command that starts without its
        // secrets sends nothing unredacted anywhere; its result then fails the
        // redaction hook, which ends the run with the same error.
        let mut env = self.secrets.env().unwrap_or_else(|error| {
            tracing::error!(%error, "the session's secrets cannot be read; the command starts without them");
            Vec::new()
        });
        env.retain(|(name, _)| name != crate::tasks_md::ENV);
        env.push((crate::tasks_md::ENV.to_string(), self.tasks_md.clone()));
        env
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo_clone::test_sandbox::TestTerminal;
    use crate::secrets::SecretStore;

    fn env_of(dir: &std::path::Path, tasks_md: &str) -> CommandEnv {
        CommandEnv::new(SessionEnv::new(SecretStore::new(dir), "session"), tasks_md)
    }

    #[test]
    fn the_tasks_path_is_the_last_word_on_its_name() {
        let dir = tempfile::tempdir().expect("a temporary directory");
        let env = env_of(dir.path(), "/workspace/tasks.md").env(&LaunchInfo::default());
        let ours: Vec<_> = env
            .iter()
            .filter(|(name, _)| name == crate::tasks_md::ENV)
            .collect();
        assert_eq!(
            ours,
            [&(
                crate::tasks_md::ENV.to_string(),
                "/workspace/tasks.md".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn a_sandboxed_command_sees_the_session_tasks_path() {
        let session = tempfile::tempdir().expect("a temporary directory");
        let secrets = tempfile::tempdir().expect("a temporary directory");
        let tasks_md = session.path().join("tasks.md");
        let terminal = TestTerminal::with_env(
            session.path(),
            env_of(secrets.path(), &tasks_md.display().to_string()),
        )
        .await;
        let output = terminal
            .sandbox()
            .run(session.path(), "printenv MAY_TASKS")
            .await
            .expect("the command runs");
        assert_eq!(output.trim_end(), tasks_md.display().to_string());
    }
}
