//! Transfer only the controller's global Git author defaults.
//!
//! Authentication and author identity are independent. A working GitHub login
//! does not make commits possible, and copying an entire Git config would also
//! transplant credential helpers, signing paths and machine-specific settings.

use super::transport::{Transport, shell_quote};
use anyhow::{Context, bail};
use std::path::Path;
use std::process::Command;

/// The two Git defaults used for authors and committers unless a repository
/// or an explicit environment override supplies its own identity.
pub struct GitIdentity {
    name: String,
    email: String,
}

impl GitIdentity {
    /// Read global config from the controller's home, so invoking bootstrap
    /// inside a repository does not copy that repository's identity instead.
    /// Missing or empty fields fail preflight before any target is changed.
    pub fn read(home: &Path) -> anyhow::Result<Self> {
        Self::read_with(|| {
            let mut git = Command::new("git");
            git.current_dir(home);
            git
        })
    }

    /// Inject the child command so tests can use real Git with isolated config
    /// files without changing the test process's environment or Git defaults.
    fn read_with(mut git: impl FnMut() -> Command) -> anyhow::Result<Self> {
        let mut value = |key: &str| -> anyhow::Result<String> {
            let output = git()
                // A caller may export these even when our cwd is outside its
                // checkout. They can activate repository-specific includeIf
                // sections in an otherwise global configuration read.
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .args(["config", "--global", "--includes", "--get", key])
                .output()
                .context("cannot read controller Git author configuration")?;
            if !output.status.success() {
                bail!(
                    "cannot read global Git {key}; configure it on the controller before bootstrap"
                );
            }
            let text = String::from_utf8(output.stdout).context("Git author info is not UTF-8")?;
            let text = text.strip_suffix('\n').unwrap_or(&text);
            if text.trim().is_empty() || text.contains(['\n', '\r', '\0']) {
                bail!("global Git {key} must be a nonempty single-line value");
            }
            Ok(text.to_owned())
        };
        Ok(Self {
            name: value("user.name")?,
            email: value("user.email")?,
        })
    }

    /// Set the intended user's global defaults, never root's or a repository's.
    /// Repeat after dotfiles installation because it can replace Git config.
    pub fn install(&self, target: &Transport) -> anyhow::Result<()> {
        target.run(&self.script())
    }

    /// Shell-quote identity data instead of putting it in an agent prompt.
    /// Explicit guards stop on write failures; reads also catch an include
    /// overriding the values we just wrote. Unrelated Git keys are preserved.
    fn script(&self) -> String {
        let name = shell_quote(&self.name);
        let email = shell_quote(&self.email);
        format!(
            "git config --global --replace-all -- user.name {name} || exit 1\n\
             git config --global --replace-all -- user.email {email} || exit 1\n\
             actual=$(git config --global --includes --get user.name) && [ \"$actual\" = {name} ] || exit 1\n\
             actual=$(git config --global --includes --get user.email) && [ \"$actual\" = {email} ] || exit 1\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Isolate Git's global file in each child, including the shell that runs
    /// the transfer. Real Git parses and updates the files under test.
    fn isolated(program: &str, config: &Path) -> Command {
        let mut command = Command::new(program);
        command.env("GIT_CONFIG_GLOBAL", config);
        command
    }

    /// A rerun must preserve target settings and quote author data literally;
    /// the source's signing and credential settings must never be imported.
    #[test]
    fn transfers_only_author_defaults_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::write(&source, "[user]\nname = O'Example $(false)\nemail = author@example.com\n[commit]\ngpgsign = true\n").unwrap();
        fs::write(
            &target,
            "[core]\neditor = nano\n[user]\nname = Old\nemail = old@example.com\n",
        )
        .unwrap();
        let identity = GitIdentity::read_with(|| isolated("git", &source)).unwrap();
        let mut first = Vec::new();
        for run in 0..2 {
            assert!(
                isolated("bash", &target)
                    .args(["-c", &identity.script()])
                    .status()
                    .unwrap()
                    .success()
            );
            let restored = GitIdentity::read_with(|| isolated("git", &target)).unwrap();
            assert_eq!(restored.name, "O'Example $(false)");
            assert_eq!(restored.email, "author@example.com");
            let contents = fs::read(&target).unwrap();
            assert!(String::from_utf8_lossy(&contents).contains("editor = nano"));
            assert!(!String::from_utf8_lossy(&contents).contains("gpgsign"));
            if run == 0 {
                first = contents;
            } else {
                assert_eq!(contents, first);
            }
        }
    }

    /// Missing or malformed identity must fail locally rather than silently
    /// inventing an address or leaving the target unable to commit.
    #[test]
    fn rejects_incomplete_author_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        for contents in [
            "",
            "[user]\nname = Example\n",
            "[user]\nname =\nemail = author@example.com\n",
            "[user]\nname = Example\nemail = \"a\\nb\"\n",
            "invalid config",
        ] {
            fs::write(&source, contents).unwrap();
            assert!(GitIdentity::read_with(|| isolated("git", &source)).is_err());
        }
    }

    /// Includes are part of the laptop's global defaults, and a fresh target
    /// must not need an existing Git config file before receiving them.
    #[test]
    fn reads_global_includes_and_initializes_fresh_target() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let target = dir.path().join("target");
        fs::write(&source, "[include]\npath = identity\n").unwrap();
        fs::write(
            dir.path().join("identity"),
            "[user]\nname = Example\nemail = author@example.com\n",
        )
        .unwrap();
        let identity = GitIdentity::read_with(|| isolated("git", &source)).unwrap();
        assert!(
            isolated("bash", &target)
                .args(["-c", &identity.script()])
                .status()
                .unwrap()
                .success()
        );
        let restored = GitIdentity::read_with(|| isolated("git", &target)).unwrap();
        assert_eq!(restored.name, "Example");
        assert_eq!(restored.email, "author@example.com");
    }

    /// An exported repository path must not activate a conditional global
    /// include and turn a per-repository identity into the devbox default.
    #[test]
    fn ignores_inherited_repository_selection() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo.git");
        assert!(
            Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&repo)
                .status()
                .unwrap()
                .success()
        );
        let source = dir.path().join("source");
        // macOS temporary paths may traverse /var's symlink; Git matches the
        // canonical gitdir, including the directory itself rather than children.
        fs::write(&source, format!("[user]\nname = Global Default\nemail = global@example.com\n[includeIf \"gitdir:{}\"]\npath = identity\n", repo.canonicalize().unwrap().display())).unwrap();
        fs::write(
            dir.path().join("identity"),
            "[user]\nname = Repository Selected\nemail = repository@example.com\n",
        )
        .unwrap();
        let command = || {
            let mut git = isolated("git", &source);
            git.current_dir(dir.path())
                .env("GIT_DIR", &repo)
                .env("GIT_COMMON_DIR", &repo)
                .env("GIT_WORK_TREE", dir.path());
            git
        };
        // Prove that the fixture would select the wrong identity without the
        // production environment isolation, so the test cannot pass vacuously.
        let selected = command()
            .args(["config", "--global", "--includes", "--get", "user.name"])
            .output()
            .unwrap();
        assert!(selected.status.success());
        assert_eq!(selected.stdout, b"Repository Selected\n");
        let identity = GitIdentity::read_with(command).unwrap();
        assert_eq!(identity.name, "Global Default");
        assert_eq!(identity.email, "global@example.com");
    }
}
