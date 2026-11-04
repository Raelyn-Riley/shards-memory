// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! The data core: one view of every open shard, computed in plain code.
//!
//! Per open shard the board reads, without asking the shard anything:
//!
//! - from git, every `facts/`, `people/` and `notes/` path changed on
//!   `shard/<id>` since its merge-base with main, plus uncommitted changes
//!   in its worktree: together, what it has *in flight*;
//! - from the activity source (see [`crate::activity`]), when it last did
//!   anything and its last few actions;
//! - the optional gist from `mem shard status`.
//!
//! Then it computes **overlaps** between shards (`same-fact`,
//! `same-section`) and lists open conflicts and pending soul proposals.
//!
//! Reading the board never takes a main or shard lock and never blocks a
//! writer: it only reads git (with `GIT_OPTIONAL_LOCKS=0`, so `git status`
//! doesn't touch the index lock) and updates `.shards/activity/`.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;

use crate::activity::Action;
use crate::address::Address;
use crate::error::Result;
use crate::fact::parse_front;
use crate::git::Opts;
use crate::memory::Memory;
use crate::shard::ShardInfo;

/// Bumped on any incompatible change to the JSON below.
pub const BOARD_FORMAT: u32 = 1;
/// Actions shown per shard.
pub const SHOW_ACTIONS: usize = 5;

/// Top-level sections whose changes count as work in flight.
const TRACKED: &[&str] = &["facts", "people", "notes"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Active,
    Idle,
    Stale,
}

impl State {
    pub fn as_str(&self) -> &'static str {
        match self {
            State::Active => "active",
            State::Idle => "idle",
            State::Stale => "stale",
        }
    }
}

/// One path a shard has in flight.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InFlight {
    /// Repo-relative path, e.g. `facts/devices/ctl/ip.md`.
    pub path: String,
    /// The fact address for `facts/` paths, else null.
    pub address: Option<String>,
    /// Top-level section: the fact's section (`devices`), or `people` / `notes`.
    pub section: String,
    /// True if (also) changed in the worktree but not yet committed.
    pub uncommitted: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ShardRow {
    pub id: String,
    pub human: Option<String>,
    pub branch: String,
    pub state: State,
    pub gist: Option<String>,
    pub gist_at: Option<String>,
    pub opened: String,
    /// Latest of: transcript activity, a commit on the shard, an
    /// uncommitted file change, the shard being opened.
    pub last_activity: String,
    /// Seconds since `last_activity`.
    pub age_secs: i64,
    /// Where `last_activity` came from: `transcript`, `commit`, `worktree`, `opened`.
    pub last_activity_from: String,
    pub in_flight: Vec<InFlight>,
    /// How many of `in_flight` are facts.
    pub facts_in_flight: usize,
    /// Newest last.
    pub recent_actions: Vec<Action>,
    pub soul_pending: Vec<String>,
}

impl ShardRow {
    pub fn facts(&self) -> impl Iterator<Item = &InFlight> {
        self.in_flight.iter().filter(|f| f.address.is_some())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OverlapKind {
    SameFact,
    SameSection,
}

impl OverlapKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            OverlapKind::SameFact => "same-fact",
            OverlapKind::SameSection => "same-section",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Overlap {
    pub kind: OverlapKind,
    /// The fact address (or `people/`/`notes/` path) for `same-fact`; the
    /// section for `same-section`.
    pub target: String,
    /// Sorted shard ids, two or more.
    pub shards: Vec<String>,
    /// Their humans, where known, in shard order.
    pub humans: Vec<String>,
    /// The shared in-flight addresses/paths.
    pub addresses: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OpenConflict {
    pub shard: String,
    /// Fact address, or repo path for profiles and notes.
    pub target: String,
    pub record: String,
    pub humans: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SoulProposal {
    pub shard: String,
    pub human: Option<String>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Thresholds {
    pub idle_secs: f64,
    pub stale_secs: f64,
}

/// `mem board --json`. See the README for the documented format.
#[derive(Debug, Clone, Serialize)]
pub struct Board {
    pub format: u32,
    pub generated: String,
    pub memory: String,
    pub activity_source: String,
    pub thresholds: Thresholds,
    pub shards: Vec<ShardRow>,
    pub overlaps: Vec<Overlap>,
    pub conflicts: Vec<OpenConflict>,
    pub soul_proposals: Vec<SoulProposal>,
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|t| t.with_timezone(&Utc))
}

fn section_of(path: &str) -> Option<(Option<String>, String)> {
    let top = path.split('/').next()?;
    if !TRACKED.contains(&top) {
        return None;
    }
    if top == "facts" {
        let a = Address::from_home(path)?;
        let section = a.section().to_string();
        Some((Some(a.to_string()), section))
    } else {
        Some((None, top.to_string()))
    }
}

impl Memory {
    fn no_lock_opts() -> Opts<'static> {
        Opts {
            env: vec![("GIT_OPTIONAL_LOCKS", "0".to_string())],
            ..Default::default()
        }
    }

    /// Paths changed on the shard since its merge-base with main (committed),
    /// and in its worktree (uncommitted, with file mtimes).
    fn shard_changes(
        &self,
        info: &ShardInfo,
    ) -> (BTreeSet<String>, BTreeMap<String, Option<DateTime<Utc>>>) {
        let mut committed = BTreeSet::new();
        let mut uncommitted = BTreeMap::new();
        let branch = &info.meta.branch;
        let tip = self.git.run(&["rev-parse", "--verify", "--quiet", branch]);
        if let Ok(tip) = tip {
            let tip = tip.trim();
            if let Ok(base) = self.git.run(&["merge-base", "main", tip]) {
                if let Ok(out) = self.git.run(&[
                    "diff-tree",
                    "-r",
                    "--no-commit-id",
                    "--name-only",
                    base.trim(),
                    tip,
                ]) {
                    committed.extend(out.lines().map(String::from));
                }
            }
        }
        let wt = &info.meta.worktree;
        if wt.is_dir() {
            if let Ok(out) = self.git.at(wt).exec(
                &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
                Self::no_lock_opts(),
            ) {
                let mut it = out.split('\0');
                while let Some(entry) = it.next() {
                    if entry.len() < 4 {
                        continue;
                    }
                    let (xy, path) = entry.split_at(3);
                    if xy.starts_with('R') || xy.starts_with('C') {
                        // The source path follows; it changed too.
                        if let Some(from) = it.next() {
                            uncommitted.insert(from.to_string(), None);
                        }
                    }
                    let mtime = fs::metadata(wt.join(path))
                        .and_then(|m| m.modified())
                        .ok()
                        .map(DateTime::<Utc>::from);
                    uncommitted.insert(path.to_string(), mtime);
                }
            }
        }
        (committed, uncommitted)
    }

    fn shard_row(&self, info: ShardInfo, now: DateTime<Utc>) -> Result<ShardRow> {
        let activity = self.observe(&info.meta)?;
        let (committed, uncommitted) = self.shard_changes(&info);

        let mut in_flight = Vec::new();
        let all: BTreeSet<&String> = committed.iter().chain(uncommitted.keys()).collect();
        for p in all {
            if let Some((address, section)) = section_of(p) {
                in_flight.push(InFlight {
                    path: p.clone(),
                    address,
                    section,
                    uncommitted: uncommitted.contains_key(p),
                });
            }
        }

        // Last activity: the latest of several observations.
        let mut best: (DateTime<Utc>, &str) = (
            parse_time(&info.meta.opened).unwrap_or(DateTime::<Utc>::UNIX_EPOCH),
            "opened",
        );
        let mut consider = |t: Option<DateTime<Utc>>, from: &'static str| {
            if let Some(t) = t {
                if t > best.0 {
                    best = (t, from);
                }
            }
        };
        consider(
            activity.last_activity.as_deref().and_then(parse_time),
            "transcript",
        );
        let range = format!("main..{}", info.meta.branch);
        consider(
            self.git
                .run(&["log", "-1", "--format=%cI", &range])
                .ok()
                .and_then(|s| parse_time(&s)),
            "commit",
        );
        consider(uncommitted.values().filter_map(|t| *t).max(), "worktree");
        let (last, from) = best;
        let age = (now - last).num_seconds().max(0);
        let cfg = &self.config.awareness;
        let state = if (age as f64) >= cfg.stale_secs {
            State::Stale
        } else if (age as f64) >= cfg.idle_secs {
            State::Idle
        } else {
            State::Active
        };

        let facts_in_flight = in_flight.iter().filter(|f| f.address.is_some()).count();
        let mut recent = activity.recent;
        let skip = recent.len().saturating_sub(SHOW_ACTIONS);
        recent.drain(..skip);
        Ok(ShardRow {
            id: info.meta.id.clone(),
            human: info.meta.human.clone(),
            branch: info.meta.branch.clone(),
            state,
            gist: info.status.as_ref().map(|s| s.gist.clone()),
            gist_at: info.status.as_ref().map(|s| s.at.clone()),
            opened: info.meta.opened.clone(),
            last_activity: rfc3339(last),
            age_secs: age,
            last_activity_from: from.to_string(),
            in_flight,
            facts_in_flight,
            recent_actions: recent,
            soul_pending: info.meta.soul_pending.clone(),
        })
    }

    fn open_conflicts(&self) -> Result<Vec<OpenConflict>> {
        let out = self.git.run(&["ls-files", "--", "conflicts/"])?;
        let mut v = Vec::new();
        for record in out.lines() {
            let Some(rest) = record.strip_prefix("conflicts/") else {
                continue;
            };
            let Some((shard, tail)) = rest.split_once('/') else {
                continue;
            };
            let target = match tail.strip_prefix("@/") {
                Some(p) => p.to_string(),
                None => tail.strip_suffix(".md").unwrap_or(tail).to_string(),
            };
            let mut humans = Vec::new();
            if let Ok(Some(text)) = self.read_file_at("main", record) {
                if let Ok((pairs, _)) = parse_front(&text, record) {
                    for k in ["main_human", "shard_human"] {
                        if let Some((_, h)) = pairs.iter().find(|(key, _)| key == k) {
                            if !h.is_empty() && !humans.contains(h) {
                                humans.push(h.clone());
                            }
                        }
                    }
                }
            }
            v.push(OpenConflict {
                shard: shard.to_string(),
                target,
                record: record.to_string(),
                humans,
            });
        }
        Ok(v)
    }

    /// Build the board as of `now`. Takes no main or shard lock.
    pub fn board(&self, now: DateTime<Utc>) -> Result<Board> {
        let main = self.main()?;
        let mut shards = Vec::new();
        for info in main.shard_list()? {
            shards.push(main.shard_row(info, now)?);
        }
        let overlaps = overlaps(&shards);
        let soul_proposals = shards
            .iter()
            .filter(|s| !s.soul_pending.is_empty())
            .map(|s| SoulProposal {
                shard: s.id.clone(),
                human: s.human.clone(),
                paths: s.soul_pending.clone(),
            })
            .collect();
        let cfg = &main.config;
        Ok(Board {
            format: BOARD_FORMAT,
            generated: rfc3339(now),
            memory: main.main_root.to_string_lossy().to_string(),
            activity_source: cfg.activity.source.as_str().to_string(),
            thresholds: Thresholds {
                idle_secs: cfg.awareness.idle_secs,
                stale_secs: cfg.awareness.stale_secs,
            },
            conflicts: main.open_conflicts()?,
            shards,
            overlaps,
            soul_proposals,
        })
    }
}

/// Overlaps between open shards, computed from what each has in flight.
pub fn overlaps(shards: &[ShardRow]) -> Vec<Overlap> {
    // target -> shard -> addresses
    let mut by_fact: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();
    let mut by_section: BTreeMap<String, BTreeMap<&str, BTreeSet<String>>> = BTreeMap::new();
    for s in shards {
        for f in &s.in_flight {
            let target = f.address.clone().unwrap_or_else(|| f.path.clone());
            by_fact.entry(target.clone()).or_default().insert(&s.id);
            by_section
                .entry(f.section.clone())
                .or_default()
                .entry(&s.id)
                .or_default()
                .insert(target);
        }
    }
    let humans = |ids: &[String]| -> Vec<String> {
        ids.iter()
            .filter_map(|id| shards.iter().find(|s| &s.id == id)?.human.clone())
            .collect()
    };
    let mut out = Vec::new();
    for (target, ids) in by_fact {
        if ids.len() >= 2 {
            let ids: Vec<String> = ids.into_iter().map(String::from).collect();
            out.push(Overlap {
                kind: OverlapKind::SameFact,
                humans: humans(&ids),
                addresses: vec![target.clone()],
                target,
                shards: ids,
            });
        }
    }
    for (section, per) in by_section {
        if per.len() >= 2 {
            let ids: Vec<String> = per.keys().map(|s| s.to_string()).collect();
            let addresses: BTreeSet<String> = per.into_values().flatten().collect();
            out.push(Overlap {
                kind: OverlapKind::SameSection,
                humans: humans(&ids),
                addresses: addresses.into_iter().collect(),
                target: section,
                shards: ids,
            });
        }
    }
    out
}

/// "3m", "2h", "5d" for an age in seconds.
pub fn human_age(secs: i64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86400),
    }
}
