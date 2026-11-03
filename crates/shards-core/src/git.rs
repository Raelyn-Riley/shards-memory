// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! A deliberately narrow wrapper around the `git` binary.
//!
//! Only the subcommands in [`ALLOWED`] can be run, flags that rewrite or
//! discard history are refused on every subcommand, and a few subcommands get
//! extra rules (`merge` must be `--ff-only`; `branch` may only `-d` a branch
//! that is already merged). This is the structural guarantee behind "no
//! command may rewrite history": no code path in shards can call `rebase`,
//! `reset`, `push`, `update-ref`, `commit --amend`, `branch -D`, etc.

use std::io::Write;
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
    "ls-tree",
    "merge-base",
    "worktree",
    "hash-object",
    "read-tree",
    "update-index",
    "write-tree",
    "commit-tree",
    "merge",
    "branch",
];

/// Flags refused on any subcommand because they rewrite or discard history.
pub const FORBIDDEN_FLAGS: &[&str] = &[
    "--amend", "--force", "-f", "--fixup", "--squash", "-D", "-M", "-C", "--hard",
];

/// Identity and timestamp stamped onto a commit.
#[derive(Debug, Clone)]
pub struct Stamp {
    pub name: String,
    pub email: String,
    /// RFC 3339; used for both author and committer dates.
    pub date: String,
}

/// Per-call options.
#[derive(Debug, Default, Clone)]
pub struct Opts<'a> {
    pub stamp: Option<&'a Stamp>,
    pub stdin: Option<&'a [u8]>,
    pub env: Vec<(&'a str, String)>,
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

    pub fn env(&self) -> &[(String, String)] {
        &self.extra_env
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Same environment, different working directory.
    pub fn at(&self, dir: impl Into<PathBuf>) -> Git {
        Git {
            dir: dir.into(),
            extra_env: self.extra_env.clone(),
        }
    }

    /// Enforce the allowlist. Only flags before `--` are checked, so paths
    /// and values can never trip it.
    pub fn check(args: &[&str]) -> Result<()> {
        let sub = args.first().copied().unwrap_or("");
        let refuse = |what: String| Err(Error::ForbiddenGit(what));
        if !ALLOWED.contains(&sub) {
            return refuse(sub.to_string());
        }
        let flags: Vec<&str> = args[1..]
            .iter()
            .copied()
            .take_while(|a| *a != "--")
            .collect();
        if let Some(flag) = flags.iter().find(|a| FORBIDDEN_FLAGS.contains(a)) {
            return refuse(format!("{sub} {flag}"));
        }
        match sub {
            "merge" if !flags.contains(&"--ff-only") => refuse("merge without --ff-only".into()),
            "branch" if flags.iter().any(|f| f.starts_with('-') && *f != "-d") => {
                refuse(format!("branch {}", flags.join(" ")))
            }
            "worktree"
                if !matches!(
                    flags.first(),
                    Some(&"add") | Some(&"remove") | Some(&"list")
                ) =>
            {
                refuse(format!("worktree {}", flags.join(" ")))
            }
            _ => Ok(()),
        }
    }

    fn command(&self, args: &[&str], opts: &Opts) -> Result<Command> {
        Self::check(args)?;
        let mut cmd = Command::new("git");
        cmd.arg("-C").arg(&self.dir);
        // Never let a pager or editor block us.
        cmd.env("GIT_PAGER", "cat").env("GIT_EDITOR", "true");
        for (k, v) in &self.extra_env {
            cmd.env(k, v);
        }
        for (k, v) in &opts.env {
            cmd.env(k, v);
        }
        if let Some(s) = opts.stamp {
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

    /// Run git with options, return stdout on success.
    pub fn exec(&self, args: &[&str], opts: Opts) -> Result<String> {
        let mut child = self
            .command(args, &opts)?
            .stdin(if opts.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(input) = opts.stdin {
            child.stdin.take().expect("piped").write_all(input)?;
        }
        let out = child.wait_with_output()?;
        if !out.status.success() {
            return Err(Error::Git {
                args: args.join(" "),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// Run git, return stdout on success.
    pub fn run(&self, args: &[&str]) -> Result<String> {
        self.exec(args, Opts::default())
    }

    /// Run git and report only whether it exited 0 (for existence probes).
    pub fn probe(&self, args: &[&str]) -> Result<bool> {
        let status = self
            .command(args, &Opts::default())?
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        Ok(status.success())
    }

    /// Make a commit of exactly `paths` (`commit --only`), message on stdin.
    pub fn commit_only(&self, paths: &[&str], msg: &str, stamp: &Stamp) -> Result<()> {
        let mut args = vec!["commit", "--quiet", "--only", "-F", "-", "--"];
        args.extend_from_slice(paths);
        self.exec(
            &args,
            Opts {
                stamp: Some(stamp),
                stdin: Some(msg.as_bytes()),
                ..Default::default()
            },
        )?;
        Ok(())
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
            &["branch", "-D", "shard/x"],
            &["branch", "-m", "main", "old"],
            &["checkout", "."],
            &["gc", "--prune=now"],
            &["reflog", "expire"],
            &["commit", "--amend", "-m", "x"],
            &["commit", "--fixup", "HEAD"],
            &["add", "--force", "x"],
            &["merge", "shard/x"],
            &["merge", "--no-ff", "shard/x"],
            &["worktree", "remove", "--force", "x"],
            &["worktree", "prune"],
        ] {
            assert!(
                matches!(g.run(args), Err(Error::ForbiddenGit(_))),
                "{args:?} must be refused"
            );
        }
    }

    #[test]
    fn safe_forms_pass_the_check() {
        for args in [
            &["merge", "--ff-only", "--quiet", "abc"][..],
            &["branch", "-d", "shard/x"],
            &["worktree", "add", "-b", "shard/x", "p", "main"],
            &["commit", "--only", "-F", "-", "--", "-f"],
        ] {
            assert!(Git::check(args).is_ok(), "{args:?}");
        }
    }
}
