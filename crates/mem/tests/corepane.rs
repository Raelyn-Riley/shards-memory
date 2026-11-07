// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Brief 11: the core's pane finds `claude` the way a shard's pane does.
//!
//! `[core] command` is left at its default, `claude`. That `claude` is a
//! stand-in found only in `$HOME/.local/bin` and `mem` runs with a bare PATH,
//! as on a fresh machine. The stand-in logs the PATH it was started with,
//! then runs the fake core from `shards_core::sample`, so no model is called.
//! `mem core start`, the watchdog's restore (`mem core check`) and `mem core
//! rinit` must each start it. Without tmux the tests pass with a note.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use shards_core::sample::write_fake_core;
use shards_core::tmux::{self, Tmux};
use tempfile::TempDir;

/// The launcher's PATH: system dirs only, no `~/.local/bin`, no `mem`.
const BARE_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// `claude` in `$HOME/.local/bin`: log the PATH, then be the fake core.
const STAND_IN: &str = r#"#!/bin/sh
echo "$PATH" >> @CTL@/paths.log
exec sh @FAKE@ @TX@ @CTL@ "$@"
"#;

struct World {
    tmp: TempDir,
    m: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        self.tmux().kill_server();
    }
}

impl World {
    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }
    fn ctl(&self) -> PathBuf {
        self.tmp.path().join("ctl")
    }
    fn tmux(&self) -> Tmux {
        Tmux::new(self.tmp.path().join("core.sock"), "core")
    }

    /// `mem` as a person at a terminal on a fresh machine runs it.
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_mem"));
        c.args(args)
            .current_dir(if self.m.exists() {
                &self.m
            } else {
                self.tmp.path()
            })
            .env_clear()
            .env("PATH", BARE_PATH)
            .env("HOME", self.home())
            .env("SHARDS_AUTHOR", "core")
            .env("SHARDS_HUMAN", "Ada")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("SHARDS_TMUX_SCOPE", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        c
    }
    fn mem(&self, args: &[&str]) -> Output {
        self.cmd(args).output().unwrap()
    }

    /// The PATH of each `claude` started so far.
    fn paths(&self) -> Vec<String> {
        std::fs::read_to_string(self.ctl().join("paths.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }

    /// Each launch's PATH has `~/.local/bin` and `mem`'s directory.
    fn assert_paths(&self, n: usize) {
        let paths = self.paths();
        assert_eq!(paths.len(), n, "{paths:?}");
        let local = self.home().join(".local/bin");
        let mem_dir = Path::new(env!("CARGO_BIN_EXE_mem")).parent().unwrap();
        for p in &paths {
            assert!(p.contains(&*local.to_string_lossy()), "{p}");
            assert!(p.contains(&*mem_dir.to_string_lossy()), "{p}");
        }
    }

    fn start(&self) -> String {
        let out = ok(&self.mem(&["core", "start"]));
        assert!(out.contains("core started in its pane; session "), "{out}");
        self.assert_paths(1);
        self.session_id()
    }

    fn session_id(&self) -> String {
        let st: serde_json::Value =
            serde_json::from_str(&ok(&self.mem(&["core", "status", "--json"]))).unwrap();
        assert_eq!(st["command_alive"], true, "{st}");
        st["session_id"].as_str().unwrap().to_string()
    }

    /// Kill the core's command, leaving the pane at its shell.
    fn kill_command(&self) {
        let pane = self.tmux().pane().expect("pane");
        for c in tmux::children(pane.pid) {
            Command::new("kill")
                .args(["-KILL", &c.to_string()])
                .status()
                .unwrap();
        }
        let t = Instant::now();
        while !tmux::children(pane.pid).is_empty() {
            assert!(t.elapsed() < Duration::from_secs(2), "command did not die");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn starts_log(&self) -> String {
        std::fs::read_to_string(self.ctl().join("starts.log")).unwrap_or_default()
    }
}

fn ok(o: &Output) -> String {
    assert!(
        o.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn world() -> Option<World> {
    if !tmux::available() {
        eprintln!("tmux is not installed; skipping");
        return None;
    }
    let tmp = TempDir::new().unwrap();
    let m = tmp.path().join("m");
    let w = World { m: m.clone(), tmp };
    std::fs::create_dir_all(w.ctl()).unwrap();
    let bin = w.home().join(".local").join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let fake = write_fake_core(&w.tmp.path().join("fake")).unwrap();
    let tx = w.tmp.path().join("tx");
    let claude = STAND_IN
        .replace("@CTL@", &w.ctl().to_string_lossy())
        .replace("@FAKE@", &fake.to_string_lossy())
        .replace("@TX@", &tx.to_string_lossy());
    std::fs::write(bin.join("claude"), claude).unwrap();
    Command::new("chmod")
        .arg("+x")
        .arg(bin.join("claude"))
        .status()
        .unwrap();

    ok(&w.mem(&["init", m.to_str().unwrap()]));
    let root = std::fs::canonicalize(&m).unwrap();
    let cc = w.tmp.path().join("claude.json");
    std::fs::write(
        &cc,
        serde_json::json!({"projects": {root.to_string_lossy(): {"hasTrustDialogAccepted": true}}})
            .to_string(),
    )
    .unwrap();
    // `[core] command` is left at its default, `claude`.
    let toml = format!(
        "[activity]\ntranscript_dir = \"{}\"\n[shards]\nclaude_config = \"{}\"\n\
         [core]\nsocket = \"{}\"\nstart_timeout_secs = 5\nexit_timeout_secs = 4\n\
         land_timeout_secs = 3\n",
        tx.display(),
        cc.display(),
        w.tmp.path().join("core.sock").display(),
    );
    std::fs::write(m.join("shards.toml"), toml).unwrap();
    ok(&w.mem(&["set", "devices/kiln-east/ip", "10.0.4.40", "--why", "seed"]));
    Some(w)
}

#[test]
fn core_start_finds_claude_in_home_local_bin_on_a_bare_path() {
    let Some(w) = world() else { return };
    w.start();
    assert_eq!(w.starts_log().lines().count(), 1, "{}", w.starts_log());
}

#[test]
fn the_watchdog_restores_claude_from_home_local_bin_on_a_bare_path() {
    let Some(w) = world() else { return };
    let id = w.start();
    w.kill_command();
    let r = ok(&w.mem(&["core", "check"]));
    assert!(r.contains("restored"), "{r}");
    w.assert_paths(2);
    assert!(
        w.starts_log().contains(&format!("--resume {id}")),
        "{}",
        w.starts_log()
    );
    assert_eq!(w.session_id(), id);
}

#[test]
fn rinit_starts_claude_from_home_local_bin_on_a_bare_path() {
    let Some(w) = world() else { return };
    let id = w.start();
    let mut c = w.cmd(&["core", "rinit", "--baton", "-"]);
    c.stdin(Stdio::piped());
    let mut child = c.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"BATON: nothing open; kiln-east settled.\n")
        .unwrap();
    let r = ok(&child.wait_with_output().unwrap());
    assert!(r.contains("handed off; new session "), "{r}");
    w.assert_paths(2);
    let new = w.session_id();
    assert_ne!(new, id);
}
