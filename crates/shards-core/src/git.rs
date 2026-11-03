// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! A deliberately narrow wrapper around the `git` binary.
//!
//! Only the subcommands in [`ALLOWED`] can be run, and a few flags that would
//! rewrite or discard history are refused even on allowed subcommands. This is
//! the structural guarantee behind "no command may rewrite history": there is
//! no code path in shards that can call `rebase`, `reset`, `push --force`,
//! `commit --amend`, `filter-branch`, `update-ref`, etc.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::{Error, Result};

/// The only git subcommands shards ever runs.
pub const ALLOWED: &[&str] = &[
    "init",
    "add",
    "commit",
    "log",
    "show",
    "cat-file",
    "rev-parse",
    "rev-list",
    "symbolic-ref",
    "status",
    "diff-tree",
    "ls-files",
];

/// Flags refused on any subcommand because they rewrite or discard history.
pub const FORBIDDEN_FLAGS: &[&str] = &["--amend", "--force", "-f", "--fixup", "--squash"];

/// Identity and timestamp stamped onto a commit.
#[derive(Debug, Clone)]
pub struct Stamp {
    pub name: String,
    pub email: String,
    /// RFC 3339; used for both author and committer dates.
    pub date: String,
}

#[derive(Debug, Clone)]
pub struct Git {
    dir: PathBuf,
    extra_env: Vec<(String, String)>,
}

impl Git {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Git {
            dir: dir.into(),
            extra_env: Vec::new(),
        }
    }

    /// Extra environment for every git invocation (tests use this to isolate
    /// from the user's global git config).
    pub fn with_env(mut self, env: Vec<(String, String)>) -> Self {
        self.extra_env = env;
        self
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn check(args: &[&str]) -> Result<()> {
        let sub = args.first().copied().unwrap_or("");
        if !ALLOWED.contains(&sub) {
            return Err(Error::ForbiddenGit(sub.to_string()));
        }
        if let Some(flag) = args.iter().find(|a| FORBIDDEN_FLAGS.contains(a)) {
            return Err(Error::ForbiddenGit(format!("{sub} {flag}")));
        }
        Ok(())
    }

    fn command(&self, args: &[&str], stamp: Option<&Stamp>) -> Result<Command> {
        Self::check(args)?;
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.dir);
        // Never let a pager or editor block us.
        cmd.env("GIT_PAGER", "cat").env("GIT_EDITOR", "true");
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        if let Some(s) = stamp {
            cmd.env("GIT_AUTHOR_NAME", &s.name)
                .env("GIT_AUTHOR_EMAIL", &s.email)
                .env("GIT_AUTHOR_DATE", &s.date)
                .env("GIT_COMMITTER_NAME", &s.name)
                .env("GIT_COMMITTER_EMAIL", &s.email)
                .env("GIT_COMMITTER_DATE", &s.date);
        }
        cmd.args(args);
        Ok(cmd)
    }

    /// Run git, return stdout on success.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        self.run_stamped(args, None)
    }

    pub fn run_stamped(&self, args: &[&str], stamp: Option<&Stamp>) -> Result<String> {
        let out = self.command(args, stamp)?.stdin(Stdio::null()).output()?;
        if !out.status.success() {
            return Err(Error::Git {
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run git and report only whether it exited 0 (for existence probes).
    pub fn probe(&self, args: &[&str]) -> Result<bool> {
        let status = self
            .command(args, None)?
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        Ok(status.success())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_rewriting_commands_are_refused() {
        let g = Git::new(".");
        for args in [
            &["rebase", "main"][..],
            &["reset", "--hard", "HEAD~1"],
            &["push", "origin", "main"],
            &["filter-branch"],
            &["update-ref", "refs/heads/main", "HEAD~1"],
            &["branch", "-f", "main", "HEAD~1"],
            &["checkout", "."],
            &["gc", "--prune=now"],
            &["reflog", "expire"],
            &["commit", "--amend", "-m", "x"],
            &["commit", "--fixup", "HEAD"],
            &["add", "--force", "x"],
        ] {
            assert!(
                matches!(g.run(args), Err(Error::ForbiddenGit(_))),
                "{args:?} must be refused"
            );
        }
    }
}
