// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! A shards memory on disk: open/init, and the read and write operations.
//!
//! # Commit message format
//!
//! Every commit the tool makes looks like this:
//!
//! ```text
//! supersede: devices/rotoforge-ctl/ip
//!
//! Shards-Op: supersede
//! Shards-Address: devices/rotoforge-ctl/ip
//! Shards-Author: shard-7f3a
//! Shards-Human: Raelyn
//! Shards-Why: moved to the new switch
//! ```
//!
//! The subject is `<op>: <address>` (`init: memory` for the first commit).
//! The rest are git trailers, so `git log --format='%(trailers:key=Shards-Why)'`
//! works. `Shards-Human` is omitted when no human is known. The git author and
//! committer are both set to the shard (`<author>` /
//! `<author>@shards.invalid`) and both dates to the fact's `updated` time.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{SecondsFormat, Utc};

use crate::address::Address;
use crate::error::{Error, Result};
use crate::fact::{Fact as FactFile, FrontMatter};
use crate::git::{Git, Stamp};

/// Who is writing, and why.
#[derive(Debug, Clone, Default)]
pub struct Provenance {
    /// Shard / session id. Required.
    pub author: String,
    /// The human at the keyboard, if any.
    pub human: Option<String>,
    /// Why this change is being made. Required for fact writes.
    pub why: String,
    /// Override the timestamp (RFC 3339). `None` = now. Used by the sample
    /// generator so that output is deterministic by seed.
    pub at: Option<String>,
}

impl Provenance {
    pub fn new(author: impl Into<String>, human: Option<String>, why: impl Into<String>) -> Self {
        Provenance {
            author: author.into(),
            human,
            why: why.into(),
            at: None,
        }
    }

    fn validate(&self, need_why: bool) -> Result<()> {
        if self.author.trim().is_empty() {
            return Err(Error::MissingAuthor);
        }
        if need_why && self.why.trim().is_empty() {
            return Err(Error::MissingWhy);
        }
        for (field, v) in [
            ("author", Some(self.author.as_str())),
            ("human", self.human.as_deref()),
            ("why", Some(self.why.as_str())),
        ] {
            if v.is_some_and(|v| v.contains('\n') || v.contains('\r')) {
                return Err(Error::Multiline { field });
            }
        }
        Ok(())
    }

    fn timestamp(&self) -> String {
        self.at
            .clone()
            .unwrap_or_else(|| Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true))
    }

    fn stamp(&self, date: &str) -> Stamp {
        let local: String = self
            .author
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        Stamp {
            name: self.author.clone(),
            email: format!("{local}@shards.invalid"),
            date: date.to_string(),
        }
    }

    fn message(&self, op: &str, subject: &str, address: Option<&Address>) -> String {
        let mut m = format!("{op}: {subject}\n\nShards-Op: {op}\n");
        if let Some(a) = address {
            m += &format!("Shards-Address: {a}\n");
        }
        m += &format!("Shards-Author: {}\n", self.author);
        if let Some(h) = self.human.as_deref().filter(|h| !h.is_empty()) {
            m += &format!("Shards-Human: {h}\n");
        }
        if !self.why.is_empty() {
            m += &format!("Shards-Why: {}\n", self.why);
        }
        m
    }
}

/// Options for [`Memory::set`].
#[derive(Debug, Clone, Default)]
pub struct SetOptions {
    /// Create the fact even though a fact with the same name already lives
    /// elsewhere in the same section.
    pub force_new_home: bool,
}

/// A fact as read from the memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fact {
    pub address: Address,
    /// Repo-relative path, e.g. `facts/devices/rotoforge-ctl/ip.md`.
    pub home: String,
    pub front: FrontMatter,
    pub value: String,
}

/// One version of a fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryEntry {
    pub commit: String,
    pub value: String,
    pub updated: String,
    pub author: String,
    pub human: Option<String>,
    pub why: String,
}

/// An open memory repository.
#[derive(Debug, Clone)]
pub struct Memory {
    root: PathBuf,
    git: Git,
}

const DIRS: &[&str] = &["facts", "soul", "people"];

const README_STUB: &str = "\
# Memory

This is a shards memory: a git repository of fact files, written only by the
`mem` tool.

- `facts/` — one file per fact; the path is the fact's only home.
- `soul/` — who the agent is. Changes require a human admin.
- `people/` — one profile per person.

Read with `mem get <address>`; change with `mem set` / `mem supersede`.
Never edit fact files by hand and never rewrite this repo's history.
";

impl Memory {
    /// Create a new memory at `dir` (which must not exist or be empty).
    pub fn init(dir: impl AsRef<Path>, prov: &Provenance) -> Result<Memory> {
        Self::init_with_env(dir, prov, Vec::new())
    }

    /// [`Memory::init`] with extra environment for git (see [`Git::with_env`]).
    pub fn init_with_env(
        dir: impl AsRef<Path>,
        prov: &Provenance,
        env: Vec<(String, String)>,
    ) -> Result<Memory> {
        prov.validate(false)?;
        let dir = dir.as_ref();
        if dir.exists() && fs::read_dir(dir)?.next().is_some() {
            return Err(Error::DirNotEmpty(dir.to_path_buf()));
        }
        fs::create_dir_all(dir)?;
        let git = Git::new(dir).with_env(env);
        git.run(&["init", "--quiet", "--initial-branch=main"])?;
        let mut paths = vec!["README.md".to_string()];
        fs::write(dir.join("README.md"), README_STUB)?;
        for d in DIRS {
            fs::create_dir_all(dir.join(d))?;
            fs::write(dir.join(d).join(".gitkeep"), "")?;
            paths.push(format!("{d}/.gitkeep"));
        }
        let mut add = vec!["add", "--"];
        add.extend(paths.iter().map(String::as_str));
        git.run(&add)?;
        let date = prov.timestamp();
        let msg = prov.message("init", "memory", None);
        git.run_stamped(&["commit", "--quiet", "-m", &msg], Some(&prov.stamp(&date)))?;
        Ok(Memory {
            root: dir.to_path_buf(),
            git,
        })
    }

    /// Open an existing memory.
    pub fn open(dir: impl AsRef<Path>) -> Result<Memory> {
        Self::open_with_env(dir, Vec::new())
    }

    pub fn open_with_env(dir: impl AsRef<Path>, env: Vec<(String, String)>) -> Result<Memory> {
        let dir = dir.as_ref();
        let probe = Git::new(dir).with_env(env.clone());
        if !dir.join("facts").is_dir() || !probe.probe(&["rev-parse", "--git-dir"])? {
            return Err(Error::NotAMemory(dir.to_path_buf()));
        }
        let top = probe.run(&["rev-parse", "--show-toplevel"])?;
        let root = PathBuf::from(top.trim());
        Ok(Memory {
            git: Git::new(&root).with_env(env),
            root,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Current HEAD commit, if any.
    pub fn head(&self) -> Result<Option<String>> {
        if !self
            .git
            .probe(&["rev-parse", "--verify", "--quiet", "HEAD"])?
        {
            return Ok(None);
        }
        Ok(Some(
            self.git.run(&["rev-parse", "HEAD"])?.trim().to_string(),
        ))
    }

    /// Number of commits reachable from HEAD.
    pub fn commit_count(&self) -> Result<usize> {
        if self.head()?.is_none() {
            return Ok(0);
        }
        let n = self.git.run(&["rev-list", "--count", "HEAD"])?;
        Ok(n.trim().parse().unwrap_or(0))
    }

    /// Paths changed by one commit.
    pub fn changed_paths(&self, commit: &str) -> Result<Vec<String>> {
        let out = self.git.run(&[
            "diff-tree",
            "--root",
            "--no-commit-id",
            "--name-only",
            "-r",
            commit,
        ])?;
        Ok(out.lines().map(String::from).collect())
    }

    /// Every fact address committed on HEAD, sorted.
    pub fn addresses(&self) -> Result<Vec<Address>> {
        let out = self.git.run(&["ls-files", "--", "facts/"])?;
        let mut v: Vec<Address> = out.lines().filter_map(Address::from_home).collect();
        v.sort();
        Ok(v)
    }

    fn read_at(&self, rev: &str, address: &Address) -> Result<Option<FactFile>> {
        let spec = format!("{rev}:{}", address.home());
        if !self.git.probe(&["cat-file", "-e", &spec])? {
            return Ok(None);
        }
        let text = self.git.run(&["show", &spec])?;
        Ok(Some(FactFile::parse(&text, &address.home())?))
    }

    /// Read the current (committed) value of a fact.
    pub fn get(&self, address: &Address) -> Result<Fact> {
        let f = self
            .read_at("HEAD", address)?
            .ok_or_else(|| Error::UnknownAddress(address.to_string()))?;
        Ok(Fact {
            address: address.clone(),
            home: address.home(),
            front: f.front,
            value: f.value,
        })
    }

    /// Does a fact exist at this address?
    pub fn exists(&self, address: &Address) -> Result<bool> {
        Ok(self.read_at("HEAD", address)?.is_some())
    }

    /// Other facts in the same section with the same last segment.
    pub fn likely_other_homes(&self, address: &Address) -> Result<Vec<Address>> {
        Ok(self
            .addresses()?
            .into_iter()
            .filter(|a| {
                a != address && a.section() == address.section() && a.name() == address.name()
            })
            .collect())
    }

    /// Create a new fact. Returns the commit id.
    pub fn set(
        &self,
        address: &Address,
        value: &str,
        prov: &Provenance,
        opts: &SetOptions,
    ) -> Result<String> {
        prov.validate(true)?;
        check_value(value)?;
        if self.exists(address)? {
            return Err(Error::AlreadyExists {
                address: address.to_string(),
                home: address.home(),
            });
        }
        let others = self.likely_other_homes(address)?;
        if !others.is_empty() && !opts.force_new_home {
            return Err(Error::LikelySecondHome {
                address: address.to_string(),
                section: address.section().to_string(),
                others: others.iter().map(Address::home).collect(),
            });
        }
        self.check_path_free(address)?;
        self.write_and_commit("set", address, value, prov)
    }

    /// Replace the value of an existing fact. Returns the commit id.
    pub fn supersede(&self, address: &Address, value: &str, prov: &Provenance) -> Result<String> {
        prov.validate(true)?;
        check_value(value)?;
        let current = self.get(address)?;
        if current.value == value {
            return Err(Error::Unchanged(address.to_string()));
        }
        self.write_and_commit("supersede", address, value, prov)
    }

    /// Every version of a fact, newest first.
    pub fn history(&self, address: &Address) -> Result<Vec<HistoryEntry>> {
        if !self.exists(address)? {
            return Err(Error::UnknownAddress(address.to_string()));
        }
        let home = address.home();
        let log = self.git.run(&["log", "--format=%H", "HEAD", "--", &home])?;
        let mut out = Vec::new();
        for commit in log.lines() {
            if let Some(f) = self.read_at(commit, address)? {
                out.push(HistoryEntry {
                    commit: commit.to_string(),
                    value: f.value,
                    updated: f.front.updated,
                    author: f.front.author,
                    human: f.front.human,
                    why: f.front.why,
                });
            }
        }
        Ok(out)
    }

    /// Write a markdown note (not a fact) under `notes/` as one commit.
    /// Notes refer to facts with pointers rather than holding values.
    pub fn put_note(&self, rel_path: &str, text: &str, prov: &Provenance) -> Result<String> {
        prov.validate(true)?;
        let ok = rel_path.starts_with("notes/")
            && rel_path.ends_with(".md")
            && rel_path
                .split('/')
                .all(|seg| !seg.is_empty() && seg != "." && seg != "..");
        if !ok {
            return Err(Error::PathConflict {
                address: rel_path.to_string(),
                reason: "notes must be `notes/<...>.md`".to_string(),
            });
        }
        self.write_and_commit_raw("note", rel_path, None, text, prov)
    }

    fn ensure_on_main(&self) -> Result<()> {
        let branch = self.git.run(&["symbolic-ref", "--short", "HEAD"])?;
        let branch = branch.trim();
        if branch != "main" {
            return Err(Error::NotOnMain(branch.to_string()));
        }
        Ok(())
    }

    /// A new fact's file (and its parent directories) must not collide with
    /// an existing fact file used as a directory, or vice versa.
    fn check_path_free(&self, address: &Address) -> Result<()> {
        let home = address.home();
        let dir_form = format!("facts/{}/", address.as_str());
        for a in self.addresses()? {
            let other = a.home();
            if other.starts_with(&dir_form) {
                return Err(Error::PathConflict {
                    address: address.to_string(),
                    reason: format!("`{}` is a folder of facts (e.g. {other})", address),
                });
            }
            let other_dir = format!("facts/{}/", a.as_str());
            if home.starts_with(&other_dir) {
                return Err(Error::PathConflict {
                    address: address.to_string(),
                    reason: format!("`{a}` is already a fact, so it can't be a folder"),
                });
            }
        }
        let abs = self.root.join(&home);
        if abs.exists() {
            return Err(Error::Dirty(home));
        }
        Ok(())
    }

    fn write_and_commit(
        &self,
        op: &str,
        address: &Address,
        value: &str,
        prov: &Provenance,
    ) -> Result<String> {
        let home = address.home();
        let updated = prov.timestamp();
        let file = FactFile {
            front: FrontMatter {
                address: address.to_string(),
                updated: updated.clone(),
                author: prov.author.clone(),
                human: prov.human.clone().filter(|h| !h.is_empty()),
                why: prov.why.clone(),
            },
            value: value.to_string(),
        };
        let mut p = prov.clone();
        p.at = Some(updated);
        self.write_and_commit_raw(op, &home, Some(address), &file.render(), &p)
    }

    /// Write `contents` to `rel_path` and commit exactly that one path.
    fn write_and_commit_raw(
        &self,
        op: &str,
        rel_path: &str,
        address: Option<&Address>,
        contents: &str,
        prov: &Provenance,
    ) -> Result<String> {
        self.ensure_on_main()?;
        let status = self.git.run(&["status", "--porcelain", "--", rel_path])?;
        if !status.trim().is_empty() {
            return Err(Error::Dirty(rel_path.to_string()));
        }
        let date = prov.timestamp();
        let abs = self.root.join(rel_path);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&abs, contents)?;
        let subject = address.map_or(rel_path.to_string(), |a| a.to_string());
        let msg = prov.message(op, &subject, address);
        self.git.run(&["add", "--", rel_path])?;
        // `--only` with a pathspec: commit exactly this file, whatever else
        // may be staged. This is how "touch nothing else" is enforced.
        self.git.run_stamped(
            &["commit", "--quiet", "--only", "-m", &msg, "--", rel_path],
            Some(&prov.stamp(&date)),
        )?;
        Ok(self.head()?.expect("just committed"))
    }
}

fn check_value(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(Error::EmptyValue);
    }
    Ok(())
}
