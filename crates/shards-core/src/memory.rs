// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! A shards memory on disk: open/init, and the read and write operations.
//!
//! A `Memory` is opened on one *worktree*: either the main worktree (branch
//! `main`) or a shard's worktree (branch `shard/<id>`). Reads come from that
//! worktree's HEAD; writes commit to that branch. Writes on main take the
//! main lock(s); writes on a shard take that shard's own lock.
//!
//! # Commit message format
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
//! The subject is `<op>: <address or path>`; the rest are git trailers.
//! `Shards-Human` is omitted when no human is known. Some ops add trailers
//! (see the README). The git author and committer are both the shard
//! (`<author>` / `<author>@shards.invalid`).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{SecondsFormat, Utc};

use crate::address::Address;
use crate::config::{self, Config, Scope};
use crate::error::{Error, Result};
use crate::fact::{Fact as FactFile, FrontMatter};
use crate::git::{Git, Stamp};
use crate::lock::{stderr_notifier, FileMutex, LockGuard, Locks, Notifier};

/// Who is writing, and why.
#[derive(Debug, Clone, Default)]
pub struct Provenance {
    /// Shard / session id. Required on main; defaults to the shard id in a
    /// shard worktree.
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

    pub(crate) fn validate(&self, need_why: bool) -> Result<()> {
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

    pub(crate) fn timestamp(&self) -> String {
        self.at.clone().unwrap_or_else(now)
    }

    pub(crate) fn stamp(&self, date: &str) -> Stamp {
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

    pub(crate) fn message(&self, op: &str, subject: &str, trailers: &[(&str, String)]) -> String {
        let mut m = format!("{op}: {subject}\n\nShards-Op: {op}\n");
        for (k, v) in trailers {
            m += &format!("Shards-{k}: {v}\n");
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

pub(crate) fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
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

/// Which branch this worktree is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Context {
    Main,
    Shard(String),
    /// Any other branch (or detached HEAD): read-only.
    Other(String),
}

/// One file write inside a commit: `Some(contents)` writes, `None` deletes.
pub(crate) type FileWrite = (String, Option<String>);

/// An open memory worktree.
#[derive(Clone)]
pub struct Memory {
    pub(crate) root: PathBuf,
    pub(crate) main_root: PathBuf,
    pub(crate) git: Git,
    pub(crate) config: Config,
    pub(crate) notify: Notifier,
    pub(crate) context: Context,
}

impl std::fmt::Debug for Memory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memory")
            .field("root", &self.root)
            .field("main_root", &self.main_root)
            .field("context", &self.context)
            .finish()
    }
}

const DIRS: &[&str] = &["facts", "soul", "people"];

const README_STUB: &str = "\
# Memory

This is a shards memory: a git repository of fact files, written only by the
`mem` tool.

- `facts/` — one file per fact; the path is the fact's only home.
- `soul/` — who the agent is. Changes require a human admin. `soul/core.md`
  says what the core is.
- `people/` — one profile per person.
- `conflicts/` — open conflicts from dinits, waiting for a human.
- `shards.toml` — lock, soul-admin, shard, awareness and core settings.

Read with `mem get <address>`; change with `mem set` / `mem supersede`.
Never edit fact files by hand and never rewrite this repo's history.
";

/// `soul/core.md`, written by `mem init`: what the core is, in plain words.
pub const SOUL_CORE: &str = "\
# The core

You are the core: the one long-lived session that looks after main, the
shared memory. Shards are the sessions people work in. You don't wait for
them to report; you read what they are doing.

What you do:

- **Read the board.** `mem core context` shows every open shard, what it has
  in flight, overlaps, open conflicts, soul proposals, and the events that
  are waiting for you.
- **Reason.** Decide what matters: two people changing the same fact, a
  shard gone quiet with work in flight, a conflict or proposal waiting for a
  human.
- **Tell people.** Say who should talk to whom, and why, in a sentence.
- **Write decisions to main.** What you decide and learn goes into the
  memory with `mem set`, `mem supersede` or `mem put`, with a clear `--why`.
  Your chat history is not the memory; main is.
- **Acknowledge.** When you have handled a wake, run
  `mem core ack --upto <n>` with the number from the `[WAKE]` line.
- **Hand off before you fill up.** When this session gets long, write a
  short baton (what you are in the middle of, what to watch, where in main
  things are) and run `mem core rinit --baton <file>`. A fresh session
  starts with your baton as its first message.

Messages you will see:

- `[WAKE] 3 events, up to #17: run mem core context` means events are
  pending. Read the context, act, then ack.
- A message that is exactly `[HH:MM]`, such as `[14:05]`, is a heartbeat that
  keeps your cache warm. Reply with one character and do nothing else.
- Anything else is a person who attached to your pane to talk to you.
";

const GITIGNORE: &str = "# Local shard state: worktrees, locks, status lines.\n/.shards/\n";

/// Directory (relative to the main root) for local, untracked shard state.
pub const STATE_DIR: &str = ".shards";

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
        let git = Git::new(dir).with_env(env.clone());
        git.run(&["init", "--quiet", "--initial-branch=main"])?;
        let mut files = vec![
            ("README.md".to_string(), README_STUB),
            (".gitignore".to_string(), GITIGNORE),
            (config::FILE.to_string(), config::DEFAULT_TOML),
        ];
        for d in DIRS {
            fs::create_dir_all(dir.join(d))?;
            files.push((format!("{d}/.gitkeep"), ""));
        }
        files.push(("soul/core.md".to_string(), SOUL_CORE));
        let mut add = vec!["add", "--"];
        for (p, text) in &files {
            fs::write(dir.join(p), text)?;
            add.push(p);
        }
        git.run(&add)?;
        let date = prov.timestamp();
        let msg = prov.message("init", "memory", &[]);
        git.exec(
            &["commit", "--quiet", "-F", "-"],
            crate::git::Opts {
                stamp: Some(&prov.stamp(&date)),
                stdin: Some(msg.as_bytes()),
                ..Default::default()
            },
        )?;
        Self::open_with_env(dir, env)
    }

    /// Open an existing memory (the main worktree or a shard worktree).
    pub fn open(dir: impl AsRef<Path>) -> Result<Memory> {
        Self::open_with_env(dir, Vec::new())
    }

    pub fn open_with_env(dir: impl AsRef<Path>, env: Vec<(String, String)>) -> Result<Memory> {
        let dir = dir.as_ref();
        let probe = Git::new(dir).with_env(env.clone());
        if !dir.join("facts").is_dir() || !probe.probe(&["rev-parse", "--git-dir"])? {
            return Err(Error::NotAMemory(dir.to_path_buf()));
        }
        let root = PathBuf::from(probe.run(&["rev-parse", "--show-toplevel"])?.trim());
        let common = probe.run(&["rev-parse", "--path-format=absolute", "--git-common-dir"])?;
        let common = PathBuf::from(common.trim());
        let main_root = common
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| Error::NotAMemory(dir.to_path_buf()))?;
        let git = Git::new(&root).with_env(env);
        let branch = git
            .run(&["symbolic-ref", "--quiet", "--short", "HEAD"])
            .map(|b| b.trim().to_string())
            .unwrap_or_else(|_| "(detached HEAD)".to_string());
        let context = if branch == "main" {
            Context::Main
        } else if let Some(id) = branch.strip_prefix("shard/") {
            Context::Shard(id.to_string())
        } else {
            Context::Other(branch)
        };
        let config = Config::load(&main_root)?;
        Ok(Memory {
            root,
            main_root,
            git,
            config,
            notify: stderr_notifier(),
            context,
        })
    }

    /// Where blocked-writer and lock-recovery messages go (default: stderr).
    pub fn with_notifier(mut self, notify: Notifier) -> Self {
        self.notify = notify;
        self
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The main worktree's root (same as `root()` when on main).
    pub fn main_root(&self) -> &Path {
        &self.main_root
    }

    pub fn context(&self) -> &Context {
        &self.context
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The same memory, opened on its main worktree.
    pub fn main(&self) -> Result<Memory> {
        if self.context == Context::Main {
            return Ok(self.clone());
        }
        let mut m = Memory::open_with_env(&self.main_root, self.git.env().to_vec())?;
        m.notify = self.notify.clone();
        match &m.context {
            Context::Main => {}
            Context::Other(b) => return Err(Error::NotOnMain(b.clone())),
            Context::Shard(id) => return Err(Error::NotOnMain(format!("shard/{id}"))),
        }
        Ok(m)
    }

    pub(crate) fn state_dir(&self) -> PathBuf {
        self.main_root.join(STATE_DIR)
    }

    pub(crate) fn locks(&self) -> Locks {
        let state = self.state_dir();
        Locks::new(
            state.join("locks"),
            state.join("lock.log"),
            self.config.lock.clone(),
            self.notify.clone(),
        )
    }

    /// Short internal mutex around git index/HEAD updates. With section
    /// scope two writers may hold different section locks at once, but git
    /// has one HEAD for main; this keeps their commits from racing.
    pub(crate) fn git_mutex(&self) -> Result<FileMutex> {
        FileMutex::lock(&self.state_dir().join("locks").join("git.mutex"))
    }

    /// Lock names covering writes to `paths` on main, per the configured scope.
    pub fn main_lock_names(&self, paths: &[String]) -> Vec<String> {
        if self.config.lock.scope == Scope::Store {
            return vec!["main".to_string()];
        }
        let mut names: Vec<String> = paths
            .iter()
            .map(|p| {
                let mut parts = p.split('/');
                let top = parts.next().unwrap_or("");
                match top {
                    "facts" | "conflicts" => {
                        format!("main.{top}.{}", parts.next().unwrap_or("_"))
                    }
                    _ => format!("main.{top}"),
                }
            })
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// Take the main locks for `paths` (as `holder`). Test hook: if
    /// `SHARDS_DEBUG_HOLD_MS` is set, sleep that long after acquiring.
    pub fn lock_main(
        &self,
        paths: &[String],
        holder: &str,
        human: Option<&str>,
    ) -> Result<Vec<LockGuard>> {
        let guards = self
            .locks()
            .acquire_all(&self.main_lock_names(paths), holder, human)?;
        debug_hold();
        Ok(guards)
    }

    pub(crate) fn lock_shard(
        &self,
        id: &str,
        holder: &str,
        human: Option<&str>,
    ) -> Result<LockGuard> {
        let g = self
            .locks()
            .acquire(&format!("shard.{id}"), holder, human)?;
        debug_hold();
        Ok(g)
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

    /// Paths changed by one commit (against its first parent).
    pub fn changed_paths(&self, commit: &str) -> Result<Vec<String>> {
        let parent = format!("{commit}^1");
        let out = if self
            .git
            .probe(&["rev-parse", "--verify", "--quiet", &parent])?
        {
            self.git
                .run(&["diff-tree", "-r", "--name-only", &parent, commit])?
        } else {
            self.git.run(&[
                "diff-tree",
                "--root",
                "--no-commit-id",
                "--name-only",
                "-r",
                commit,
            ])?
        };
        Ok(out.lines().map(String::from).collect())
    }

    /// Every fact address committed on HEAD, sorted.
    pub fn addresses(&self) -> Result<Vec<Address>> {
        let out = self.git.run(&["ls-files", "--", "facts/"])?;
        let mut v: Vec<Address> = out.lines().filter_map(Address::from_home).collect();
        v.sort();
        Ok(v)
    }

    /// Raw contents of `path` at `rev`, if it exists there.
    pub fn read_file_at(&self, rev: &str, path: &str) -> Result<Option<String>> {
        let spec = format!("{rev}:{path}");
        if !self.git.probe(&["cat-file", "-e", &spec])? {
            return Ok(None);
        }
        Ok(Some(self.git.run(&["cat-file", "blob", &spec])?))
    }

    fn read_at(&self, rev: &str, address: &Address) -> Result<Option<FactFile>> {
        match self.read_file_at(rev, &address.home())? {
            Some(text) => Ok(Some(FactFile::parse(&text, &address.home())?)),
            None => Ok(None),
        }
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

    /// Create a new fact. Returns the commit id.
    ///
    /// Refuses only an exact existing address. Near-duplicates (the same name
    /// elsewhere) are left to a later drift checker, which flags for a human.
    pub fn set(&self, address: &Address, value: &str, prov: &Provenance) -> Result<String> {
        check_value(value)?;
        let prov = self.effective(prov, true)?;
        self.locked_commit(
            "set",
            address.as_str(),
            &[address.home()],
            &prov,
            &[("Address", address.to_string())],
            |m, prov| {
                if m.exists(address)? {
                    return Err(Error::AlreadyExists {
                        address: address.to_string(),
                        home: address.home(),
                    });
                }
                m.check_path_free(address)?;
                Ok(vec![(
                    address.home(),
                    Some(fact_file(address, value, prov)),
                )])
            },
        )
    }

    /// Replace the value of an existing fact. Returns the commit id.
    pub fn supersede(&self, address: &Address, value: &str, prov: &Provenance) -> Result<String> {
        check_value(value)?;
        let prov = self.effective(prov, true)?;
        self.locked_commit(
            "supersede",
            address.as_str(),
            &[address.home()],
            &prov,
            &[("Address", address.to_string())],
            |m, prov| {
                let current = m.get(address)?;
                if current.value == value {
                    return Err(Error::Unchanged(address.to_string()));
                }
                Ok(vec![(
                    address.home(),
                    Some(fact_file(address, value, prov)),
                )])
            },
        )
    }

    /// Write a markdown file under `notes/`, `people/` or `soul/` as one
    /// commit. On main, a `soul/` write requires `prov.human` to be an admin;
    /// on a shard it is a proposal that dinit will hold for an admin.
    pub fn put_file(&self, rel_path: &str, text: &str, prov: &Provenance) -> Result<String> {
        let bad = |reason: &str| Error::BadPath {
            path: rel_path.to_string(),
            reason: reason.to_string(),
        };
        let top = rel_path.split('/').next().unwrap_or("");
        if !["notes", "people", "soul"].contains(&top) {
            return Err(bad(
                "only notes/, people/ and soul/ files (facts use set/supersede)",
            ));
        }
        if !rel_path.ends_with(".md")
            || rel_path.split('/').count() < 2
            || rel_path
                .split('/')
                .any(|seg| seg.is_empty() || seg == "." || seg == ".." || seg.starts_with('.'))
        {
            return Err(bad(
                "must be <dir>/<...>.md with no empty, `.` or `..` segments",
            ));
        }
        let prov = self.effective(prov, true)?;
        if top == "soul" && self.context == Context::Main {
            self.require_admin(prov.human.as_deref(), "changing the soul on main")?;
        }
        let path = rel_path.to_string();
        self.locked_commit(
            "put",
            rel_path,
            std::slice::from_ref(&path),
            &prov,
            &[("Path", path.clone())],
            |m, _| {
                if m.read_file_at("HEAD", &path)?.as_deref() == Some(text) {
                    return Err(Error::Unchanged(path.clone()));
                }
                Ok(vec![(path.clone(), Some(text.to_string()))])
            },
        )
    }

    /// Kept for brief-01 callers: write a note under `notes/`.
    pub fn put_note(&self, rel_path: &str, text: &str, prov: &Provenance) -> Result<String> {
        if !rel_path.starts_with("notes/") {
            return Err(Error::BadPath {
                path: rel_path.to_string(),
                reason: "notes must be under notes/".to_string(),
            });
        }
        self.put_file(rel_path, text, prov)
    }

    /// Every version of a fact, newest first.
    pub fn history(&self, address: &Address) -> Result<Vec<HistoryEntry>> {
        if !self.exists(address)? {
            return Err(Error::UnknownAddress(address.to_string()));
        }
        let home = address.home();
        let log = self
            .git
            .run(&["log", "--topo-order", "--format=%H", "HEAD", "--", &home])?;
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

    pub(crate) fn require_admin(&self, human: Option<&str>, what: &str) -> Result<()> {
        let h = human.unwrap_or("");
        if h.is_empty() || !self.config.soul.admins.iter().any(|a| a == h) {
            return Err(Error::NotAdmin {
                what: what.to_string(),
                human: if h.is_empty() {
                    "(no human)".into()
                } else {
                    h.into()
                },
            });
        }
        Ok(())
    }

    /// Fill in and check provenance for a write in this worktree.
    pub(crate) fn effective(&self, prov: &Provenance, need_why: bool) -> Result<Provenance> {
        let mut p = prov.clone();
        match &self.context {
            Context::Main => {}
            Context::Shard(id) => {
                if p.author.trim().is_empty() {
                    p.author = id.clone();
                } else if p.author != *id {
                    return Err(Error::NotYourShard {
                        shard: id.clone(),
                        author: p.author,
                    });
                }
                if p.human.is_none() {
                    p.human = self.shard_meta(id).ok().and_then(|m| m.human);
                }
            }
            Context::Other(b) => return Err(Error::NotOnMain(b.clone())),
        }
        p.validate(need_why)?;
        Ok(p)
    }

    /// Take the right lock for this worktree, compute the writes with
    /// `prepare` (under the lock, so checks can't go stale), then write and
    /// commit exactly those paths.
    pub(crate) fn locked_commit(
        &self,
        op: &str,
        subject: &str,
        lock_paths: &[String],
        prov: &Provenance,
        trailers: &[(&str, String)],
        prepare: impl FnOnce(&Memory, &Provenance) -> Result<Vec<FileWrite>>,
    ) -> Result<String> {
        let guards = match &self.context {
            Context::Main => self.lock_main(lock_paths, &prov.author, prov.human.as_deref())?,
            Context::Shard(id) => {
                if self.shard_meta(id).is_err() {
                    return Err(Error::UnknownShard(id.clone()));
                }
                vec![self.lock_shard(id, &prov.author, prov.human.as_deref())?]
            }
            Context::Other(b) => return Err(Error::NotOnMain(b.clone())),
        };
        let _git = self.git_mutex()?;
        // Stamp the time once the lock is held, for the file and the commit.
        let mut prov = prov.clone();
        if prov.at.is_none() {
            prov.at = Some(now());
        }
        let writes = prepare(self, &prov)?;
        for g in &guards {
            g.ensure_held()?;
        }
        self.commit_files(op, subject, &writes, &prov, trailers)
    }

    /// Write files in this worktree and commit exactly those paths.
    pub(crate) fn commit_files(
        &self,
        op: &str,
        subject: &str,
        writes: &[FileWrite],
        prov: &Provenance,
        trailers: &[(&str, String)],
    ) -> Result<String> {
        // The branch may have changed since this Memory was opened.
        let expected = match &self.context {
            Context::Main => "main".to_string(),
            Context::Shard(id) => format!("shard/{id}"),
            Context::Other(b) => return Err(Error::NotOnMain(b.clone())),
        };
        let branch = self
            .git
            .run(&["symbolic-ref", "--quiet", "--short", "HEAD"])
            .map(|b| b.trim().to_string())
            .unwrap_or_else(|_| "(detached HEAD)".to_string());
        if branch != expected {
            return Err(Error::NotOnMain(branch));
        }
        let paths: Vec<&str> = writes.iter().map(|(p, _)| p.as_str()).collect();
        let mut status_args = vec!["status", "--porcelain", "--"];
        status_args.extend(&paths);
        let status = self.git.run(&status_args)?;
        if !status.trim().is_empty() {
            return Err(Error::Dirty(paths.join(", ")));
        }
        for (p, contents) in writes {
            let abs = self.root.join(p);
            match contents {
                Some(text) => {
                    if let Some(parent) = abs.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&abs, text)?;
                }
                None => {
                    if abs.exists() {
                        fs::remove_file(&abs)?;
                    }
                    // Prune now-empty parent folders (git doesn't track them).
                    let mut dir = abs.parent();
                    while let Some(d) = dir {
                        if d == self.root || fs::remove_dir(d).is_err() {
                            break;
                        }
                        dir = d.parent();
                    }
                }
            }
        }
        let mut add = vec!["add", "--"];
        add.extend(&paths);
        self.git.run(&add)?;
        let date = prov.timestamp();
        let msg = prov.message(op, subject, trailers);
        // `--only` with pathspecs: commit exactly these files, whatever else
        // may be staged. This is how "touch nothing else" is enforced.
        self.git.commit_only(&paths, &msg, &prov.stamp(&date))?;
        Ok(self.head()?.expect("just committed"))
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
                    reason: format!("`{address}` is a folder of facts (e.g. {other})"),
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
        if self.root.join(&home).exists() {
            return Err(Error::Dirty(home));
        }
        Ok(())
    }
}

/// Render a fact file for `address` with `prov`'s provenance.
pub(crate) fn fact_file(address: &Address, value: &str, prov: &Provenance) -> String {
    FactFile {
        front: FrontMatter {
            address: address.to_string(),
            updated: prov.timestamp(),
            author: prov.author.clone(),
            human: prov.human.clone().filter(|h| !h.is_empty()),
            why: prov.why.clone(),
        },
        value: value.to_string(),
    }
    .render()
}

fn check_value(value: &str) -> Result<()> {
    if value.trim().is_empty() {
        return Err(Error::EmptyValue);
    }
    Ok(())
}

/// Test hook: `SHARDS_DEBUG_HOLD_MS` makes every lock holder sleep after
/// acquiring, so tests can force contention deterministically.
fn debug_hold() {
    if let Some(ms) = std::env::var("SHARDS_DEBUG_HOLD_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        std::thread::sleep(Duration::from_millis(ms));
    }
}
