// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! `mem watch`: triggers, events, and waking the reasoning core.
//!
//! A watch *pass* builds the board, turns it into *conditions* (one per
//! enabled trigger that currently holds), and compares them with the
//! conditions that held at the previous pass:
//!
//! - a condition that holds now but didn't before **fires**: an event is
//!   appended to `.shards/events.jsonl` and added to the pending list;
//! - a condition that still holds does nothing (dedupe);
//! - a condition that no longer holds is forgotten, so it fires again if it
//!   comes back.
//!
//! Then, if events are pending, `[wake] command` is set and the last wake was
//! at least `cooldown_secs` ago, the command is run once with the pending
//! events as a JSON array on stdin. Success clears those events; failure or
//! timeout is logged to `.shards/wake.log` and they stay pending for the next
//! wake. The cooldown counts from every attempt, failed or not.
//!
//! # State and restarts
//!
//! `.shards/watch.json` holds the conditions that held at the last pass, the
//! pending events, the last wake time and the next event id. It is written
//! after every pass and read at the start of the next, by whichever process
//! runs it. So a restarted `mem watch` (or a `mem watch --once` from cron)
//! carries on where the last pass stopped: conditions that still hold do not
//! fire again, pending events are still pending, and the cooldown still
//! counts from the last real wake. A condition that cleared and came back
//! while no watcher was running looks as if it held throughout and does not
//! fire again. If `watch.json` is unreadable it is moved aside to
//! `watch.json.bad`, logged, and the watcher starts fresh (current conditions
//! fire once).
//!
//! # Concurrency
//!
//! Each pass's read-modify-write of `watch.json` happens under a flock on
//! `.shards/watch.lock` (held for milliseconds, never during a wake). A wake
//! holds a flock on `.shards/wake.lock`; a pass that finds it held does not
//! wake. So only one wake runs at a time, across processes. Neither is a main
//! or shard lock, so watching never blocks a writer.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::board::{Board, OverlapKind, State, BOARD_FORMAT};
use crate::config::TriggersConfig;
use crate::error::Result;
use crate::lock::FileMutex;
use crate::memory::Memory;

/// Pending events kept at most; older ones stay in events.jsonl only.
pub const PENDING_CAP: usize = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind {
    SameFact,
    SameSection,
    StaleWithWork,
    OpenConflict,
    SoulProposal,
}

impl EventKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventKind::SameFact => "same-fact",
            EventKind::SameSection => "same-section",
            EventKind::StaleWithWork => "stale-with-work",
            EventKind::OpenConflict => "open-conflict",
            EventKind::SoulProposal => "soul-proposal",
        }
    }
}

/// A trigger condition that holds right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub kind: EventKind,
    /// Stable dedupe key: the same situation always gives the same key.
    pub key: String,
    pub shards: Vec<String>,
    pub addresses: Vec<String>,
    pub humans: Vec<String>,
    pub summary: String,
}

/// One line of `.shards/events.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    /// Increasing, never reused.
    pub id: u64,
    /// RFC 3339, when it fired.
    pub time: String,
    pub kind: EventKind,
    pub shards: Vec<String>,
    pub addresses: Vec<String>,
    pub humans: Vec<String>,
    pub key: String,
    pub summary: String,
}

/// `.shards/watch.json`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchState {
    pub format: u32,
    /// Dedupe key -> when it started holding.
    pub active: BTreeMap<String, String>,
    pub pending: Vec<Event>,
    pub last_wake: Option<String>,
    pub next_id: u64,
    pub last_pass: Option<String>,
}

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// The conditions that hold on `board`, for the enabled triggers.
pub fn conditions(board: &Board, t: &TriggersConfig) -> Vec<Condition> {
    let mut out = Vec::new();
    for o in &board.overlaps {
        let (on, kind) = match o.kind {
            OverlapKind::SameFact => (t.same_fact, EventKind::SameFact),
            OverlapKind::SameSection => (t.same_section, EventKind::SameSection),
        };
        if !on {
            continue;
        }
        let what = match o.kind {
            OverlapKind::SameFact => format!("both have {} in flight", o.target),
            OverlapKind::SameSection => format!("are all in flight in section {}", o.target),
        };
        out.push(Condition {
            kind,
            key: format!("{}:{}:{}", kind.as_str(), o.target, o.shards.join(",")),
            shards: o.shards.clone(),
            addresses: o.addresses.clone(),
            humans: o.humans.clone(),
            summary: format!("shards {} {what}", o.shards.join(", ")),
        });
    }
    if t.stale_with_work {
        for s in &board.shards {
            if s.state == State::Stale && s.facts_in_flight > 0 {
                out.push(Condition {
                    kind: EventKind::StaleWithWork,
                    key: format!("stale-with-work:{}", s.id),
                    shards: vec![s.id.clone()],
                    addresses: s.facts().filter_map(|f| f.address.clone()).collect(),
                    humans: s.human.iter().cloned().collect(),
                    summary: format!(
                        "shard {} is stale (no activity since {}) with {} fact(s) in flight",
                        s.id, s.last_activity, s.facts_in_flight
                    ),
                });
            }
        }
    }
    if t.open_conflict {
        for c in &board.conflicts {
            out.push(Condition {
                kind: EventKind::OpenConflict,
                key: format!("open-conflict:{}", c.record),
                shards: vec![c.shard.clone()],
                addresses: vec![c.target.clone()],
                humans: c.humans.clone(),
                summary: format!(
                    "conflict on {} from shard {} waits for a human ({})",
                    c.target, c.shard, c.record
                ),
            });
        }
    }
    if t.soul_proposal {
        for p in &board.soul_proposals {
            out.push(Condition {
                kind: EventKind::SoulProposal,
                key: format!("soul-proposal:{}:{}", p.shard, p.paths.join(",")),
                shards: vec![p.shard.clone()],
                addresses: p.paths.clone(),
                humans: p.human.iter().cloned().collect(),
                summary: format!(
                    "shard {} proposes a soul change ({}); needs a human admin",
                    p.shard,
                    p.paths.join(", ")
                ),
            });
        }
    }
    out
}

impl WatchState {
    /// Compare current conditions with the last pass: return the newly
    /// fired events (also added to `pending`), forget cleared ones.
    pub fn observe(&mut self, conds: &[Condition], now: DateTime<Utc>) -> Vec<Event> {
        let time = rfc3339(now);
        let mut fired = Vec::new();
        let mut active = BTreeMap::new();
        for c in conds {
            if let Some(since) = self.active.get(&c.key) {
                active.insert(c.key.clone(), since.clone());
                continue;
            }
            if active.contains_key(&c.key) {
                continue;
            }
            active.insert(c.key.clone(), time.clone());
            self.next_id += 1;
            fired.push(Event {
                id: self.next_id,
                time: time.clone(),
                kind: c.kind,
                shards: c.shards.clone(),
                addresses: c.addresses.clone(),
                humans: c.humans.clone(),
                key: c.key.clone(),
                summary: c.summary.clone(),
            });
        }
        self.active = active;
        self.pending.extend(fired.iter().cloned());
        fired
    }

    /// When the cooldown ends, if a wake has happened.
    pub fn cooldown_until(&self, cooldown_secs: f64) -> Option<DateTime<Utc>> {
        let last = DateTime::parse_from_rfc3339(self.last_wake.as_deref()?).ok()?;
        Some(
            last.with_timezone(&Utc)
                + chrono::Duration::milliseconds((cooldown_secs.max(0.0) * 1000.0) as i64),
        )
    }
}

/// What a pass did about waking the core.
#[derive(Debug, Clone, PartialEq)]
pub enum WakeOutcome {
    /// No pending events.
    Idle,
    /// `[wake] command` is empty; events are recorded only.
    NoCommand,
    /// Woken too recently; pending events wait until then.
    Cooldown {
        until: String,
    },
    /// Another wake is running.
    Busy,
    Delivered {
        events: usize,
        secs: f64,
    },
    Failed {
        events: usize,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct PassReport {
    pub fired: Vec<Event>,
    /// Pending after this pass.
    pub pending: usize,
    pub wake: WakeOutcome,
}

/// `mem core context`: everything a woken core needs, in one document.
#[derive(Debug, Clone, Serialize)]
pub struct CoreContext {
    pub format: u32,
    pub board: Board,
    /// Events not yet delivered to (or not yet handled by) a wake.
    pub events: Vec<Event>,
}

fn append_line(path: &Path, line: &str) -> Result<()> {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    f.write_all(format!("{line}\n").as_bytes())?;
    Ok(())
}

/// Run the wake command. `Ok(seconds)` on exit 0.
fn run_wake(
    command: &str,
    input: &[u8],
    memory: &Path,
    timeout: Duration,
) -> std::result::Result<f64, String> {
    let started = Instant::now();
    let mut cmd = Command::new("sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(memory)
        .env("SHARDS_MEMORY", memory)
        .stdin(Stdio::piped())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Own process group, so a timeout can kill everything it started.
        cmd.process_group(0);
    }
    let mut child = cmd.spawn().map_err(|e| format!("could not start: {e}"))?;
    let mut stdin = child.stdin.take().expect("piped");
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        // A command that ignores stdin closes the pipe; that's fine.
        let _ = stdin.write_all(&input);
    });
    let result = loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break Ok(started.elapsed().as_secs_f64()),
            Ok(Some(status)) => break Err(format!("exited with {status}")),
            Ok(None) if started.elapsed() >= timeout => {
                #[cfg(unix)]
                let _ = Command::new("kill")
                    .args(["-KILL", "--", &format!("-{}", child.id())])
                    .stderr(Stdio::null())
                    .status();
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!(
                    "timed out after {:.1}s and was killed",
                    timeout.as_secs_f64()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => break Err(format!("wait failed: {e}")),
        }
    };
    let _ = writer.join();
    result
}

impl Memory {
    fn watch_path(&self) -> std::path::PathBuf {
        self.state_dir().join("watch.json")
    }

    pub fn events_path(&self) -> std::path::PathBuf {
        self.state_dir().join("events.jsonl")
    }

    pub fn wake_log_path(&self) -> std::path::PathBuf {
        self.state_dir().join("wake.log")
    }

    fn wake_log(&self, now: DateTime<Utc>, msg: &str) -> Result<()> {
        append_line(&self.wake_log_path(), &format!("{} {msg}", rfc3339(now)))
    }

    /// The saved watch state (fresh if none). An unreadable file is moved
    /// aside and logged.
    pub fn watch_state(&self) -> Result<WatchState> {
        let path = self.watch_path();
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(WatchState {
                    format: 1,
                    ..Default::default()
                })
            }
            Err(e) => return Err(e.into()),
        };
        match serde_json::from_str(&text) {
            Ok(s) => Ok(s),
            Err(e) => {
                fs::rename(&path, path.with_extension("json.bad"))?;
                self.wake_log(
                    Utc::now(),
                    &format!(
                        "watch.json unreadable ({e}); moved to watch.json.bad, starting fresh"
                    ),
                )?;
                Ok(WatchState {
                    format: 1,
                    ..Default::default()
                })
            }
        }
    }

    fn save_watch_state(&self, s: &WatchState) -> Result<()> {
        let path = self.watch_path();
        fs::create_dir_all(path.parent().expect("has parent"))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(s).expect("serializable"))?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    /// `mem core context`: the board plus pending events.
    pub fn core_context(&self, now: DateTime<Utc>) -> Result<CoreContext> {
        let main = self.main()?;
        let board = main.board(now)?;
        let events = main.watch_state()?.pending;
        Ok(CoreContext {
            format: BOARD_FORMAT,
            board,
            events,
        })
    }

    /// One watch pass as of `now`: evaluate triggers, record events, maybe
    /// wake the core. Never calls a model itself; a failing wake command is
    /// reported in the outcome, not as an error.
    pub fn watch_pass(&self, now: DateTime<Utc>) -> Result<PassReport> {
        let main = self.main()?;
        let board = main.board(now)?;
        let conds = conditions(&board, &main.config.triggers);
        let wake_cfg = main.config.wake.clone();
        let state_dir = main.state_dir();

        // Phase 1: record (short, under watch.lock).
        let (fired, deliver, wake_guard, outcome) = {
            let _g = FileMutex::lock(&state_dir.join("watch.lock"))?;
            let mut st = main.watch_state()?;
            let fired = st.observe(&conds, now);
            for e in &fired {
                append_line(
                    &main.events_path(),
                    &serde_json::to_string(e).expect("serializable"),
                )?;
            }
            if st.pending.len() > PENDING_CAP {
                let drop = st.pending.len() - PENDING_CAP;
                st.pending.drain(..drop);
                main.wake_log(
                    now,
                    &format!("dropped {drop} oldest pending event(s); they remain in events.jsonl"),
                )?;
            }
            st.last_pass = Some(rfc3339(now));
            let mut deliver = Vec::new();
            let mut guard = None;
            // `None`: this pass wakes the core.
            let outcome = if st.pending.is_empty() {
                Some(WakeOutcome::Idle)
            } else if wake_cfg.command.trim().is_empty() {
                Some(WakeOutcome::NoCommand)
            } else if let Some(until) = st
                .cooldown_until(wake_cfg.cooldown_secs)
                .filter(|u| *u > now)
            {
                Some(WakeOutcome::Cooldown {
                    until: rfc3339(until),
                })
            } else {
                match FileMutex::try_lock(&state_dir.join("wake.lock"))? {
                    None => Some(WakeOutcome::Busy),
                    Some(g) => {
                        // Claim the wake now, so the cooldown holds even
                        // for a pass that starts while this one runs.
                        st.last_wake = Some(rfc3339(now));
                        deliver = st.pending.clone();
                        guard = Some(g);
                        None
                    }
                }
            };
            main.save_watch_state(&st)?;
            (fired, deliver, guard, outcome)
        };

        // Phase 2: wake (no watch.lock held, so other passes can record).
        let outcome = if let Some(outcome) = outcome {
            outcome
        } else {
            let _wake = wake_guard;
            let input = serde_json::to_vec_pretty(&deliver).expect("serializable");
            let ids: Vec<u64> = deliver.iter().map(|e| e.id).collect();
            let res = run_wake(
                &wake_cfg.command,
                &input,
                &main.main_root,
                wake_cfg.timeout(),
            );
            let _g = FileMutex::lock(&state_dir.join("watch.lock"))?;
            let mut st = main.watch_state()?;
            let outcome = match res {
                Ok(secs) => {
                    st.pending.retain(|e| !ids.contains(&e.id));
                    main.wake_log(
                        now,
                        &format!(
                            "wake ok: delivered {} event(s) (ids {}) in {secs:.1}s",
                            ids.len(),
                            id_list(&ids)
                        ),
                    )?;
                    WakeOutcome::Delivered {
                        events: ids.len(),
                        secs,
                    }
                }
                Err(reason) => {
                    main.wake_log(
                        now,
                        &format!(
                            "wake FAILED: command {reason}; {} event(s) stay pending (ids {})",
                            ids.len(),
                            id_list(&ids)
                        ),
                    )?;
                    WakeOutcome::Failed {
                        events: ids.len(),
                        reason,
                    }
                }
            };
            main.save_watch_state(&st)?;
            outcome
        };

        let pending = main.watch_state()?.pending.len();
        Ok(PassReport {
            fired,
            pending,
            wake: outcome,
        })
    }
}

fn id_list(ids: &[u64]) -> String {
    ids.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cond(key: &str) -> Condition {
        Condition {
            kind: EventKind::SameFact,
            key: key.to_string(),
            shards: vec!["s1".into(), "s2".into()],
            addresses: vec!["a/b".into()],
            humans: vec![],
            summary: String::new(),
        }
    }

    #[test]
    fn fires_once_while_holding_and_again_after_clearing() {
        let t0 = Utc::now();
        let mut st = WatchState::default();
        assert_eq!(st.observe(&[cond("k")], t0).len(), 1);
        assert_eq!(st.observe(&[cond("k")], t0).len(), 0);
        assert_eq!(st.observe(&[cond("k"), cond("k")], t0).len(), 0);
        assert_eq!(st.observe(&[], t0).len(), 0);
        let again = st.observe(&[cond("k")], t0);
        assert_eq!(again.len(), 1);
        assert_eq!(again[0].id, 2);
        assert_eq!(st.pending.len(), 2);
        // Duplicates within one pass fire once.
        assert_eq!(st.observe(&[cond("x"), cond("x")], t0).len(), 1);
    }

    #[test]
    fn state_round_trips_so_a_restart_does_not_refire() {
        let t0 = Utc::now();
        let mut st = WatchState::default();
        st.observe(&[cond("k")], t0);
        let text = serde_json::to_string(&st).unwrap();
        let mut back: WatchState = serde_json::from_str(&text).unwrap();
        assert_eq!(back.observe(&[cond("k")], t0).len(), 0);
    }
}
