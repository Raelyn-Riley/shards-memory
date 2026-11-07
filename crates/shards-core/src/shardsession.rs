// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! A shard is a whole agent session in its own tmux pane, the same shape as
//! the core.
//!
//! `mem shard start` opens the shard (branch and worktree, as
//! `mem shard open` does), then starts `[shards] command` in a pane whose
//! shell sits in the shard's worktree. Every shard is its own tmux session
//! (one window) on **one** shards tmux server (`[shards] socket`), so two
//! people can attach to two shards at once without switching each other's
//! window.
//!
//! The session id is pinned before the session starts: the adapter picks
//! the id (`claude --session-id <uuid>`), so even a blank session that has
//! not written a transcript yet is pinned. It is recorded with the shard in
//! `.shards/shards/<id>.json`. `mem shard stop` ends the session and keeps
//! the branch and worktree; `mem shard start --id <id> --resume` brings the
//! pinned session back by its explicit id (`--resume <id>`, never
//! `--continue`). Nothing stops a pane for being idle or stale; it stays up
//! until someone stops it or a clean dinit closes the shard.
//!
//! For now a shard starts **blank**: no soul, no profile, no first message.
//! [`Memory::shard_start`] has one place, [`ShardLaunch`], where an init
//! will plug in later.
//!
//! **Folder trust.** Claude Code asks "do you trust this folder?" the first
//! time it runs in a folder, which would stop every new worktree. It keeps
//! that answer per *canonical* git root, though, and a shard's worktree is a
//! worktree of the memory, so trusting the memory root once covers every
//! shard. `mem shard start` checks that (reading Claude Code's config, never
//! writing it) and refuses with instructions if not. If a trust question
//! shows anyway, the start fails at once instead of hanging.

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use serde::{Deserialize, Serialize};

use crate::coreadapter::{adapter_for, CoreAdapter, Launch, Screen, TranscriptScan};
use crate::coresession::Landed;
use crate::error::{Error, Result};
use crate::lock::FileMutex;
use crate::memory::Memory;
use crate::pane::PaneRunner;
use crate::shard::ShardMeta;
use crate::tmux::{self, Tmux};

/// The pane shell's prompt.
const PS1: &str = "shard$ ";

fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// A shard's pinned session, recorded with the shard.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShardSession {
    pub adapter: String,
    /// `None` for an adapter without session ids.
    pub session_id: Option<String>,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stopped_at: Option<String>,
}

/// Is the shard's pane up?
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum PaneState {
    /// The pane is up and the agent is running in it.
    Up,
    /// The pane is up but back at its shell: the agent exited.
    Shell,
    /// No pane.
    Down,
}

impl PaneState {
    pub fn as_str(&self) -> &'static str {
        match self {
            PaneState::Up => "up",
            PaneState::Shell => "shell",
            PaneState::Down => "down",
        }
    }
}

/// What `mem shard start` did.
#[derive(Debug, Clone)]
pub enum ShardStart {
    AlreadyRunning {
        meta: ShardMeta,
    },
    Started {
        meta: ShardMeta,
        /// Brought back a pinned session that had a transcript.
        resumed: bool,
        /// Something worth telling the person.
        note: Option<String>,
    },
}

/// How a shard's session is started. Today only [`ShardLaunch::Blank`];
/// an init (soul, profile, first message) will be a second variant here.
#[derive(Debug, Clone, Copy)]
pub enum ShardLaunch {
    /// No first message.
    Blank,
}

/// The tmux session name for a shard (tmux turns `.` into `_`, which shard
/// ids may hold too, so `.` becomes `~`, which ids can't hold).
fn tmux_name(id: &str) -> String {
    id.replace('.', "~")
}

/// Everything needed to drive one shard's pane.
struct ShardPane {
    pane: PaneRunner,
    adapter: Box<dyn CoreAdapter>,
    command: String,
    worktree: PathBuf,
}

impl ShardPane {
    fn state(&self) -> PaneState {
        match self.pane.alive() {
            (true, true) => PaneState::Up,
            (true, false) => PaneState::Shell,
            _ => PaneState::Down,
        }
    }

    /// Type the launch line and wait until the agent is ready for a
    /// message. A trust (or other) question on the screen fails at once.
    fn launch(&self, launch: &Launch) -> Result<Option<String>> {
        self.pane.ensure_pane().map_err(session_err)?;
        let line = self.adapter.launch_line(&self.command, launch);
        self.pane.type_line(&line).map_err(session_err)?;
        let timeout = self.pane.start_timeout;
        if !self.pane.wait_alive(timeout) {
            return Err(Error::Session(
                "the session's command did not start (or exited)".into(),
            ));
        }
        let start = Instant::now();
        loop {
            let screen = self.pane.tmux.capture().unwrap_or_default();
            match self.adapter.screen(&screen) {
                Screen::Ready => return Ok(None),
                Screen::Blocked(why) => {
                    return Err(Error::Session(format!(
                        "{why}; nobody is there to answer, so the session was stopped"
                    )))
                }
                Screen::Unknown => {}
            }
            if !self.pane.alive().1 {
                return Err(Error::Session("the session exited while starting".into()));
            }
            if start.elapsed() >= timeout {
                return Ok(Some(
                    "the session is running, but its screen never showed a ready prompt".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Exit the agent (or kill it) and close the pane. True if anything
    /// was running.
    fn end(&self) -> bool {
        if !self.pane.tmux.has_session() {
            return false;
        }
        if !self.pane.exit_command(self.adapter.exit_text()) {
            self.pane.kill_command();
        }
        self.pane.tmux.kill_session();
        true
    }
}

fn session_err(e: Error) -> Error {
    match e {
        Error::Core(m) => Error::Session(m),
        e => e,
    }
}

/// A message typed into a pane is one line: Enter would send it early. A
/// blank line between paragraphs becomes ` | ` and any other line break a
/// space (the same rule Parlor uses).
pub fn one_line(text: &str) -> String {
    let mut paras: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();
    for l in text.lines() {
        if l.trim().is_empty() {
            if !cur.is_empty() {
                paras.push(cur.join(" "));
                cur.clear();
            }
        } else {
            cur.push(l.trim_end());
        }
    }
    if !cur.is_empty() {
        paras.push(cur.join(" "));
    }
    paras.join(" | ").trim().to_string()
}

/// The environment a shard's pane gives its shell and agent, on top of what
/// the shards tmux server inherited.
///
/// - `SHARDS_MEMORY` is the shard's **worktree**, not the main root, so
///   `mem set`, `supersede` and `put` commit to `shard/<id>` and reads show
///   the shard's own branch. Commands that need main (`dinit`, `board`,
///   `shard ...`, `core ...`, `quirk ...`) find it from the worktree.
/// - `SHARDS_AUTHOR` is the shard id and `SHARDS_HUMAN` is cleared, so a
///   value inherited from whoever started the server can't stand in for
///   them: a write in the worktree is stamped with the shard and the
///   shard's own human from its record.
pub fn shard_pane_env(meta: &ShardMeta) -> Vec<(String, String)> {
    vec![
        (
            "SHARDS_MEMORY".to_string(),
            meta.worktree.to_string_lossy().into_owned(),
        ),
        ("SHARDS_SHARD".to_string(), meta.id.clone()),
        ("SHARDS_AUTHOR".to_string(), meta.id.clone()),
        ("SHARDS_HUMAN".to_string(), String::new()),
        ("PS1".to_string(), PS1.to_string()),
        ("ENV".to_string(), String::new()),
    ]
}

impl Memory {
    fn shard_pane(&self, meta: &ShardMeta) -> Result<ShardPane> {
        let main = self.main()?;
        let cfg = main.config().clone();
        let root = main.main_root().to_path_buf();
        let socket = {
            let p = PathBuf::from(&cfg.shards.socket);
            if p.is_absolute() {
                p
            } else {
                root.join(p)
            }
        };
        Ok(ShardPane {
            pane: PaneRunner {
                tmux: Tmux::new(socket, tmux_name(&meta.id)),
                cwd: meta.worktree.clone(),
                env: shard_pane_env(meta),
                ps1: PS1.to_string(),
                start_timeout: cfg.core.start_timeout(),
                exit_timeout: cfg.core.exit_timeout(),
                land_timeout: cfg.core.land_timeout(),
            },
            adapter: adapter_for(cfg.shards.adapter, &cfg, &root),
            command: cfg.shards.command.clone(),
            worktree: meta.worktree.clone(),
        })
    }

    fn sessions_dir(&self) -> PathBuf {
        self.state_dir().join("sessions")
    }

    /// Serializes start, stop and send for one shard.
    fn session_lock(&self, id: &str) -> Result<FileMutex> {
        FileMutex::lock(&self.sessions_dir().join(format!("{id}.lock")))
    }

    fn session_log(&self, id: &str, msg: &str) {
        let line = format!("{} {id}: {msg}\n", now());
        let _ = fs::create_dir_all(self.sessions_dir());
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.sessions_dir().join("sessions.log"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    }

    /// Change the shard's record under its shard lock, so a dinit can't
    /// write over it.
    fn update_session_pin(&self, id: &str, f: impl FnOnce(&mut ShardSession)) -> Result<()> {
        let main = self.main()?;
        let human = main.shard_meta(id)?.human;
        let _g = main.lock_shard(id, id, human.as_deref())?;
        let mut meta = main.shard_meta(id)?;
        let mut pin = meta.session.take().unwrap_or_default();
        f(&mut pin);
        meta.session = Some(pin);
        main.save_meta(&meta)
    }

    /// The first free `s<n>`: no open shard and no kept branch by that name.
    pub fn next_shard_id(&self) -> Result<String> {
        let main = self.main()?;
        for n in 1.. {
            let id = format!("s{n}");
            let branch = format!("refs/heads/shard/{id}");
            if main.shard_meta(&id).is_err()
                && !main
                    .git
                    .probe(&["rev-parse", "--verify", "--quiet", &branch])?
            {
                return Ok(id);
            }
        }
        unreachable!()
    }

    /// Is the shard's pane up?
    pub fn shard_pane_state(&self, id: &str) -> PaneState {
        self.shard_meta(id)
            .and_then(|m| self.shard_pane(&m))
            .map(|p| p.state())
            .unwrap_or(PaneState::Down)
    }

    /// The shard's pinned session's transcript, once it has one.
    pub fn shard_transcript(&self, id: &str) -> Result<Option<PathBuf>> {
        let meta = self.shard_meta(id)?;
        let sp = self.shard_pane(&meta)?;
        Ok(meta
            .session
            .and_then(|s| s.session_id)
            .and_then(|sid| sp.adapter.transcript(&sp.worktree, &sid)))
    }

    fn check_trust(&self, sp: &ShardPane) -> Result<()> {
        let main = self.main()?;
        let root = main.main_root();
        if sp.adapter.workspace_trusted(root) == Some(false) {
            return Err(Error::Session(format!(
                "Claude Code doesn't trust {} yet, so a shard's session would stop at its \
                 \"trust this folder\" question. Trust the memory root once (run `claude` there \
                 and answer, or `mem core attach` if the core runs there); every shard's \
                 worktree inherits it",
                root.display()
            )));
        }
        Ok(())
    }

    /// `mem shard start`: open a shard and start its session in its own
    /// pane, or with `resume`, bring an open shard's pinned session back.
    /// A no-op for a shard whose session is running.
    pub fn shard_start(
        &self,
        id: Option<&str>,
        human: Option<&str>,
        resume: bool,
        launch: ShardLaunch,
    ) -> Result<ShardStart> {
        if !tmux::available() {
            return Err(Error::Session("tmux is not installed".into()));
        }
        let main = self.main()?;
        if resume {
            let id = id.ok_or_else(|| Error::Session("--resume needs --id".into()))?;
            return main.shard_resume(id, launch);
        }
        // Check trust before opening anything, so a refusal leaves no shard.
        let probe = ShardMeta {
            id: "probe".into(),
            human: None,
            branch: String::new(),
            base: String::new(),
            opened: String::new(),
            worktree: main.main_root().to_path_buf(),
            soul_pending: Vec::new(),
            soul_commit: None,
            session: None,
        };
        main.check_trust(&main.shard_pane(&probe)?)?;
        let meta = match id {
            Some(id) => main.shard_open(id, human)?,
            // Two starts at once may pick the same free id; the loser
            // takes the next one.
            None => {
                let mut tries = 0;
                loop {
                    let id = main.next_shard_id()?;
                    match main.shard_open(&id, human) {
                        Err(Error::ShardExists(_)) if tries < 5 => tries += 1,
                        r => break r?,
                    }
                }
            }
        };
        let id = meta.id.clone();
        let _g = main.session_lock(&id)?;
        let sp = main.shard_pane(&meta)?;
        let sid = sp.adapter.new_session_id();
        main.update_session_pin(&id, |p| {
            *p = ShardSession {
                adapter: sp.adapter.name().to_string(),
                session_id: sid.clone(),
                started_at: now(),
                resumed_at: None,
                stopped_at: None,
            }
        })?;
        let ShardLaunch::Blank = launch;
        match sp.launch(&Launch::Blank { id: sid.as_deref() }) {
            Ok(note) => {
                main.session_log(
                    &id,
                    &format!(
                        "started blank session {}",
                        sid.as_deref().unwrap_or("(no id)")
                    ),
                );
                Ok(ShardStart::Started {
                    meta: main.shard_meta(&id)?,
                    resumed: false,
                    note,
                })
            }
            Err(e) => {
                sp.end();
                main.session_log(&id, &format!("start failed: {e}"));
                Err(Error::Session(format!(
                    "shard {id} is open (branch and worktree kept), but its session did not \
                     come up: {e}. Fix that, then: mem shard start --id {id} --resume"
                )))
            }
        }
    }

    fn shard_resume(&self, id: &str, launch: ShardLaunch) -> Result<ShardStart> {
        let meta = self.shard_meta(id)?;
        let _g = self.session_lock(id)?;
        let sp = self.shard_pane(&meta)?;
        if sp.state() == PaneState::Up {
            return Ok(ShardStart::AlreadyRunning { meta });
        }
        self.check_trust(&sp)?;
        let pinned = meta.session.as_ref().and_then(|s| s.session_id.clone());
        // A pinned session that never got a message has no transcript, so
        // there is nothing to resume: start it blank again under the same
        // id. With no pin at all (a shard from `mem shard open`), pin one.
        let (launch_as, sid, resumed, mut note) = match pinned {
            Some(sid) if sp.adapter.transcript(&sp.worktree, &sid).is_some() => {
                (Some(sid.clone()), Some(sid), true, None)
            }
            Some(sid) => (
                None,
                Some(sid),
                false,
                Some(
                    "its session had no messages yet, so it started blank again under the same id"
                        .to_string(),
                ),
            ),
            None => (
                None,
                sp.adapter.new_session_id(),
                false,
                Some(
                    "it had no pinned session, so a new blank one was started and pinned"
                        .to_string(),
                ),
            ),
        };
        let ShardLaunch::Blank = launch;
        let res = match &launch_as {
            Some(sid) => sp.launch(&Launch::Resume { id: sid }),
            None => sp.launch(&Launch::Blank { id: sid.as_deref() }),
        };
        match res {
            Ok(n) => {
                let fresh_pin = meta.session.is_none() || sid.is_none();
                self.update_session_pin(id, |p| {
                    if fresh_pin || p.session_id != sid {
                        *p = ShardSession {
                            adapter: sp.adapter.name().to_string(),
                            session_id: sid.clone(),
                            started_at: now(),
                            ..Default::default()
                        };
                    } else {
                        p.resumed_at = Some(now());
                    }
                    p.stopped_at = None;
                })?;
                self.session_log(
                    id,
                    &format!(
                        "{} session {}",
                        if resumed {
                            "resumed"
                        } else {
                            "restarted blank"
                        },
                        sid.as_deref().unwrap_or("(no id)")
                    ),
                );
                if n.is_some() {
                    note = n;
                }
                Ok(ShardStart::Started {
                    meta: self.shard_meta(id)?,
                    resumed,
                    note,
                })
            }
            Err(e) => {
                sp.end();
                self.session_log(id, &format!("resume failed: {e}"));
                Err(e)
            }
        }
    }

    /// `mem shard stop`: end the session cleanly (`/exit`), close its pane,
    /// keep the branch, worktree and pin. True if it was running.
    pub fn shard_stop(&self, id: &str) -> Result<bool> {
        let meta = self.shard_meta(id)?;
        let _g = self.session_lock(id)?;
        let was = self.shard_pane(&meta)?.end();
        if meta.session.is_some() {
            self.update_session_pin(id, |p| p.stopped_at = Some(now()))?;
        }
        self.session_log(
            id,
            if was {
                "stopped"
            } else {
                "stop: was not running"
            },
        );
        Ok(was)
    }

    /// End a closing shard's session (a clean dinit, which holds the shard
    /// lock and deletes the record, so nothing is written here).
    pub(crate) fn shard_session_end(&self, meta: &ShardMeta) -> bool {
        let Ok(sp) = self.shard_pane(meta) else {
            return false;
        };
        let _g = self.session_lock(&meta.id).ok();
        let was = sp.end();
        if was {
            self.session_log(&meta.id, "stopped by a clean dinit");
        }
        let _ = fs::remove_file(self.sessions_dir().join(format!("{}.lock", meta.id)));
        was
    }

    /// `mem shard send`: type one message into the shard's pane and confirm
    /// it reached its pinned session's transcript. Refuses unless the agent
    /// is running and waiting (typed into a bare shell, text would run as a
    /// command).
    pub fn shard_send(&self, id: &str, text: &str) -> Result<Landed> {
        let meta = self.shard_meta(id)?;
        let line = one_line(text);
        if line.is_empty() {
            return Err(Error::Session("the message is empty".into()));
        }
        let _g = self.session_lock(id)?;
        let sp = self.shard_pane(&meta)?;
        match sp.state() {
            PaneState::Up => {}
            PaneState::Shell => {
                return Err(Error::Session(format!(
                    "shard {id}'s session isn't running (its pane is at a shell); \
                     mem shard start --id {id} --resume"
                )))
            }
            PaneState::Down => {
                return Err(Error::Session(format!(
                    "shard {id}'s pane is down; mem shard start --id {id} --resume"
                )))
            }
        }
        if let Screen::Blocked(why) = sp
            .adapter
            .screen(&sp.pane.tmux.capture().unwrap_or_default())
        {
            return Err(Error::Session(format!("not sent: {why}")));
        }
        let sid = meta.session.and_then(|s| s.session_id);
        let path = || {
            sid.as_deref()
                .and_then(|s| sp.adapter.transcript(&sp.worktree, s))
        };
        let mut scan = TranscriptScan::default();
        if let Some(p) = path() {
            scan.update(&p);
        }
        let before = (scan.prompts, scan.queued);
        sp.pane.type_message(&line).map_err(session_err)?;
        if sid.is_none() {
            return Ok(Landed::Unconfirmed);
        }
        let start = Instant::now();
        // The first message of a blank session creates its transcript.
        let wait = sp.pane.land_timeout.max(Duration::from_secs(10));
        loop {
            if let Some(p) = path() {
                scan.update(&p);
                if scan.landed_since(before) {
                    return Ok(Landed::Confirmed);
                }
            }
            if start.elapsed() >= wait {
                return Ok(Landed::Unconfirmed);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn shard_attach_command(&self, id: &str) -> Result<Vec<String>> {
        let meta = self.shard_meta(id)?;
        Ok(self.shard_pane(&meta)?.pane.tmux.attach_command())
    }

    /// Attach this terminal to the shard's pane (detach with the tmux
    /// prefix then `d`; the session keeps running).
    pub fn shard_attach(&self, id: &str) -> Result<()> {
        let meta = self.shard_meta(id)?;
        let sp = self.shard_pane(&meta)?;
        if !sp.pane.tmux.has_session() {
            return Err(Error::Session(format!(
                "shard {id}'s pane is down; mem shard start --id {id} --resume"
            )));
        }
        let cmd = sp.pane.tmux.attach_command();
        let status = std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .env_remove("TMUX")
            .status()?;
        if !status.success() {
            return Err(Error::Session(format!("tmux attach exited with {status}")));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_rule() {
        assert_eq!(one_line("hi\nthere\n\nnext para\n"), "hi there | next para");
        assert_eq!(one_line("  \n\n"), "");
        assert_eq!(tmux_name("s1.v2"), "s1~v2");
    }
}
