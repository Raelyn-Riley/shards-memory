// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! A small wrapper around `tmux` for the panes of the core and the shards.
//!
//! The core gets its **own tmux server** (`tmux -S <socket>`), and the shards
//! share one more, so nothing the operator does in their own tmux can touch
//! them and `kill-server` only ever kills that server. Each shard is its own
//! tmux session (one window) on the shards server. A server is started with
//! `-f /dev/null`, so a personal `~/.tmux.conf` can't change how a pane
//! behaves.
//!
//! The pane runs a plain `/bin/sh`. The agent's command is *typed into that
//! shell*, so when the command exits the pane is still there, back at the
//! shell. That is what lets a handoff check that the old session really
//! exited before it starts the new one in the same pane.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use crate::error::{Error, Result};

/// Is `tmux` on PATH?
pub fn available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// One tmux session on its own socket.
#[derive(Debug, Clone)]
pub struct Tmux {
    pub socket: PathBuf,
    pub session: String,
}

/// What the pane is doing right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    /// The pane's shell.
    pub pid: u32,
    /// The shell itself has exited.
    pub dead: bool,
}

fn tmux_err(args: &[&str], out: &Output) -> Error {
    Error::Core(format!(
        "tmux {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    ))
}

impl Tmux {
    pub fn new(socket: impl Into<PathBuf>, session: impl Into<String>) -> Tmux {
        Tmux {
            socket: socket.into(),
            session: session.into(),
        }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new("tmux");
        c.arg("-S")
            .arg(&self.socket)
            .args(args)
            // Inside the operator's own tmux, TMUX would point elsewhere.
            .env_remove("TMUX")
            .stdin(Stdio::null());
        c
    }

    fn output(&self, args: &[&str]) -> Result<Output> {
        self.cmd(args)
            .output()
            .map_err(|e| Error::Core(format!("could not run tmux: {e}")))
    }

    fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.output(args)?;
        if !out.status.success() {
            return Err(tmux_err(args, &out));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn target(&self) -> String {
        // `=name` matches the session name exactly, not as a prefix.
        format!("={}:", self.session)
    }

    /// Is a tmux server listening on this socket?
    pub fn server_running(&self) -> bool {
        self.output(&["list-sessions"])
            .is_ok_and(|o| o.status.success())
    }

    /// Start the server (with its first session) in its own transient
    /// systemd user scope, so it outlives whatever ran `mem`. Without that,
    /// a server started from a service (Parlor, say) sits in the service's
    /// cgroup, and restarting the service kills every pane on it. False if
    /// there is no systemd user manager to ask, or
    /// `SHARDS_TMUX_SCOPE=0`; the caller then starts it the plain way.
    fn new_server_in_scope(&self, args: &[&str]) -> bool {
        if std::env::var("SHARDS_TMUX_SCOPE").is_ok_and(|v| v == "0")
            || std::env::var_os("XDG_RUNTIME_DIR").is_none()
        {
            return false;
        }
        Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "--quiet",
                "--collect",
                "--",
                "tmux",
                "-S",
            ])
            .arg(&self.socket)
            .args(args)
            .env_remove("TMUX")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    /// Does the session exist?
    pub fn has_session(&self) -> bool {
        self.output(&["has-session", "-t", &self.target()])
            .is_ok_and(|o| o.status.success())
    }

    /// Create the session with a shell in `cwd`, a wide window (so a typed
    /// line never wraps) and `env` set for the shell.
    pub fn new_session(&self, cwd: &Path, env: &[(String, String)]) -> Result<()> {
        if let Some(p) = self.socket.parent() {
            std::fs::create_dir_all(p)?;
        }
        let cwd = cwd.to_string_lossy();
        let mut args: Vec<String> = [
            "-f",
            "/dev/null",
            "new-session",
            "-d",
            "-s",
            &self.session,
            "-x",
            "220",
            "-y",
            "50",
            "-c",
            &cwd,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // Set through `env` in the pane's command: tmux's `-e` doesn't reach
        // every variable (dash resets PS1 from it).
        let mut line = String::from("exec env");
        for (k, v) in env {
            line.push(' ');
            line.push_str(&crate::coreadapter::shell_quote(&format!("{k}={v}")));
        }
        line.push_str(" /bin/sh");
        args.push(line);
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        if !self.server_running() && self.new_server_in_scope(&refs) && self.has_session() {
            // Started in its own scope.
        } else {
            self.run(&refs)?;
        }
        // Keep a dead shell's pane around, so status can say so.
        let _ = self.run(&["set-option", "-t", &self.target(), "remain-on-exit", "on"]);
        let _ = self.run(&["set-option", "-t", &self.target(), "history-limit", "5000"]);
        Ok(())
    }

    /// The first pane of the session, if any.
    pub fn pane(&self) -> Option<Pane> {
        let out = self
            .run(&[
                "list-panes",
                "-t",
                &self.target(),
                "-F",
                "#{pane_pid} #{pane_dead}",
            ])
            .ok()?;
        let line = out.lines().next()?;
        let mut it = line.split_whitespace();
        let pid = it.next()?.parse().ok()?;
        let dead = it.next() == Some("1");
        Some(Pane { pid, dead })
    }

    /// Type `text` literally (no key names), without Enter.
    pub fn type_text(&self, text: &str) -> Result<()> {
        self.run(&["send-keys", "-t", &self.target(), "-l", "--", text])
            .map(|_| ())
    }

    /// Press Enter. Sent as its own tmux call, after the text, so a TUI that
    /// treats a fast burst as a paste still sees a real key press.
    pub fn enter(&self) -> Result<()> {
        self.run(&["send-keys", "-t", &self.target(), "Enter"])
            .map(|_| ())
    }

    /// The visible pane text, with wrapped lines joined.
    pub fn capture(&self) -> Result<String> {
        self.run(&["capture-pane", "-p", "-J", "-t", &self.target()])
    }

    /// Poll the pane until `text` is visible or `timeout` passes.
    pub fn wait_for_text(&self, text: &str, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            if self.capture().is_ok_and(|c| c.contains(text)) {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Kill this session only. The server exits by itself once it has no
    /// sessions left.
    pub fn kill_session(&self) {
        let _ = self.output(&["kill-session", "-t", &self.target()]);
    }

    /// Kill the whole tmux server on this socket (every session on it).
    pub fn kill_server(&self) {
        let _ = self.output(&["kill-server"]);
        let _ = std::fs::remove_file(&self.socket);
    }

    /// The command a person runs to attach.
    pub fn attach_command(&self) -> Vec<String> {
        vec![
            "tmux".into(),
            "-S".into(),
            self.socket.to_string_lossy().into_owned(),
            "attach".into(),
            "-t".into(),
            self.target(),
        ]
    }
}

/// Child processes of `pid` (Linux `/proc`; elsewhere `ps`).
pub fn children(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir("/proc") {
        for e in rd.filter_map(|e| e.ok()) {
            let Some(child) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else {
                continue;
            };
            // pid (comm) state ppid ...; comm may hold spaces and parens.
            let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
                continue;
            };
            let mut f = rest.split_whitespace();
            let state = f.next();
            let ppid = f.next().and_then(|p| p.parse::<u32>().ok());
            if ppid == Some(pid) && state != Some("Z") {
                out.push(child);
            }
        }
        return out;
    }
    if let Ok(o) = Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,stat="])
        .output()
    {
        for line in String::from_utf8_lossy(&o.stdout).lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() >= 3 && f[1].parse() == Ok(pid) && !f[2].starts_with('Z') {
                if let Ok(c) = f[0].parse() {
                    out.push(c);
                }
            }
        }
    }
    out
}
