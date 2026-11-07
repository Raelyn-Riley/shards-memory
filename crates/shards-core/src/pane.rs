// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Driving one agent session in one tmux pane: the steps the core and every
//! shard share.
//!
//! The pane runs a plain `/bin/sh`, and the agent's command is typed into it
//! (see [`crate::tmux`]). So "the pane is up" and "the command is alive" are
//! two different things: a pane whose shell has no child is back at its
//! prompt, and anything typed there would run as a shell command.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::error::{Error, Result};
use crate::tmux::{self, Tmux};

/// One pane on its own tmux session, with the shell's environment and the
/// timeouts that bound every step.
#[derive(Debug, Clone)]
pub struct PaneRunner {
    pub tmux: Tmux,
    /// Where the pane's shell starts.
    pub cwd: PathBuf,
    /// Set for the pane's shell (`PS1` among them).
    pub env: Vec<(String, String)>,
    /// The shell prompt, waited for when the pane is created.
    pub ps1: String,
    pub start_timeout: Duration,
    pub exit_timeout: Duration,
    pub land_timeout: Duration,
}

/// Remove whitespace and box-drawing borders, so a line an input box wrapped
/// over several rows can still be found in a capture.
fn squash(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_whitespace() && !matches!(c, '│' | '┃' | '|'))
        .collect()
}

impl PaneRunner {
    /// (pane up, command alive).
    pub fn alive(&self) -> (bool, bool) {
        match self.tmux.pane() {
            Some(p) if !p.dead => (true, !tmux::children(p.pid).is_empty()),
            _ => (false, false),
        }
    }

    /// A pane at a fresh shell, ready for a launch line, creating it if
    /// needed. Leaves a running command alone. A pane that is back at its
    /// shell, or whose shell died, gets a new shell: a command that was
    /// killed (rather than told to exit) leaves the terminal raw, and the
    /// old shell can't be typed into reliably.
    pub fn ensure_pane(&self) -> Result<()> {
        match self.tmux.pane() {
            Some(p) if !p.dead && !tmux::children(p.pid).is_empty() => return Ok(()),
            Some(_) => self.tmux.respawn(&self.cwd, &self.env)?,
            None => self.tmux.new_session(&self.cwd, &self.env)?,
        }
        if !self.tmux.wait_for_text(&self.ps1, Duration::from_secs(5)) {
            return Err(Error::Core("the pane's shell did not come up".into()));
        }
        Ok(())
    }

    /// Wait until the pane's command is running and still running a moment
    /// later.
    pub fn wait_alive(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        let settle = Duration::from_millis(700);
        loop {
            if self.alive().1 {
                std::thread::sleep(settle);
                if self.alive().1 {
                    return true;
                }
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait until the command has exited and the pane is back at its shell.
    pub fn wait_exited(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            let (up, alive) = self.alive();
            if up && !alive {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Type `text`, check it shows in the pane (so it wasn't collapsed into
    /// a paste placeholder), then press Enter as a separate call.
    pub fn type_line(&self, text: &str) -> Result<()> {
        let count = |c: &str| c.matches(text).count();
        let before = self.tmux.capture().map(|c| count(&c)).unwrap_or(0);
        self.tmux.type_text(text)?;
        let start = Instant::now();
        loop {
            if self.tmux.capture().map(|c| count(&c)).unwrap_or(0) > before {
                break;
            }
            if start.elapsed() >= self.land_timeout {
                return Err(Error::Core(
                    "the typed line did not show in the pane; Enter not pressed".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.tmux.enter()
    }

    /// Like [`type_line`](Self::type_line), for a person's message, which
    /// may be long: an input box wraps it over several rows, and a long
    /// paste may collapse into a `[Pasted text …]` placeholder. So the check
    /// is that the message's last 40 characters (ignoring whitespace and
    /// box borders) or a placeholder newly show.
    pub fn type_message(&self, text: &str) -> Result<()> {
        let tail: String = {
            let s = squash(text);
            let n = s.chars().count();
            s.chars().skip(n.saturating_sub(40)).collect()
        };
        let count = |c: &str| {
            let c = squash(c);
            c.matches(tail.as_str()).count() + c.matches("[Pastedtext").count()
        };
        let before = self.tmux.capture().map(|c| count(&c)).unwrap_or(0);
        self.tmux.type_text(text)?;
        let start = Instant::now();
        loop {
            if self.tmux.capture().map(|c| count(&c)).unwrap_or(0) > before {
                break;
            }
            if start.elapsed() >= self.land_timeout {
                return Err(Error::Core(
                    "the typed message did not show in the pane; Enter not pressed".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        self.tmux.enter()
    }

    /// Tell the running command to exit with `exit_text`; true once the pane
    /// is back at its shell.
    pub fn exit_command(&self, exit_text: &str) -> bool {
        if !self.alive().1 {
            return true;
        }
        if self.type_line(exit_text).is_err() {
            return false;
        }
        self.wait_exited(self.exit_timeout)
    }

    /// Kill whatever the pane's shell is running (a last resort).
    pub fn kill_command(&self) {
        if let Some(p) = self.tmux.pane() {
            for c in tmux::children(p.pid) {
                let _ = std::process::Command::new("kill")
                    .args(["-TERM", &c.to_string()])
                    .status();
            }
        }
        self.wait_exited(Duration::from_secs(3));
    }
}

#[cfg(test)]
mod tests {
    use super::squash;

    #[test]
    fn squash_joins_wrapped_rows() {
        let box_ = "│ > tell me about the\n│   heron feeder  │";
        assert!(squash(box_).contains(&squash("about the heron feeder")));
    }
}
