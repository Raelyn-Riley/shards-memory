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
pub mod coreadapter;
pub mod coresession;
pub mod error;
pub mod fact;
pub mod git;
pub mod incident;
pub mod index;
pub mod init;
pub mod lock;
pub mod memory;
pub mod notes;
pub mod pane;
pub mod pointer;
pub mod proposal;
pub mod question;
pub mod quirk;
pub(crate) mod record;
pub mod sample;
pub mod shard;
pub mod shardsession;
pub mod tmux;
pub mod watch;

pub use address::Address;
pub use board::Board;
pub use config::Config;
pub use coresession::{CheckReport, CoreStatus, StartReport, UsageEntry};
pub use error::{Error, Result};
pub use fact::{Fact, FrontMatter};
pub use incident::{Incident, IncidentNote, IncidentSide, Settlement};
pub use index::IndexEntry;
pub use memory::{Context, Fact as StoredFact, HistoryEntry, Memory, Provenance, Review};
pub use notes::{NoteHit, Owner, Rank};
pub use pointer::{resolve, ResolveOptions};
pub use proposal::{
    Change, ChangeOp, ChangeStatus, DecideReport, Decision, Draft, Proposal, Verdict,
};
pub use question::{Answer, Question, QuestionStatus};
pub use quirk::{NewQuirk, Quirk, QuirkState, QuirkVersion};
pub use shard::{Conflict, DinitReport, Resolution, ShardInfo, ShardMeta, ShardStatus, Side, Take};
pub use shardsession::{PaneState, ShardSession, ShardStart};
pub use watch::{CoreContext, Event, PassReport, WakeOutcome, CHECK_RULE};
