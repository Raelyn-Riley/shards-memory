// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Acceptance tests for brief 05: a shard is a session in its own pane. No
//! model is ever called: `[shards] command` is the fake core
//! (`sample::FAKE_CORE`), which reads lines and writes a fake transcript,
//! and Claude Code's config is a fake file. These tests need `tmux`;
//! without it they print a note and pass.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use shards_core::coresession::Landed;
use shards_core::sample::write_fake_core;
use shards_core::shardsession::ShardLaunch;
use shards_core::tmux::{self, Tmux};
use shards_core::{Error, Memory, PaneState, Provenance, ShardMeta, ShardStart};
use tempfile::TempDir;

fn env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ]
}

struct World {
    tmp: TempDir,
    mem: Memory,
}

impl Drop for World {
    fn drop(&mut self) {
        Tmux::new(self.socket(), "x").kill_server();
    }
}

impl World {
    fn root(&self) -> PathBuf {
        self.mem.root().to_path_buf()
    }
    fn tx(&self) -> PathBuf {
        self.tmp.path().join("tx")
    }
    fn ctl(&self) -> PathBuf {
        self.tmp.path().join("ctl")
    }
    fn socket(&self) -> PathBuf {
        self.tmp.path().join("shards.sock")
    }
    fn claude_json(&self) -> PathBuf {
        self.tmp.path().join("claude.json")
    }
    fn flag(&self, name: &str, on: bool) {
        let p = self.ctl().join(name);
        if on {
            fs::write(p, "").unwrap();
        } else {
            let _ = fs::remove_file(p);
        }
    }
    /// Write Claude Code's (fake) config: is the memory root trusted?
    fn trust(&self, trusted: bool) {
        let root = fs::canonicalize(self.root()).unwrap();
        let v = serde_json::json!({"projects": {
            root.to_string_lossy(): {"hasTrustDialogAccepted": trusted}
        }});
        fs::write(self.claude_json(), v.to_string()).unwrap();
    }
    fn starts(&self) -> Vec<String> {
        fs::read_to_string(self.ctl().join("starts.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
    fn start(&self, id: Option<&str>, human: &str) -> ShardMeta {
        match self
            .mem
            .shard_start(id, Some(human), false, ShardLaunch::Blank)
            .unwrap()
        {
            ShardStart::Started { meta, .. } => meta,
            other => panic!("not started: {other:?}"),
        }
    }
    fn resume(&self, id: &str) -> (ShardMeta, bool, Option<String>) {
        match self
            .mem
            .shard_start(Some(id), None, true, ShardLaunch::Blank)
            .unwrap()
        {
            ShardStart::Started {
                meta,
                resumed,
                note,
            } => (meta, resumed, note),
            other => panic!("not started: {other:?}"),
        }
    }
    /// Prompts the fake agent recorded in a shard's pinned session.
    fn prompts(&self, id: &str) -> Vec<String> {
        let Some(p) = self.mem.shard_transcript(id).unwrap() else {
            return Vec::new();
        };
        fs::read_to_string(p)
            .unwrap()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "user")
            .filter_map(|v| v["message"]["content"].as_str().map(String::from))
            .collect()
    }
    fn pane(&self, id: &str) -> PaneState {
        self.mem.shard_pane_state(id)
    }
}

fn sid(meta: &ShardMeta) -> String {
    meta.session
        .as_ref()
        .and_then(|s| s.session_id.clone())
        .expect("pinned")
}

/// A fresh memory with the fake agent as `[shards] command` and a trusted
/// root, or `None` without tmux.
fn world() -> Option<World> {
    if !tmux::available() {
        eprintln!("tmux is not installed; skipping");
        return None;
    }
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("m");
    let prov = Provenance::new("core", Some("Ada".to_string()), "seed");
    Memory::init_with_env(&root, &prov, env()).unwrap();
    let mem = Memory::open_with_env(&root, env()).unwrap();
    mem.set(
        &shards_core::Address::parse("devices/kiln-east/ip").unwrap(),
        "10.0.4.40",
        &prov,
    )
    .unwrap();
    let fake = write_fake_core(&tmp.path().join("bin")).unwrap();
    fs::create_dir_all(tmp.path().join("ctl")).unwrap();
    let mut w = World { tmp, mem };
    let text = format!(
        "[activity]\ntranscript_dir = \"{tx}\"\n\
         [shards]\ncommand = \"sh {fake} {tx} {ctl}\"\nsocket = \"{sock}\"\nclaude_config = \"{cc}\"\n\
         [core]\nstart_timeout_secs = 5\nexit_timeout_secs = 4\nland_timeout_secs = 3\n",
        tx = w.tx().display(),
        fake = fake.display(),
        ctl = w.ctl().display(),
        sock = w.socket().display(),
        cc = w.claude_json().display(),
    );
    fs::write(w.root().join("shards.toml"), text).unwrap();
    w.mem = Memory::open_with_env(w.root(), env()).unwrap();
    w.trust(true);
    Some(w)
}

#[test]
fn two_shards_two_panes_one_server_each_chat_separate() {
    let Some(w) = world() else { return };
    let a = w.start(None, "Ada");
    let b = w.start(None, "Basil");
    assert_eq!((a.id.as_str(), b.id.as_str()), ("s1", "s2"));
    // Pinned before anything was said, and launched under that id.
    let (ida, idb) = (sid(&a), sid(&b));
    assert_ne!(ida, idb);
    let starts = w.starts();
    assert!(starts
        .iter()
        .any(|l| l == &format!("args: --session-id {ida}")));
    assert!(starts
        .iter()
        .any(|l| l == &format!("args: --session-id {idb}")));
    // A blank start: no first message, so no transcript yet.
    assert!(w.mem.shard_transcript("s1").unwrap().is_none());
    // Both panes live on the one shards server, each in its own worktree.
    assert_eq!((w.pane("s1"), w.pane("s2")), (PaneState::Up, PaneState::Up));
    let out = std::process::Command::new("tmux")
        .arg("-S")
        .arg(w.socket())
        .args([
            "list-sessions",
            "-F",
            "#{session_name} #{pane_current_path}",
        ])
        .output()
        .unwrap();
    let ls = String::from_utf8_lossy(&out.stdout);
    assert!(ls.contains(&format!("s1 {}", a.worktree.display())), "{ls}");
    assert!(ls.contains(&format!("s2 {}", b.worktree.display())), "{ls}");

    // The same text to both still lands in each one's own transcript.
    assert_eq!(
        w.mem.shard_send("s1", "hello heron feeder").unwrap(),
        Landed::Confirmed
    );
    assert_eq!(
        w.mem.shard_send("s2", "hello heron feeder").unwrap(),
        Landed::Confirmed
    );
    assert_eq!(
        w.mem
            .shard_send("s1", "first line\nsecond line\n\nnew para")
            .unwrap(),
        Landed::Confirmed
    );
    assert_eq!(
        w.prompts("s1"),
        vec!["hello heron feeder", "first line second line | new para"]
    );
    assert_eq!(w.prompts("s2"), vec!["hello heron feeder"]);
    let ta = w.mem.shard_transcript("s1").unwrap().unwrap();
    assert!(ta.to_string_lossy().ends_with(&format!("{ida}.jsonl")));

    // The board and list show the panes and pins.
    let board = w.mem.board(Utc::now()).unwrap();
    let row = board.shards.iter().find(|s| s.id == "s2").unwrap();
    assert_eq!(row.pane, PaneState::Up);
    assert_eq!(row.session_id.as_deref(), Some(idb.as_str()));
}

#[test]
fn stop_keeps_the_shard_and_resume_brings_the_pinned_session_back() {
    let Some(w) = world() else { return };
    let a = w.start(Some("s7"), "Ada");
    let id = sid(&a);
    w.mem.shard_send("s7", "remember the kestrel").unwrap();
    assert!(w.mem.shard_stop("s7").unwrap());
    assert_eq!(w.pane("s7"), PaneState::Down);
    // Branch, worktree and pin kept.
    let meta = w.mem.shard_meta("s7").unwrap();
    assert!(meta.worktree.is_dir());
    assert_eq!(sid(&meta), id);
    assert!(meta.session.as_ref().unwrap().stopped_at.is_some());
    let err = w.mem.shard_send("s7", "anyone?").unwrap_err();
    assert!(err.to_string().contains("--resume"), "{err}");
    // Stopping again is harmless.
    assert!(!w.mem.shard_stop("s7").unwrap());

    let (meta, resumed, note) = w.resume("s7");
    assert!(resumed, "{note:?}");
    assert_eq!(sid(&meta), id);
    assert!(meta.session.as_ref().unwrap().stopped_at.is_none());
    assert_eq!(w.starts().last().unwrap(), &format!("args: --resume {id}"));
    assert!(!w.starts().iter().any(|l| l.contains("--continue")));
    w.mem.shard_send("s7", "still there?").unwrap();
    assert_eq!(
        w.prompts("s7"),
        vec!["remember the kestrel", "still there?"]
    );

    // Resuming a running shard does nothing.
    assert!(matches!(
        w.mem
            .shard_start(Some("s7"), None, true, ShardLaunch::Blank)
            .unwrap(),
        ShardStart::AlreadyRunning { .. }
    ));
    // A fresh start can't reuse an open shard's id.
    let err = w
        .mem
        .shard_start(Some("s7"), Some("Ada"), false, ShardLaunch::Blank)
        .unwrap_err();
    assert!(matches!(err, Error::ShardExists(_)), "{err}");
}

#[test]
fn a_blank_session_with_nothing_said_restarts_under_the_same_id() {
    let Some(w) = world() else { return };
    let a = w.start(None, "Cleo");
    let id = sid(&a);
    w.mem.shard_stop(&a.id).unwrap();
    let (meta, resumed, note) = w.resume(&a.id);
    // Nothing to --resume (no transcript), so the same id starts blank again.
    assert!(!resumed);
    assert!(note.unwrap().contains("same id"));
    assert_eq!(sid(&meta), id);
    assert_eq!(
        w.starts().last().unwrap(),
        &format!("args: --session-id {id}")
    );
    // A shard from `mem shard open` gets a session pinned on resume.
    w.mem.shard_open("s9", Some("Cleo")).unwrap();
    let (meta, _, note) = w.resume("s9");
    assert!(note.unwrap().contains("pinned"));
    assert_eq!(
        w.starts().last().unwrap(),
        &format!("args: --session-id {}", sid(&meta))
    );
}

#[test]
fn an_untrusted_root_is_refused_before_anything_is_opened() {
    let Some(w) = world() else { return };
    w.trust(false);
    let err = w
        .mem
        .shard_start(None, Some("Ada"), false, ShardLaunch::Blank)
        .unwrap_err();
    assert!(err.to_string().contains("doesn't trust"), "{err}");
    assert!(w.mem.shard_list().unwrap().is_empty());
    assert!(w.starts().is_empty());
    // With no Claude Code config to read, start goes ahead (and the screen
    // check is still there).
    fs::remove_file(w.claude_json()).unwrap();
    w.start(None, "Ada");
}

#[test]
fn a_trust_question_on_screen_fails_fast_instead_of_hanging() {
    let Some(w) = world() else { return };
    w.flag("trust-prompt", true);
    let t = std::time::Instant::now();
    let err = w
        .mem
        .shard_start(Some("s3"), Some("Ada"), false, ShardLaunch::Blank)
        .unwrap_err();
    assert!(t.elapsed() < Duration::from_secs(5), "{:?}", t.elapsed());
    let msg = err.to_string();
    assert!(
        msg.contains("trust this folder") && msg.contains("--resume"),
        "{msg}"
    );
    // The shard is open; its pane was closed rather than left asking.
    assert!(w.mem.shard_meta("s3").is_ok());
    assert_eq!(w.pane("s3"), PaneState::Down);
    w.flag("trust-prompt", false);
    w.resume("s3");
    assert_eq!(w.pane("s3"), PaneState::Up);
}

#[test]
fn send_refuses_a_pane_at_its_shell() {
    let Some(w) = world() else { return };
    let a = w.start(None, "Ada");
    // The agent dies; the pane is back at its shell, where text would run.
    let t = Tmux::new(w.socket(), "s1");
    let pane = t.pane().unwrap();
    for c in tmux::children(pane.pid) {
        std::process::Command::new("kill")
            .args(["-KILL", &c.to_string()])
            .status()
            .unwrap();
    }
    for _ in 0..100 {
        if w.pane(&a.id) == PaneState::Shell {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(w.pane(&a.id), PaneState::Shell);
    let err = w.mem.shard_send(&a.id, "echo PLANTED-SHELL").unwrap_err();
    assert!(err.to_string().contains("at a shell"), "{err}");
    assert!(!t.capture().unwrap().contains("PLANTED-SHELL"));
    // --resume brings it back in the same pane.
    w.resume(&a.id);
    assert_eq!(w.pane(&a.id), PaneState::Up);
}

#[test]
fn idle_panes_stay_up_and_a_clean_dinit_stops_the_session_first() {
    let Some(w) = world() else { return };
    let a = w.start(None, "Ada");
    let b = w.start(None, "Basil");
    // A watcher pass (and the board) never close a pane, however idle.
    w.mem
        .watch_pass(Utc::now() + chrono::Duration::days(2))
        .unwrap();
    assert_eq!(
        (w.pane(&a.id), w.pane(&b.id)),
        (PaneState::Up, PaneState::Up)
    );
    let r = w.mem.dinit(&a.id, false).unwrap();
    assert!(r.closed && r.session_stopped, "{r:?}");
    assert_eq!(w.pane(&a.id), PaneState::Down);
    assert!(!a.worktree.exists());
    assert!(w.starts().iter().all(|l| !l.contains("--resume")));
    // The other shard is untouched.
    assert_eq!(w.pane(&b.id), PaneState::Up);
    // A kept branch means the next auto id skips s1.
    assert_eq!(w.mem.next_shard_id().unwrap(), "s3");
}

#[test]
fn attach_command_targets_the_shards_server_and_the_shard() {
    let Some(w) = world() else { return };
    let a = w.start(Some("s1.v2"), "Ada");
    let cmd = w.mem.shard_attach_command(&a.id).unwrap().join(" ");
    assert!(
        cmd.contains(&w.socket().to_string_lossy().into_owned()),
        "{cmd}"
    );
    assert!(cmd.ends_with("-t =s1~v2:"), "{cmd}");
    assert_eq!(w.pane("s1.v2"), PaneState::Up);
}
