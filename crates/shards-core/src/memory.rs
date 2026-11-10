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
    /// The open question this change answers (`ask2`), from a shard. It
    /// rides the commit as `Shards-Answers` and lands in the proposal.
    pub answers: Option<String>,
}

impl Provenance {
    pub fn new(author: impl Into<String>, human: Option<String>, why: impl Into<String>) -> Self {
        Provenance {
            author: author.into(),
            human,
            why: why.into(),
            at: None,
            answers: None,
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
        if let Some(a) = &self.answers {
            m += &format!("Shards-Answers: {a}\n");
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
    /// Set when the version was accepted from a shard's proposal.
    pub review: Option<Review>,
}

/// How a version accepted from a proposal was reviewed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    /// `p3#2`: the proposal and the change number.
    pub proposal: String,
    pub by: String,
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
- `proposals/` — shards' fact and people changes, and every decision on them.
- `questions/` — held changes, as questions for the next person who can answer.
- `incidents/` — conflicts a person settled: both sides, who, why, and notes.
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
  in flight, overlaps, open conflicts, soul proposals, proposals, open
  questions, recent incidents, and the events that are waiting for you.
- **Reason.** Decide what matters: two people changing the same fact, a
  shard gone quiet with work in flight, a conflict or proposal waiting for a
  human.
- **Review proposals.** A shard's fact and people changes don't reach main
  at dinit; they wait in a proposal for you. `mem proposal show <id>`
  lists each change with its value and the shard's reason. Decide each one
  with `mem proposal accept|reject|hold <id> --change N --why \"...\"`.
  To fix a value, accept it with `--value <fixed>`. Hold a change that
  only a person could check, and say what to ask with
  `--ask \"<question>\"`: the next person who can answer gets asked by
  their shard. An answer comes back as a proposal citing the question
  (`answers: ask<n>`); accepting it closes the question and the held change.
  Every change is reviewed; nothing is accepted for you.
- **Read incidents.** When a person settles a conflict, their pick is final
  and goes straight to main; an incident records both sides, who settled it
  and why. `mem incident show <id>` reads it. If you have something to add,
  `mem incident note <id> \"...\" --why \"...\"`. Nothing in an incident is
  ever removed.
- **Check first.** This whole system is checking: what's in the memory,
  and, where you can, the thing itself. On every conflict, every answer to
  a question and every held change, if there is a harmless check
  (read-only: ping it, read a config or label file, ask a device what it
  reports; never anything that writes, moves or actuates), run it before
  you decide or ask a person. Write the check and its result into your
  `--why` (or the incident's note). If the check disagrees with an answer,
  reject the answer with the check's result: the question stays open, and
  the next person asked sees that result.
- **Superseded is fixed.** When newer information about a fact reaches
  main (a change you accept, a conflict a person settles, a write on main),
  any held change on that fact becomes `fixed` and its open question closes
  as `fixed`, linked to what fixed it. You don't need to close them by hand.
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
        self.set_described(address, value, None, prov)
    }

    /// [`Memory::set`] with a one-line description of what the fact is
    /// (for `mem index`; never the value).
    pub fn set_described(
        &self,
        address: &Address,
        value: &str,
        description: Option<&str>,
        prov: &Provenance,
    ) -> Result<String> {
        check_value(value)?;
        let description = check_description(description, value)?;
        let description = description.as_deref();
        let prov = self.effective(prov, true)?;
        self.check_answers(&prov, address.as_str())?;
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
                let mut w = vec![(
                    address.home(),
                    Some(fact_file(address, value, description, prov)),
                )];
                m.close_superseded(
                    &mut w,
                    &[address.home()],
                    &main_write("set", address.as_str(), prov),
                    prov,
                )?;
                Ok(w)
            },
        )
    }

    /// Replace the value of an existing fact. Returns the commit id.
    pub fn supersede(&self, address: &Address, value: &str, prov: &Provenance) -> Result<String> {
        self.supersede_described(address, value, None, prov)
    }

    /// [`Memory::supersede`]; the fact keeps its description unless a new
    /// one is given.
    pub fn supersede_described(
        &self,
        address: &Address,
        value: &str,
        description: Option<&str>,
        prov: &Provenance,
    ) -> Result<String> {
        check_value(value)?;
        let given = check_description(description, value)?;
        let prov = self.effective(prov, true)?;
        self.check_answers(&prov, address.as_str())?;
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
                let description = given.clone().or(current.front.description);
                check_description(description.as_deref(), value)?;
                let mut w = vec![(
                    address.home(),
                    Some(fact_file(address, value, description.as_deref(), prov)),
                )];
                m.close_superseded(
                    &mut w,
                    &[address.home()],
                    &main_write("supersede", address.as_str(), prov),
                    prov,
                )?;
                Ok(w)
            },
        )
    }

    /// Change only a fact's description, on main (the core's to edit; a
    /// shard gives one with its `set`, and the core reviews it). The value
    /// and its provenance stay as they are; the commit says who changed the
    /// description and why.
    pub fn describe(
        &self,
        address: &Address,
        description: &str,
        prov: &Provenance,
    ) -> Result<String> {
        if let Context::Shard(s) = &self.context {
            return Err(Error::Description(format!(
                "shard `{s}` can't edit descriptions: the core does, on main. Give one with \
                 `mem set ... --description \"...\"` when you create a fact"
            )));
        }
        let prov = self.effective(prov, true)?;
        self.locked_commit(
            "describe",
            address.as_str(),
            &[address.home()],
            &prov,
            &[("Address", address.to_string())],
            |m, _| {
                let mut f = m
                    .read_at("HEAD", address)?
                    .ok_or_else(|| Error::UnknownAddress(address.to_string()))?;
                let d = check_description(Some(description), &f.value)?;
                if f.front.description == d {
                    return Err(Error::Unchanged(address.to_string()));
                }
                f.front.description = d;
                Ok(vec![(address.home(), Some(f.render()))])
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
        self.check_answers(&prov, rel_path)?;
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
            |m, prov| {
                if m.read_file_at("HEAD", &path)?.as_deref() == Some(text) {
                    return Err(Error::Unchanged(path.clone()));
                }
                let mut w = vec![(path.clone(), Some(text.to_string()))];
                m.close_superseded(
                    &mut w,
                    std::slice::from_ref(&path),
                    &main_write("put", &path, prov),
                    prov,
                )?;
                Ok(w)
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
                // A description-only edit (`mem describe`) is not a new
                // version of the value: list the version once, at the
                // commit that wrote the value.
                if let Some(e) = out.last_mut().filter(|e: &&mut HistoryEntry| {
                    e.value == f.value && e.updated == f.front.updated
                }) {
                    e.commit = commit.to_string();
                    continue;
                }
                out.push(HistoryEntry {
                    commit: commit.to_string(),
                    value: f.value,
                    updated: f.front.updated,
                    author: f.front.author,
                    human: f.front.human,
                    why: f.front.why,
                    review: f.front.proposal.map(|proposal| Review {
                        proposal,
                        by: f.front.reviewed_by.unwrap_or_default(),
                        why: f.front.review_why.unwrap_or_default(),
                    }),
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
        if p.answers.as_deref().is_some_and(|a| a.trim().is_empty()) {
            p.answers = None;
        }
        Ok(p)
    }

    /// A write citing an open question (`--answers`): only from a shard, so
    /// the answer becomes a proposal the core reviews; the question must be
    /// open, about `target`, and answerable by this person.
    fn check_answers(&self, prov: &Provenance, target: &str) -> Result<()> {
        let Some(q) = &prov.answers else {
            return Ok(());
        };
        if !matches!(self.context, Context::Shard(_)) {
            return Err(Error::BadQuestion(format!(
                "an answer to {q} goes through a shard, so the core reviews it; run it from a shard"
            )));
        }
        self.check_answer(q, target, prov.human.as_deref())
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

/// What fixed a held change when a write on main did: `supersede
/// devices/ctl/ip on main by core`.
pub(crate) fn main_write(op: &str, target: &str, prov: &Provenance) -> String {
    match &prov.human {
        Some(h) => format!("{op} {target} on main by {} / {h}", prov.author),
        None => format!("{op} {target} on main by {}", prov.author),
    }
}

/// Render a fact file for `address` with `prov`'s provenance.
pub(crate) fn fact_file(
    address: &Address,
    value: &str,
    description: Option<&str>,
    prov: &Provenance,
) -> String {
    FactFile {
        front: FrontMatter {
            address: address.to_string(),
            description: description.map(String::from),
            updated: prov.timestamp(),
            author: prov.author.clone(),
            human: prov.human.clone().filter(|h| !h.is_empty()),
            why: prov.why.clone(),
            ..Default::default()
        },
        value: value.to_string(),
    }
    .render()
}

/// A description is one line saying what the fact is. It must never carry
/// the value, so the index can't become a second home for it.
pub(crate) fn check_description(d: Option<&str>, value: &str) -> Result<Option<String>> {
    let Some(d) = d.map(str::trim).filter(|d| !d.is_empty()) else {
        return Ok(None);
    };
    if d.contains('\n') || d.contains('\r') {
        return Err(Error::Multiline {
            field: "description",
        });
    }
    let v = value.trim();
    if v.chars().count() >= 3 && d.to_lowercase().contains(&v.to_lowercase()) {
        return Err(Error::Description(format!(
            "the description contains the value ({v}); say what the fact is, not what it is \
             set to, so the index never holds a copy of a value"
        )));
    }
    Ok(Some(d.to_string()))
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
