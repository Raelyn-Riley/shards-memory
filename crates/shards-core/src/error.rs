// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

use std::path::PathBuf;

/// Everything that can go wrong in `shards-core`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid address `{address}`: {reason}")]
    InvalidAddress { address: String, reason: String },

    #[error("no fact at address `{0}`")]
    UnknownAddress(String),

    #[error("fact `{address}` already exists at {home}; use `mem supersede` to change it")]
    AlreadyExists { address: String, home: String },

    #[error("cannot create `{address}`: {reason}")]
    PathConflict { address: String, reason: String },

    #[error("new value for `{0}` is identical to the current value; nothing to supersede")]
    Unchanged(String),

    #[error("the value must not be empty")]
    EmptyValue,

    #[error("--why is required and must not be empty")]
    MissingWhy,

    #[error("no author: pass --author or set SHARDS_AUTHOR")]
    MissingAuthor,

    #[error("{field} must not contain a newline")]
    Multiline { field: &'static str },

    #[error("{0} has uncommitted changes; refusing to write over them")]
    Dirty(String),

    #[error(
        "this worktree is on branch `{0}`; writes go to `main` or to your own `shard/<id>` branch"
    )]
    NotOnMain(String),

    #[error("this is shard `{shard}`'s worktree; author `{author}` can't write here (leave --author/SHARDS_AUTHOR unset or use `{shard}`)")]
    NotYourShard { shard: String, author: String },

    #[error("no open shard `{0}`")]
    UnknownShard(String),

    #[error("shard `{0}` already exists (open, or its branch shard/{0} is kept); pick another id")]
    ShardExists(String),

    #[error(
        "invalid shard id `{0}`: use a-z, 0-9, `-`, `_`, `.`, starting with a letter or digit"
    )]
    BadShardId(String),

    #[error("{what} requires a human admin; `{human}` is not in [soul] admins in shards.toml")]
    NotAdmin { what: String, human: String },

    #[error("shard `{0}` has no pending soul proposal")]
    NothingToApprove(String),

    #[error("no open conflict for `{0}`")]
    NoConflict(String),

    #[error("`{target}` has conflicts from several shards ({}); pass --shard", shards.join(", "))]
    AmbiguousConflict { target: String, shards: Vec<String> },

    #[error("`{path}` can't be written with `mem put`: {reason}")]
    BadPath { path: String, reason: String },

    #[error("no quirk `{0}` (ids look like q3; see `mem quirk list --all`)")]
    UnknownQuirk(String),

    #[error("quirk: {0}")]
    BadQuirk(String),

    #[error("lock `{name}` is busy: {msg}")]
    Locked { name: String, msg: String },

    #[error("gave up waiting for lock `{name}` after {waited:.1}s: {msg}")]
    LockTimeout {
        name: String,
        msg: String,
        waited: f64,
    },

    #[error(
        "lost lock `{0}` while working (lease expired and it was recovered); nothing was committed"
    )]
    LockLost(String),

    #[error("core: {0}")]
    Core(String),

    #[error("shard session: {0}")]
    Session(String),

    #[error("bad shards.toml: {0}")]
    Config(String),

    #[error("{0} is not a shards memory (no git repo with a facts/ directory)")]
    NotAMemory(PathBuf),

    #[error("{0} already exists and is not empty")]
    DirNotEmpty(PathBuf),

    #[error("malformed fact file {path}: {reason}")]
    Malformed { path: String, reason: String },

    #[error("unresolvable pointer(s): {}", pointers.iter().map(|p| format!("{{{{fact:{p}}}}}")).collect::<Vec<_>>().join(", "))]
    Unresolvable { pointers: Vec<String> },

    #[error("git subcommand `{0}` is not allowed by shards (history is append-only)")]
    ForbiddenGit(String),

    #[error("git {args} failed: {stderr}")]
    Git { args: String, stderr: String },

    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
