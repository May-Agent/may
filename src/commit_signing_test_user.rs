//! A throwaway user for tests: a fresh ed25519 key and a git configuration
//! naming it, standing in for the user's own. Nothing here reads the real
//! `~/.ssh`, agent or git configuration, and nothing mutates the process
//! environment: a fixture reads this user through [`HostGit::from_file`].

use std::path::{Path, PathBuf};
use std::process::Command;

use super::CommitSigning;
use crate::host_git::HostGit;

/// The test user's identity.
pub const NAME: &str = "Test User";
/// The test user's email.
pub const EMAIL: &str = "test.user@example.com";

/// Runs `git` with `args`, which must succeed.
fn git(args: &[&std::ffi::OsStr]) {
    let status = Command::new("git").args(args).status().unwrap();
    assert!(status.success(), "git {args:?}");
}

/// A user with a key and a global git configuration, both under one directory.
#[derive(Debug, Clone)]
pub struct TestUser {
    dir: PathBuf,
    key: PathBuf,
    config: PathBuf,
    signers: PathBuf,
}

impl TestUser {
    /// Generates the user's files under `dir`.
    pub fn new(dir: &Path) -> Self {
        Self::named(dir, NAME)
    }

    /// Generates the user's files under `dir`, the user going by `name`.
    pub fn named(dir: &Path, name: &str) -> Self {
        std::fs::create_dir_all(dir).unwrap();
        let key = dir.join("id_ed25519");
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-C", "test", "-f"])
            .arg(&key)
            .status()
            .unwrap();
        assert!(status.success());
        let config = dir.join("gitconfig");
        for (setting, value) in [
            ("user.name", name),
            ("user.email", EMAIL),
            ("user.signingkey", &key.to_string_lossy()),
            ("gpg.format", "ssh"),
            ("commit.gpgsign", "true"),
        ] {
            git(&[
                "config".as_ref(),
                "--file".as_ref(),
                config.as_os_str(),
                setting.as_ref(),
                value.as_ref(),
            ]);
        }
        let public = std::fs::read_to_string(dir.join("id_ed25519.pub")).unwrap();
        let signers = dir.join("allowed_signers");
        std::fs::write(&signers, format!("{EMAIL} namespaces=\"git\" {public}")).unwrap();
        Self {
            dir: dir.to_path_buf(),
            key,
            config,
            signers,
        }
    }

    /// Sets `setting` in the user's global git configuration.
    pub fn set(&self, setting: &str, value: &str) {
        git(&[
            "config".as_ref(),
            "--file".as_ref(),
            self.config.as_os_str(),
            setting.as_ref(),
            value.as_ref(),
        ]);
    }

    /// Removes `setting` from the user's global git configuration.
    pub fn unset(&self, setting: &str) {
        git(&[
            "config".as_ref(),
            "--file".as_ref(),
            self.config.as_os_str(),
            "--unset".as_ref(),
            setting.as_ref(),
        ]);
    }

    /// The user's git configuration as the host reads it.
    pub fn host(&self) -> HostGit {
        HostGit::from_file(&self.config)
    }

    /// The harness of `session` signing as this user.
    pub fn signing(&self, may_dir: &Path, session: &str) -> CommitSigning {
        CommitSigning::new(self.host(), may_dir, session)
    }

    /// The user's allowed-signers file, for `git verify-commit`.
    pub fn allowed_signers(&self) -> &Path {
        &self.signers
    }

    /// Installs `script` as the user's global `hook`, the way a user's
    /// `core.hooksPath` reaches every repository they run git in.
    pub fn install_global_hook(&self, hook: &str, script: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        let hooks = self.dir.join("hooks");
        std::fs::create_dir_all(&hooks).unwrap();
        let path = hooks.join(hook);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        git(&[
            "config".as_ref(),
            "--file".as_ref(),
            self.config.as_os_str(),
            "core.hooksPath".as_ref(),
            hooks.as_os_str(),
        ]);
    }

    /// Undoes [`Self::identify`], leaving `repo` to the configuration its
    /// reader brings.
    pub fn unidentify(repo: &Path) {
        for key in [
            "user.name",
            "user.email",
            "gpg.format",
            "user.signingkey",
            "commit.gpgsign",
        ] {
            git(&[
                "-C".as_ref(),
                repo.as_os_str(),
                "config".as_ref(),
                "--unset".as_ref(),
                key.as_ref(),
            ]);
        }
    }

    /// Makes plain host git in `repo` commit as this user and sign with the
    /// key file, the way the user's own checkout does.
    pub fn identify(&self, repo: &Path) {
        for (setting, value) in [
            ("user.name", NAME.to_string()),
            ("user.email", EMAIL.to_string()),
            ("gpg.format", "ssh".to_string()),
            ("user.signingkey", self.key.to_string_lossy().into_owned()),
            ("commit.gpgsign", "true".to_string()),
        ] {
            git(&[
                "-C".as_ref(),
                repo.as_os_str(),
                "config".as_ref(),
                setting.as_ref(),
                value.as_ref(),
            ]);
        }
    }
}
