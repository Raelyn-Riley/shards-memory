// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! `shards-core`: the library behind `mem`.
//!
//! A *memory* is a git repository of markdown fact files. Every fact lives at
//! exactly one path, `facts/<section>/<...>/<name>.md`, and is addressed by
//! that path without the `facts/` prefix and `.md` suffix
//! (e.g. `devices/rotoforge-ctl/ip`). Every change to a fact is one commit on
//! `main`, made by this library. Nothing here can rewrite history: the git
//! wrapper only permits an allowlist of non-destructive subcommands.

pub mod activity;
pub mod address;
pub mod board;
pub mod config;
pub mod error;
pub mod fact;
pub mod git;
pub mod lock;
pub mod memory;
pub mod pointer;
pub mod sample;
pub mod shard;

pub use address::Address;
pub use board::Board;
pub use config::Config;
pub use error::{Error, Result};
pub use fact::{Fact, FrontMatter};
pub use memory::{Context, Fact as StoredFact, HistoryEntry, Memory, Provenance};
pub use pointer::{resolve, ResolveOptions};
pub use shard::{Conflict, DinitReport, ShardInfo, ShardMeta, ShardStatus, Side, Take};
