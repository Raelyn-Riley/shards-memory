// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! The reasoning core as one long-lived chat session in a tmux pane.
//!
//! `mem core start` starts the core's command in a pane on the core's own
//! tmux server and gives it a first message: read `soul/`, then
//! `mem core context`. The session keeps what it has reasoned about, so the
//! watcher wakes it by typing one short line into the pane rather than
//! running a fresh one-shot command each time. A person can attach to the
//! pane and talk to it.
//!
//! The session id is *pinned* in `.shards/core/session.json`. The watchdog
//! (`mem core check`, and each `mem watch` pass) restores a dead pane or
//! command and resumes the pinned session by its explicit id. When the
//! session fills up, `mem core rinit` hands off: the old session exits and a
//! fresh one starts in the same pane with a baton as its first message.
//!
//! Everything that drives the core runs under a flock on
//! `.shards/core/core.lock`. A handoff holds it from start to end; wakes,
//! watchdog passes and keep-warm pings only *try* it, so during a handoff
//! they do nothing and events stay pending.
//!
//! Files in `.shards/core/`: `session.json` (the pin), `state.json`
//! (watchdog, keep-warm and transcript-scan state: times and numbers only),
//! `core.log`, `usage.jsonl`, `first-message.md` and the tmux socket.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Local, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::coreadapter::{adapter_for, parse, CoreAdapter, Launch, TranscriptScan, Usage};
use crate::error::{Error, Result};
use crate::lock::FileMutex;
use crate::memory::Memory;
use crate::pane::{pane_path, PaneRunner};
use crate::tmux::{self, Tmux};

/// Consecutive failed restores after which the watchdog stops trying.
pub const MAX_FAILURES: u32 = 3;
/// A sent ping or wake whose reply never shows is logged without usage
/// after this long.
const REPLY_WAIT_SECS: i64 = 15 * 60;
/// The pane shell's prompt.
const PS1: &str = "core$ ";

fn rfc3339(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// `.shards/core/session.json`: which session is the core.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionPin {
    pub adapter: String,
    /// `None` until found (or for an adapter without sessions).
    pub session_id: Option<String>,
    pub cwd: String,
    pub started_at: String,
    pub pinned_at: Option<String>,
    /// Earlier sessions this core handed off from, oldest first.
    #[serde(default)]
    pub previous: Vec<String>,
}

/// A ping or wake whose reply hasn't been read yet.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Sent {
    pub at: String,
    /// Prompts in the transcript when it was sent: its reply is the first
    /// finished turn after this.
    #[serde(default)]
    pub after: u64,
    pub kind: String,
    pub gap_secs: Option<f64>,
    pub idle_secs: Option<f64>,
    #[serde(default)]
    pub events: Option<usize>,
    /// It landed as a queued line (the core was busy), not a new prompt.
    #[serde(default)]
    pub queued: bool,
}

/// `.shards/core/state.json`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CoreState {
    /// A core was started and not deliberately stopped since.
    pub started: bool,
    pub stopped_at: Option<String>,
    /// Consecutive failed restores.
    pub failures: u32,
    /// The watchdog gave up after [`MAX_FAILURES`]; `mem core start` resets.
    pub gave_up: bool,
    pub last_restore: Option<String>,
    pub last_wake: Option<String>,
    pub last_ping: Option<String>,
    /// Pings since the last real activity.
    pub quiet_pings: u32,
    /// The last real (non-heartbeat) prompt seen in the transcript.
    pub last_real_seen: Option<String>,
    /// The last usage.jsonl entry's time (for gaps).
    pub last_sent: Option<String>,
    /// The prompt number of the last reply counted in usage.jsonl.
    #[serde(default)]
    pub last_counted: u64,
    pub awaiting: Vec<Sent>,
    pub scan: TranscriptScan,
}

/// Why a typed line did or didn't land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Landed {
    /// Seen as a prompt (or a queued prompt) in the transcript.
    Confirmed,
    /// Typed and Enter pressed, but the transcript didn't show it in time
    /// (or the adapter has no transcript).
    Unconfirmed,
}

/// What a pane wake did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PaneWake {
    Landed(Landed),
    /// A handoff or another core operation holds the core lock.
    Busy,
    /// No pane or no running command.
    Down,
    Failed(String),
}

/// Result of one watchdog / keep-warm step, for printing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CheckReport {
    pub notes: Vec<String>,
    /// The watchdog had to restore the core this pass.
    pub restored: bool,
    pub pinged: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct KeepWarmStatus {
    pub enabled: bool,
    pub state: String,
    pub last_entry: Option<String>,
    pub quiet_pings: u32,
    pub quiet_limit: u32,
    pub last_ping: Option<String>,
}

/// `mem core status`.
#[derive(Debug, Clone, Serialize)]
pub struct CoreStatus {
    pub started: bool,
    pub pane_up: bool,
    pub command_alive: bool,
    pub adapter: String,
    pub session_id: Option<String>,
    pub pinned_at: Option<String>,
    pub last_wake: Option<String>,
    pub wake_mode: String,
    pub watchdog: bool,
    pub failures: u32,
    pub gave_up: bool,
    pub keep_warm: KeepWarmStatus,
    pub attach: String,
}

/// One line of `usage.jsonl`: numbers only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct UsageEntry {
    pub time: String,
    /// `ping` or `wake`.
    pub kind: String,
    /// Since the previous usage entry.
    pub gap_secs: Option<f64>,
    /// Since the core's last transcript entry, when it was sent.
    pub idle_secs: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub events: Option<usize>,
    /// A reply was found in the transcript.
    pub reply: bool,
    #[serde(default)]
    pub calls: u32,
    #[serde(default)]
    pub usage: Usage,
    /// The reply's first model call, which shows whether the cache was warm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first: Option<Usage>,
    /// Queued into a turn that an earlier entry already counted: its usage
    /// is in that entry, so it is left out of the summary.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub same_turn: bool,
}

impl UsageEntry {
    /// Warm: the first call read more from the cache than it wrote to it.
    pub fn warm(&self) -> Option<bool> {
        self.first.map(|f| f.cache_read > f.cache_creation)
    }
}

/// One row of `mem core usage`.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct UsageRow {
    pub kind: String,
    pub gap: String,
    /// `warm`, `cold` or `unknown` (no reply seen).
    pub cache: String,
    pub count: usize,
    pub avg_input: f64,
    pub avg_output: f64,
    pub avg_cache_read: f64,
    pub avg_cache_creation: f64,
}

/// Gap buckets for `mem core usage`, in seconds (upper bounds).
pub const GAP_BUCKETS: &[(f64, &str)] = &[
    (300.0, "<5m"),
    (1800.0, "5-30m"),
    (3600.0, "30-60m"),
    (7200.0, "1-2h"),
    (f64::INFINITY, "2h+"),
];

pub fn gap_bucket(gap: Option<f64>) -> &'static str {
    match gap {
        None => "first",
        Some(g) => GAP_BUCKETS
            .iter()
            .find(|(b, _)| g < *b)
            .map(|(_, n)| *n)
            .unwrap_or("2h+"),
    }
}

/// Summarize usage entries: per kind, gap bucket and warm/cold.
pub fn summarize_usage(entries: &[UsageEntry]) -> Vec<UsageRow> {
    let order = |g: &str| {
        GAP_BUCKETS
            .iter()
            .position(|(_, n)| *n == g)
            .map(|i| i + 1)
            .unwrap_or(0)
    };
    let mut groups: BTreeMap<(String, usize, String), Vec<&UsageEntry>> = BTreeMap::new();
    for e in entries.iter().filter(|e| !e.same_turn) {
        let gap = gap_bucket(e.gap_secs).to_string();
        groups
            .entry((e.kind.clone(), order(&gap), gap))
            .or_default()
            .push(e);
    }
    let mut rows = Vec::new();
    for ((kind, _, gap), es) in groups {
        for cache in ["warm", "cold", "unknown"] {
            let sel: Vec<&&UsageEntry> = es
                .iter()
                .filter(|e| match e.warm() {
                    Some(true) => cache == "warm",
                    Some(false) => cache == "cold",
                    None => cache == "unknown",
                })
                .collect();
            if sel.is_empty() {
                continue;
            }
            let n = sel.len() as f64;
            let avg =
                |f: fn(&Usage) -> u64| sel.iter().map(|e| f(&e.usage) as f64).sum::<f64>() / n;
            rows.push(UsageRow {
                kind: kind.clone(),
                gap: gap.clone(),
                cache: cache.to_string(),
                count: sel.len(),
                avg_input: avg(|u| u.input),
                avg_output: avg(|u| u.output),
                avg_cache_read: avg(|u| u.cache_read),
                avg_cache_creation: avg(|u| u.cache_creation),
            });
        }
    }
    rows
}

/// The core's first message. The baton, if any, is appended.
pub fn first_message(root: &Path, baton: Option<&str>) -> String {
    let mut s = format!(
        "You are the shards core for the memory at {}. Read soul/core.md and the rest of \
         soul/, then run `mem core context`. A line starting with [WAKE] means events are \
         pending: run `mem core context`, reason, tell people, write decisions to main, then \
         run `mem core ack --upto <n>` with the number from the [WAKE] line. A message that \
         is exactly [HH:MM] is a heartbeat: reply with one character and do nothing else.",
        root.display()
    );
    if let Some(b) = baton {
        s.push_str("\n\nBaton from your previous session:\n\n");
        s.push_str(b.trim_end());
    }
    s.push('\n');
    s
}

/// The wake line: one short line; the data is read with `mem core context`.
pub fn wake_line(events: usize, upto: u64) -> String {
    format!(
        "[WAKE] {events} event{}, up to #{upto}: run mem core context",
        if events == 1 { "" } else { "s" }
    )
}

fn heartbeat_text(now: DateTime<Utc>) -> String {
    now.with_timezone(&Local).format("[%H:%M]").to_string()
}

fn secs_between(a: DateTime<Utc>, b: DateTime<Utc>) -> f64 {
    (a - b).num_milliseconds() as f64 / 1000.0
}

/// Everything needed to drive the core of one memory.
struct Core {
    root: PathBuf,
    dir: PathBuf,
    cwd: PathBuf,
    pane: PaneRunner,
    adapter: Box<dyn CoreAdapter>,
    cfg: crate::config::CoreConfig,
}

impl Core {
    fn pin_path(&self) -> PathBuf {
        self.dir.join("session.json")
    }
    fn state_path(&self) -> PathBuf {
        self.dir.join("state.json")
    }
    fn usage_path(&self) -> PathBuf {
        self.dir.join("usage.jsonl")
    }
    fn first_path(&self) -> PathBuf {
        self.dir.join("first-message.md")
    }

    fn log(&self, msg: &str) {
        let _ = fs::create_dir_all(&self.dir);
        let line = format!("{} {msg}\n", rfc3339(Utc::now()));
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("core.log"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    }

    fn read_json<T: for<'de> Deserialize<'de> + Default>(path: &Path) -> T {
        fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn write_json<T: Serialize>(path: &Path, v: &T) -> Result<()> {
        fs::create_dir_all(path.parent().expect("has parent"))?;
        let tmp = path.with_extension("json.tmp");
        fs::write(&tmp, serde_json::to_string_pretty(v).expect("serializable"))?;
        fs::rename(tmp, path)?;
        Ok(())
    }

    fn pin(&self) -> SessionPin {
        Self::read_json(&self.pin_path())
    }
    fn state(&self) -> CoreState {
        Self::read_json(&self.state_path())
    }
    fn save_pin(&self, p: &SessionPin) -> Result<()> {
        Self::write_json(&self.pin_path(), p)
    }
    fn save_state(&self, s: &CoreState) -> Result<()> {
        Self::write_json(&self.state_path(), s)
    }

    fn lock(&self) -> Result<FileMutex> {
        FileMutex::lock(&self.dir.join("core.lock"))
    }
    fn try_lock(&self) -> Result<Option<FileMutex>> {
        FileMutex::try_lock(&self.dir.join("core.lock"))
    }

    /// (pane up, command alive).
    fn alive(&self) -> (bool, bool) {
        self.pane.alive()
    }

    fn type_line(&self, text: &str) -> Result<()> {
        self.pane.type_line(text)
    }

    /// Type a line to the running core and confirm it reached the session,
    /// noting on the last [`Sent`] whether it was queued (the core was busy).
    fn deliver(&self, st: &mut CoreState, text: &str) -> Result<Landed> {
        let path = self.transcript();
        if let Some(p) = &path {
            st.scan.update(p);
        }
        let before = (st.scan.prompts, st.scan.queued);
        self.type_line(text)?;
        let Some(path) = path else {
            return Ok(Landed::Unconfirmed);
        };
        let start = Instant::now();
        loop {
            st.scan.update(&path);
            if st.scan.landed_since(before) {
                if let Some(s) = st.awaiting.last_mut() {
                    s.queued = st.scan.prompts == before.0;
                }
                return Ok(Landed::Confirmed);
            }
            if start.elapsed() >= self.cfg.land_timeout() {
                return Ok(Landed::Unconfirmed);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn transcript(&self) -> Option<PathBuf> {
        let id = self.pin().session_id?;
        self.adapter.transcript(&self.cwd, &id)
    }

    /// Start a fresh session (pane must have a live shell). Returns the
    /// pinned id, if the adapter has sessions.
    fn launch_fresh(&self, baton: Option<&str>, not: Option<&str>) -> Result<Option<String>> {
        let first = self.first_path();
        fs::create_dir_all(&self.dir)?;
        fs::write(&first, first_message(&self.root, baton))?;
        let started = Utc::now();
        let line = self.adapter.launch_line(
            &self.cfg.command,
            &Launch::Fresh {
                first_message: &first,
            },
        );
        self.type_line(&line)?;
        if !self.pane.wait_alive(self.cfg.start_timeout()) {
            return Err(Error::Core(
                "the core's command did not start (or exited)".into(),
            ));
        }
        if !self.adapter.has_sessions() {
            self.type_line(&format!(
                "[START] read {} and follow it",
                first.to_string_lossy()
            ))?;
            return Ok(None);
        }
        // The first message rides on the command line; the session's
        // transcript appearing is the proof it was delivered.
        let start = Instant::now();
        loop {
            if let Some(id) = self.adapter.find_session(&self.cwd, started, not) {
                return Ok(Some(id));
            }
            if !self.alive().1 {
                return Err(Error::Core(
                    "the new session exited before it could be pinned".into(),
                ));
            }
            if start.elapsed() >= self.cfg.start_timeout() {
                return Err(Error::Core(
                    "no transcript for the new session appeared; first message not confirmed"
                        .into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn launch_resume(&self, id: &str) -> Result<()> {
        let line = self
            .adapter
            .launch_line(&self.cfg.command, &Launch::Resume { id });
        self.type_line(&line)?;
        if !self.pane.wait_alive(self.cfg.start_timeout()) {
            return Err(Error::Core(format!(
                "resuming session {id}: the command did not start (or exited)"
            )));
        }
        Ok(())
    }

    /// Tell the running command to exit; true once the pane is back at its
    /// shell.
    fn exit_command(&self) -> bool {
        self.pane.exit_command(self.adapter.exit_text())
    }

    /// Read new transcript lines and write usage entries for replies found.
    fn collect_usage(&self, st: &mut CoreState, now: DateTime<Utc>) -> Result<()> {
        if let Some(path) = self.transcript() {
            st.scan.update(&path);
        }
        while let Some(s) = st.awaiting.first().cloned() {
            let Some(at) = parse(&s.at) else {
                st.awaiting.remove(0);
                continue;
            };
            let reply = st
                .scan
                .reply_for(s.after, s.queued, st.last_counted)
                .map(|(t, shared)| (t.clone(), shared));
            let entry = match reply {
                Some((_, true)) => UsageEntry {
                    time: s.at.clone(),
                    kind: s.kind.clone(),
                    gap_secs: s.gap_secs,
                    idle_secs: s.idle_secs,
                    events: s.events,
                    reply: true,
                    calls: 0,
                    usage: Usage::default(),
                    first: None,
                    same_turn: true,
                },
                Some((t, false)) => {
                    st.last_counted = st.last_counted.max(t.seq);
                    UsageEntry {
                        time: s.at.clone(),
                        kind: s.kind.clone(),
                        gap_secs: s.gap_secs,
                        idle_secs: s.idle_secs,
                        events: s.events,
                        reply: true,
                        calls: t.calls,
                        usage: t.total,
                        first: t.first,
                        same_turn: false,
                    }
                }
                None if (now - at).num_seconds() > REPLY_WAIT_SECS => UsageEntry {
                    time: s.at.clone(),
                    kind: s.kind.clone(),
                    gap_secs: s.gap_secs,
                    idle_secs: s.idle_secs,
                    events: s.events,
                    reply: false,
                    calls: 0,
                    usage: Usage::default(),
                    first: None,
                    same_turn: false,
                },
                None => break,
            };
            let line = serde_json::to_string(&entry).expect("serializable");
            crate::watch::append_line(&self.usage_path(), &line)?;
            st.awaiting.remove(0);
        }
        Ok(())
    }

    /// Note a sent ping or wake, for usage.jsonl.
    fn sent(&self, st: &mut CoreState, kind: &str, now: DateTime<Utc>, events: Option<usize>) {
        let gap = st
            .last_sent
            .as_deref()
            .and_then(parse)
            .map(|p| secs_between(now, p));
        let idle = st
            .scan
            .last_entry
            .as_deref()
            .and_then(parse)
            .map(|p| secs_between(now, p).max(0.0));
        st.awaiting.push(Sent {
            after: st.scan.prompts,
            at: rfc3339(now),
            kind: kind.to_string(),
            gap_secs: gap,
            idle_secs: idle,
            events,
            queued: false,
        });
        st.last_sent = Some(rfc3339(now));
    }

    /// Late pinning: if a session is running but its id isn't known yet.
    fn try_pin(&self) -> Result<()> {
        let mut pin = self.pin();
        if pin.session_id.is_some() || !self.adapter.has_sessions() {
            return Ok(());
        }
        let Some(started) = parse(&pin.started_at) else {
            return Ok(());
        };
        if let Some(id) = self.adapter.find_session(&self.cwd, started, None) {
            self.log(&format!("pinned session {id} (late)"));
            pin.session_id = Some(id);
            pin.pinned_at = Some(rfc3339(Utc::now()));
            self.save_pin(&pin)?;
        }
        Ok(())
    }
}

/// What `mem core start` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartReport {
    AlreadyRunning {
        session_id: Option<String>,
    },
    Started {
        session_id: Option<String>,
        resumed: bool,
    },
}

impl Memory {
    fn core(&self) -> Result<Core> {
        let main = self.main()?;
        let cfg = main.config().clone();
        let root = main.main_root().to_path_buf();
        let abs = |s: &str| {
            let p = Path::new(s);
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                root.join(p)
            }
        };
        let cwd = if cfg.core.cwd.trim().is_empty() {
            root.clone()
        } else {
            abs(cfg.core.cwd.trim())
        };
        let pane = PaneRunner {
            tmux: Tmux::new(abs(&cfg.core.socket), cfg.core.session.clone()),
            cwd: cwd.clone(),
            env: vec![
                (
                    "SHARDS_MEMORY".to_string(),
                    root.to_string_lossy().into_owned(),
                ),
                // As a shard's pane: `mem`'s directory, `~/.local/bin`, `~/bin`.
                ("PATH".to_string(), pane_path()),
                ("PS1".to_string(), PS1.to_string()),
                ("ENV".to_string(), String::new()),
            ],
            ps1: PS1.to_string(),
            start_timeout: cfg.core.start_timeout(),
            exit_timeout: cfg.core.exit_timeout(),
            land_timeout: cfg.core.land_timeout(),
        };
        Ok(Core {
            dir: main.state_dir().join("core"),
            pane,
            adapter: adapter_for(cfg.core.adapter, &cfg, &root),
            cwd,
            root,
            cfg: cfg.core,
        })
    }

    /// Has a core been started (and not deliberately stopped)?
    pub fn core_started(&self) -> bool {
        self.core().is_ok_and(|c| c.state().started)
    }

    /// `mem core start`: a fresh session with the first message, or with
    /// `resume`, the pinned session. A no-op if the core is running.
    pub fn core_start(&self, resume: bool) -> Result<StartReport> {
        if !tmux::available() {
            return Err(Error::Core("tmux is not installed".into()));
        }
        let c = self.core()?;
        let _g = c.lock()?;
        let mut st = c.state();
        if c.alive().1 {
            if !st.started {
                st.started = true;
                st.stopped_at = None;
                c.save_state(&st)?;
            }
            return Ok(StartReport::AlreadyRunning {
                session_id: c.pin().session_id,
            });
        }
        c.pane.ensure_pane()?;
        let old = c.pin();
        let now = Utc::now();
        let (id, resumed) = match old.session_id.as_deref().filter(|_| resume) {
            Some(id) => {
                c.launch_resume(id)?;
                (Some(id.to_string()), true)
            }
            None => (c.launch_fresh(None, None)?, false),
        };
        let pin = SessionPin {
            adapter: c.adapter.name().to_string(),
            session_id: id.clone(),
            cwd: c.cwd.to_string_lossy().into_owned(),
            started_at: rfc3339(now),
            pinned_at: id.as_ref().map(|_| rfc3339(Utc::now())),
            previous: if resumed {
                old.previous
            } else {
                let mut p = old.previous;
                p.extend(old.session_id);
                p
            },
        };
        c.save_pin(&pin)?;
        st = CoreState {
            started: true,
            last_wake: st.last_wake,
            last_sent: st.last_sent,
            ..Default::default()
        };
        if resumed {
            st.scan = c.state().scan;
        }
        c.save_state(&st)?;
        c.log(&format!(
            "{} session {}",
            if resumed {
                "start: resumed"
            } else {
                "start: fresh"
            },
            id.as_deref().unwrap_or("(no id)")
        ));
        if !resumed {
            self.watch_reset_woken()?;
        }
        Ok(StartReport::Started {
            session_id: id,
            resumed,
        })
    }

    /// `mem core stop`: deliberate, so the watchdog leaves it stopped.
    pub fn core_stop(&self) -> Result<bool> {
        let c = self.core()?;
        let _g = c.lock()?;
        let mut st = c.state();
        let was = st.started || c.pane.tmux.has_session();
        st.started = false;
        st.stopped_at = Some(rfc3339(Utc::now()));
        c.save_state(&st)?;
        if c.pane.tmux.has_session() {
            if !c.exit_command() {
                c.log("stop: the command did not exit when told; killing the pane");
            }
            c.pane.tmux.kill_server();
        }
        c.log("stop: deliberate; the watchdog will leave it stopped");
        Ok(was)
    }

    pub fn core_attach_command(&self) -> Result<Vec<String>> {
        Ok(self.core()?.pane.tmux.attach_command())
    }

    /// Attach this terminal to the core's pane (detach with the tmux prefix
    /// then `d`; the core keeps running).
    pub fn core_attach(&self) -> Result<()> {
        let c = self.core()?;
        if !c.pane.tmux.has_session() {
            return Err(Error::Core("the core isn't running; mem core start".into()));
        }
        let cmd = c.pane.tmux.attach_command();
        let status = std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .env_remove("TMUX")
            .status()?;
        if !status.success() {
            return Err(Error::Core(format!("tmux attach exited with {status}")));
        }
        Ok(())
    }

    pub fn core_status(&self, now: DateTime<Utc>) -> Result<CoreStatus> {
        let c = self.core()?;
        if let Ok(Some(_g)) = c.try_lock() {
            let _ = c.try_pin();
        }
        let st = c.state();
        let pin = c.pin();
        let (pane_up, alive) = c.alive();
        let mut scan = st.scan.clone();
        if let Some(p) = c.transcript() {
            scan.update(&p);
        }
        let main = self.main()?;
        let watch = main.watch_state()?;
        let mode = match main.config().wake.mode {
            crate::config::WakeMode::Auto => {
                if st.started {
                    "auto (pane)"
                } else {
                    "auto (command)"
                }
            }
            m => m.as_str(),
        };
        Ok(CoreStatus {
            started: st.started,
            pane_up,
            command_alive: alive,
            adapter: c.adapter.name().to_string(),
            session_id: pin.session_id,
            pinned_at: pin.pinned_at,
            last_wake: watch.last_wake,
            wake_mode: mode.to_string(),
            watchdog: c.cfg.watchdog,
            failures: st.failures,
            gave_up: st.gave_up,
            keep_warm: KeepWarmStatus {
                enabled: c.cfg.keep_warm,
                state: keep_warm_state(&c.cfg, &st, &scan, now),
                last_entry: scan.last_entry.clone(),
                quiet_pings: st.quiet_pings,
                quiet_limit: c.cfg.keep_warm_quiet_limit,
                last_ping: st.last_ping.clone(),
            },
            attach: c.pane.tmux.attach_command().join(" "),
        })
    }

    /// The watchdog: if a started core's pane or command is gone, restore
    /// it and resume the pinned session by id. Gives up after
    /// [`MAX_FAILURES`] failures in a row. Never undoes `mem core stop`.
    pub fn core_watchdog(&self, now: DateTime<Utc>) -> Result<CheckReport> {
        let mut r = CheckReport::default();
        let c = self.core()?;
        let Some(_g) = c.try_lock()? else {
            r.notes
                .push("core busy (handoff in progress); watchdog skipped".into());
            return Ok(r);
        };
        let mut st = c.state();
        if !st.started {
            return Ok(r);
        }
        if st.gave_up {
            r.notes.push(format!(
                "watchdog gave up after {MAX_FAILURES} failed restores; run mem core start"
            ));
            return Ok(r);
        }
        let (up, alive) = c.alive();
        if alive {
            c.try_pin()?;
            return Ok(r);
        }
        if !tmux::available() {
            return Err(Error::Core("tmux is not installed".into()));
        }
        let pin = c.pin();
        c.log(&format!(
            "watchdog: {} gone; restoring",
            if up { "command" } else { "pane" }
        ));
        let res = c
            .pane
            .ensure_pane()
            .and_then(|_| match pin.session_id.as_deref() {
                Some(id) => c.launch_resume(id).map(|_| Some(id.to_string())),
                // Never pinned: nothing to resume by id, so start fresh rather
                // than guess with --continue.
                None => c.launch_fresh(None, None),
            });
        match res {
            Ok(id) => {
                st.failures = 0;
                st.last_restore = Some(rfc3339(now));
                if pin.session_id.is_none() {
                    let mut p = pin.clone();
                    p.session_id = id.clone();
                    p.pinned_at = id.as_ref().map(|_| rfc3339(Utc::now()));
                    p.started_at = rfc3339(now);
                    c.save_pin(&p)?;
                }
                c.save_state(&st)?;
                let msg = format!(
                    "watchdog: restored; resumed session {}",
                    id.as_deref().unwrap_or("(none)")
                );
                c.log(&msg);
                r.notes.push(msg);
                r.restored = true;
                drop(_g);
                // The restored session may have lost a line typed before it
                // died: let pending events wake it again.
                self.watch_reset_woken()?;
            }
            Err(e) => {
                st.failures += 1;
                let msg = format!(
                    "watchdog: restore failed ({}/{MAX_FAILURES}): {e}",
                    st.failures
                );
                c.log(&msg);
                r.notes.push(msg);
                if st.failures >= MAX_FAILURES {
                    st.gave_up = true;
                    c.log("watchdog: giving up; recorded a core-down event");
                    r.notes
                        .push("watchdog: giving up; recorded a core-down event".into());
                    c.save_state(&st)?;
                    drop(_g);
                    self.watch_record_event(
                        crate::watch::EventKind::CoreDown,
                        &format!("core-down:{}", pin.session_id.as_deref().unwrap_or("-")),
                        &format!(
                            "the core is down and {MAX_FAILURES} restores failed; see .shards/core/core.log"
                        ),
                        now,
                    )?;
                    return Ok(r);
                }
                c.save_state(&st)?;
            }
        }
        Ok(r)
    }

    /// Keep-warm: ping an idle core with `[HH:MM]` when its last transcript
    /// entry is older than `keep_warm_mins`, at most `keep_warm_quiet_limit`
    /// times without real activity. Also collects usage for replies.
    pub fn core_keep_warm(&self, now: DateTime<Utc>) -> Result<CheckReport> {
        let mut r = CheckReport::default();
        let c = self.core()?;
        let Some(_g) = c.try_lock()? else {
            return Ok(r);
        };
        let mut st = c.state();
        c.collect_usage(&mut st, now)?;
        if let Some(real) = st.scan.last_real.clone() {
            if st.last_real_seen.as_deref() != Some(real.as_str()) {
                st.last_real_seen = Some(real);
                st.quiet_pings = 0;
            }
        }
        let due = st.started
            && c.cfg.keep_warm
            && st.quiet_pings < c.cfg.keep_warm_quiet_limit
            && st
                .scan
                .last_entry
                .as_deref()
                .and_then(parse)
                .is_some_and(|t| secs_between(now, t) >= c.cfg.keep_warm_mins * 60.0)
            && c.alive().1;
        if due {
            let text = heartbeat_text(now);
            c.sent(&mut st, "ping", now, None);
            match c.deliver(&mut st, &text) {
                Ok(l) => {
                    st.quiet_pings += 1;
                    st.last_ping = Some(rfc3339(now));
                    let msg = format!(
                        "keep-warm: ping {}/{}{}",
                        st.quiet_pings,
                        c.cfg.keep_warm_quiet_limit,
                        if l == Landed::Confirmed {
                            ""
                        } else {
                            " (not confirmed in transcript)"
                        }
                    );
                    c.log(&msg);
                    r.notes.push(msg);
                    r.pinged = true;
                }
                Err(e) => {
                    st.awaiting.pop();
                    c.log(&format!("keep-warm: ping failed: {e}"));
                    r.notes.push(format!("keep-warm: ping failed: {e}"));
                }
            }
        }
        c.save_state(&st)?;
        Ok(r)
    }

    /// `mem core check`: one watchdog pass, then keep-warm.
    pub fn core_check(&self, now: DateTime<Utc>) -> Result<CheckReport> {
        let mut r = self.core_watchdog(now)?;
        let k = self.core_keep_warm(now)?;
        r.notes.extend(k.notes);
        r.pinged = k.pinged;
        Ok(r)
    }

    /// Type the wake line into the pane. Called by a watch pass that has
    /// claimed this pending set.
    pub(crate) fn core_pane_wake(
        &self,
        events: usize,
        upto: u64,
        now: DateTime<Utc>,
    ) -> Result<PaneWake> {
        let c = self.core()?;
        let Some(_g) = c.try_lock()? else {
            return Ok(PaneWake::Busy);
        };
        let mut st = c.state();
        if !c.alive().1 {
            return Ok(PaneWake::Down);
        }
        c.collect_usage(&mut st, now)?;
        c.sent(&mut st, "wake", now, Some(events));
        let res = c.deliver(&mut st, &wake_line(events, upto));
        let out = match res {
            Ok(l) => {
                st.last_wake = Some(rfc3339(now));
                c.log(&format!(
                    "wake: {events} event(s) up to #{upto}{}",
                    if l == Landed::Confirmed {
                        ""
                    } else {
                        " (typed; not confirmed in transcript)"
                    }
                ));
                PaneWake::Landed(l)
            }
            Err(e) => {
                st.awaiting.pop();
                c.log(&format!("wake failed: {e}"));
                PaneWake::Failed(e.to_string())
            }
        };
        c.save_state(&st)?;
        Ok(out)
    }

    /// `mem core rinit`: hand off to a fresh session whose first message
    /// carries `baton`. Each step is verified; if one fails, the old session
    /// is left running (resumed if it had already exited) and the error
    /// says which step failed.
    pub fn core_rinit(&self, baton: &str) -> Result<Option<String>> {
        if baton.trim().is_empty() {
            return Err(Error::Core("the baton is empty".into()));
        }
        let c = self.core()?;
        // 1. Lock against wakes (and the watchdog, and keep-warm).
        let _g = c.lock()?;
        let mut st = c.state();
        if !st.started || !c.alive().1 {
            return Err(Error::Core(
                "no running core to hand off from; use mem core start".into(),
            ));
        }
        let mut pin = c.pin();
        let old = pin.session_id.clone();
        if c.adapter.has_sessions() && old.is_none() {
            return Err(Error::Core(
                "the running session has no pinned id, so it couldn't be resumed if the \
                 handoff failed; refusing"
                    .into(),
            ));
        }
        let oldname = old.as_deref().unwrap_or("(no id)").to_string();
        c.log(&format!("rinit: handing off from session {oldname}"));
        // 2-3. Send the exit; verify the pane is back at its shell.
        if !c.exit_command() {
            c.log("rinit: FAILED at exit; the old session is still running");
            return Err(Error::Core(format!(
                "handoff failed: the old session {oldname} did not exit; it is still running"
            )));
        }
        // 4-6. Start the new session, deliver the baton, pin its id.
        match c
            .pane
            .ensure_pane()
            .and_then(|_| c.launch_fresh(Some(baton), old.as_deref()))
        {
            Ok(new) => {
                if let Some(o) = old {
                    pin.previous.push(o);
                }
                pin.session_id = new.clone();
                pin.started_at = rfc3339(Utc::now());
                pin.pinned_at = new.as_ref().map(|_| rfc3339(Utc::now()));
                c.save_pin(&pin)?;
                // Replies still owed by the old session are not counted.
                let _ = c.collect_usage(&mut st, Utc::now());
                st.awaiting.clear();
                st.scan = TranscriptScan::default();
                st.last_counted = 0;
                st.quiet_pings = 0;
                st.failures = 0;
                c.save_state(&st)?;
                c.log(&format!(
                    "rinit: new session {} pinned",
                    new.as_deref().unwrap_or("(no id)")
                ));
                // 7. Unlock; pending events wake the new session.
                drop(_g);
                self.watch_reset_woken()?;
                Ok(new)
            }
            Err(e) => {
                c.log(&format!("rinit: FAILED starting the new session: {e}"));
                c.pane.kill_command();
                let back = c.pane.ensure_pane().and_then(|_| match old.as_deref() {
                    Some(id) => c.launch_resume(id),
                    None => c.launch_fresh(None, None).map(|_| ()),
                });
                match back {
                    Ok(()) => {
                        c.log(&format!("rinit: old session {oldname} resumed"));
                        Err(Error::Core(format!(
                            "handoff failed starting the new session ({e}); the old session \
                             {oldname} was resumed and is still the core"
                        )))
                    }
                    Err(e2) => {
                        c.log(&format!("rinit: resuming the old session failed too: {e2}"));
                        Err(Error::Core(format!(
                            "handoff failed starting the new session ({e}), and resuming the \
                             old session {oldname} failed too ({e2}); the watchdog will retry"
                        )))
                    }
                }
            }
        }
    }

    /// Every usage.jsonl entry (after reading any new replies).
    pub fn core_usage(&self, now: DateTime<Utc>) -> Result<Vec<UsageEntry>> {
        let c = self.core()?;
        if let Some(_g) = c.try_lock()? {
            let mut st = c.state();
            c.collect_usage(&mut st, now)?;
            c.save_state(&st)?;
        }
        let text = fs::read_to_string(c.usage_path()).unwrap_or_default();
        Ok(text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect())
    }

    pub fn core_dir(&self) -> Result<PathBuf> {
        Ok(self.core()?.dir)
    }
}

fn keep_warm_state(
    cfg: &crate::config::CoreConfig,
    st: &CoreState,
    scan: &TranscriptScan,
    now: DateTime<Utc>,
) -> String {
    if !cfg.keep_warm {
        return "off".into();
    }
    if st.quiet_pings >= cfg.keep_warm_quiet_limit {
        return format!(
            "paused after {} quiet pings; resumes on the next real activity",
            st.quiet_pings
        );
    }
    match scan.last_entry.as_deref().and_then(parse) {
        None => "on; no transcript entry yet".into(),
        Some(t) => {
            let idle = secs_between(now, t).max(0.0);
            let due = (cfg.keep_warm_mins * 60.0 - idle).max(0.0);
            format!(
                "on; last entry {} ago, next ping in {}",
                crate::board::human_age(idle as i64),
                crate::board::human_age(due as i64)
            )
        }
    }
}
