// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Acceptance tests for brief 04: the core as a long-lived session in a tmux
//! pane. No model is ever called: `[core] command` is a small fake core
//! (`sample::FAKE_CORE`) that reads lines and writes a fake transcript.
//! These tests need `tmux`; without it they print a note and pass.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use chrono::Utc;
use shards_core::coresession::{gap_bucket, summarize_usage, UsageEntry};
use shards_core::sample::write_fake_core;
use shards_core::tmux::{self, Tmux};
use shards_core::watch::EventKind;
use shards_core::{Address, Memory, Provenance, StartReport, WakeOutcome};
use tempfile::TempDir;

fn env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ]
}

fn prov(why: &str) -> Provenance {
    Provenance::new("core", Some("Ada".to_string()), why)
}

fn addr(s: &str) -> Address {
    Address::parse(s).unwrap()
}

struct World {
    tmp: TempDir,
    mem: Memory,
    extra: String,
}

impl Drop for World {
    fn drop(&mut self) {
        self.tmux().kill_server();
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
        self.tmp.path().join("core.sock")
    }
    fn tmux(&self) -> Tmux {
        Tmux::new(self.socket(), "core")
    }
    fn config(&mut self, extra: &str) {
        self.extra = extra.to_string();
        let fake = self.tmp.path().join("bin/fake-core.sh");
        // Defaults first; a key in `extra` replaces its default.
        let mut core = vec![
            ("start_timeout_secs", "5".to_string()),
            ("exit_timeout_secs", "4".to_string()),
            ("land_timeout_secs", "3".to_string()),
        ];
        for l in extra.lines() {
            if let Some((k, v)) = l.split_once(" = ") {
                core.retain(|(d, _)| *d != k);
                core.push((k, v.to_string()));
            }
        }
        let extra: String = core.iter().map(|(k, v)| format!("{k} = {v}\n")).collect();
        let text = format!(
            "[activity]\ntranscript_dir = \"{tx}\"\n[core]\ncommand = \"sh {fake} {tx} {ctl}\"\n\
             socket = \"{sock}\"\n{extra}",
            tx = self.tx().display(),
            fake = fake.display(),
            ctl = self.ctl().display(),
            sock = self.socket().display(),
        );
        // `extra` may add keys to [core] or new tables; put [core] keys first.
        fs::write(self.root().join("shards.toml"), text).unwrap();
        self.mem = Memory::open_with_env(self.root(), env()).unwrap();
    }
    fn flag(&self, name: &str, on: bool) {
        let p = self.ctl().join(name);
        if on {
            fs::write(p, "").unwrap();
        } else {
            let _ = fs::remove_file(p);
        }
    }
    fn starts(&self) -> Vec<String> {
        fs::read_to_string(self.ctl().join("starts.log"))
            .unwrap_or_default()
            .lines()
            .map(String::from)
            .collect()
    }
    fn transcript(&self, id: &str) -> PathBuf {
        self.tx()
            .join(shards_core::activity::claude_slug(&self.root()))
            .join(format!("{id}.jsonl"))
    }
    /// Prompts the fake core recorded in session `id`.
    fn prompts(&self, id: &str) -> Vec<String> {
        fs::read_to_string(self.transcript(id))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["type"] == "user")
            .filter_map(|v| v["message"]["content"].as_str().map(String::from))
            .collect()
    }
    fn wakes(&self, id: &str) -> Vec<String> {
        self.prompts(id)
            .into_iter()
            .filter(|p| p.starts_with("[WAKE]"))
            .collect()
    }
    /// Queued lines the fake core fed into a busy turn in session `id`.
    fn absorbed(&self, id: &str) -> Vec<String> {
        fs::read_to_string(self.transcript(id))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v["attachment"]["type"] == "queued_command")
            .filter_map(|v| v["attachment"]["prompt"].as_str().map(String::from))
            .collect()
    }
    /// Keep the fake core busy for `secs` seconds (a turn with tool calls),
    /// as a person typing into its pane would.
    fn busy(&self, id: &str, secs: u32) {
        let line = format!("work {secs}");
        self.tmux().type_text(&line).unwrap();
        self.tmux().enter().unwrap();
        self.wait_for(
            || self.prompts(id).contains(&line),
            "the busy turn to start",
        );
    }
    fn wait_for(&self, mut ok: impl FnMut() -> bool, what: &str) {
        for _ in 0..200 {
            if ok() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("timed out waiting for {what}");
    }
    /// Kill the core's command, leaving the pane at its shell.
    fn kill_command(&self) {
        let pane = self.tmux().pane().expect("pane");
        for c in tmux::children(pane.pid) {
            std::process::Command::new("kill")
                .args(["-KILL", &c.to_string()])
                .status()
                .unwrap();
        }
        for _ in 0..100 {
            if tmux::children(pane.pid).is_empty() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("command did not die");
    }
    /// Two shards edit the same fact: a same-fact event on the next pass.
    fn overlap(&self, a: &str, b: &str, address: &str) {
        for id in [a, b] {
            let meta = self.mem.shard_open(id, Some("Ada")).unwrap();
            let p = meta.worktree.join(addr(address).home());
            let text = fs::read_to_string(&p).unwrap();
            fs::write(
                &p,
                text.replace("10.0.4.40", &format!("10.9.{}.1", id.len())),
            )
            .unwrap();
        }
    }
}

/// A fresh memory with the fake core configured, or `None` without tmux.
fn world(extra: &str) -> Option<World> {
    if !tmux::available() {
        eprintln!("tmux is not installed; skipping");
        return None;
    }
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("m");
    Memory::init_with_env(&root, &prov(""), env()).unwrap();
    let mem = Memory::open_with_env(&root, env()).unwrap();
    for (a, v) in [
        ("devices/kiln-east/ip", "10.0.4.40"),
        ("devices/press-02/ip", "10.0.4.40"),
        ("network/dns", "10.0.4.40"),
    ] {
        mem.set(&addr(a), v, &prov("seed")).unwrap();
    }
    write_fake_core(&tmp.path().join("bin")).unwrap();
    fs::create_dir_all(tmp.path().join("ctl")).unwrap();
    let mut w = World {
        tmp,
        mem,
        extra: String::new(),
    };
    w.config(extra);
    Some(w)
}

fn started_id(r: StartReport) -> String {
    match r {
        StartReport::Started { session_id, .. } => session_id.expect("pinned"),
        other => panic!("not started: {other:?}"),
    }
}

/// Every file under `.shards/core/`, as text.
fn core_files(w: &World) -> String {
    let mut s = String::new();
    for e in fs::read_dir(w.root().join(".shards/core")).unwrap() {
        let p = e.unwrap().path();
        if p.is_file() {
            s.push_str(&String::from_utf8_lossy(&fs::read(&p).unwrap_or_default()));
        }
    }
    s
}

#[test]
fn start_status_and_stop() {
    let Some(w) = world("") else { return };
    let st = w.mem.core_status(Utc::now()).unwrap();
    assert!(!st.started && !st.pane_up && !st.command_alive);
    assert_eq!(st.wake_mode, "auto (command)");

    let id = started_id(w.mem.core_start(false).unwrap());
    // The pin is the newest transcript for the core's cwd.
    assert!(w.transcript(&id).is_file());
    let st = w.mem.core_status(Utc::now()).unwrap();
    assert!(st.started && st.pane_up && st.command_alive);
    assert_eq!(st.session_id.as_deref(), Some(id.as_str()));
    assert_eq!(st.wake_mode, "auto (pane)");
    assert_eq!(st.keep_warm.state, "off");
    assert!(st.attach.contains("attach") && st.attach.contains("core.sock"));
    // The first message points at soul/ and mem core context, and was
    // delivered as the session's first prompt.
    let first = &w.prompts(&id)[0];
    assert!(first.contains("soul/core.md") && first.contains("mem core context"));
    assert!(w.root().join("soul/core.md").is_file());

    // Starting again is a no-op.
    match w.mem.core_start(false).unwrap() {
        StartReport::AlreadyRunning { session_id } => assert_eq!(session_id, Some(id.clone())),
        other => panic!("{other:?}"),
    }
    assert_eq!(w.starts().len(), 1);

    assert!(w.mem.core_stop().unwrap());
    let st = w.mem.core_status(Utc::now()).unwrap();
    assert!(!st.started && !st.pane_up && !st.command_alive);
    // The pin survives a stop, so the session can be resumed.
    assert_eq!(st.session_id.as_deref(), Some(id.as_str()));
    match w.mem.core_start(true).unwrap() {
        StartReport::Started {
            session_id,
            resumed,
        } => {
            assert!(resumed);
            assert_eq!(session_id.as_deref(), Some(id.as_str()));
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(w.starts().last().unwrap(), &format!("args: --resume {id}"));
    w.mem.core_stop().unwrap();
}

#[test]
fn a_wake_lands_exactly_once_and_long_text_is_not_typed() {
    let Some(w) = world("") else { return };
    let id = started_id(w.mem.core_start(false).unwrap());
    w.overlap("s1", "s2", "devices/kiln-east/ip");
    w.overlap("s3", "s4", "devices/press-02/ip");
    let r = w.mem.watch_pass(Utc::now()).unwrap();
    assert_eq!(r.fired.len(), 2);
    match r.wake {
        WakeOutcome::Woken {
            events,
            upto,
            confirmed,
        } => {
            assert_eq!(events, 2);
            assert_eq!(upto, 2);
            assert!(confirmed, "the line was seen in the transcript");
        }
        other => panic!("{other:?}"),
    }
    // Same pending set: no second wake, however many passes run.
    for _ in 0..3 {
        let r = w.mem.watch_pass(Utc::now()).unwrap();
        assert_eq!(r.wake, WakeOutcome::AwaitingAck { events: 2 });
    }
    let wakes = w.wakes(&id);
    assert_eq!(
        wakes,
        vec!["[WAKE] 2 events, up to #2: run mem core context"]
    );
    // One short line: the event data is not typed.
    assert!(wakes[0].len() < 80);
    let ctx = w.mem.core_context(Utc::now()).unwrap();
    assert_eq!(ctx.events.len(), 2);
    assert_eq!(ctx.ack_upto, Some(2));
    for e in &ctx.events {
        assert!(!w.prompts(&id).iter().any(|p| p.contains(&e.summary)));
    }

    // Unacknowledged events stay pending; a new event wakes again, once.
    w.overlap("s5", "s6", "network/dns");
    let r = w.mem.watch_pass(Utc::now()).unwrap();
    assert!(
        matches!(
            r.wake,
            WakeOutcome::Woken {
                events: 3,
                upto: 3,
                ..
            }
        ),
        "{:?}",
        r.wake
    );
    assert_eq!(w.wakes(&id).len(), 2);

    // The ack marks them handled.
    let (done, left) = w.mem.core_ack(Some(3), Utc::now()).unwrap();
    assert_eq!((done, left), (3, 0));
    assert_eq!(
        w.mem.watch_pass(Utc::now()).unwrap().wake,
        WakeOutcome::Idle
    );
    assert_eq!(w.wakes(&id).len(), 2);
    assert!(w.mem.core_context(Utc::now()).unwrap().events.is_empty());
}

#[test]
fn ack_only_handles_what_the_core_saw() {
    let Some(w) = world("") else { return };
    w.mem.core_start(false).unwrap();
    w.overlap("s1", "s2", "devices/kiln-east/ip");
    w.mem.watch_pass(Utc::now()).unwrap();
    // An event arrives after the core read its context, before its ack.
    w.overlap("s3", "s4", "network/dns");
    let r = w.mem.watch_pass(Utc::now()).unwrap();
    assert_eq!(r.fired.len(), 1);
    let (done, left) = w.mem.core_ack(Some(1), Utc::now()).unwrap();
    assert_eq!((done, left), (1, 1));
    assert_eq!(w.mem.core_context(Utc::now()).unwrap().events[0].id, 2);
}

#[test]
fn events_survive_a_dead_core() {
    let Some(mut w) = world("watchdog = false") else {
        return;
    };
    let id = started_id(w.mem.core_start(false).unwrap());
    w.kill_command();
    w.overlap("s1", "s2", "devices/kiln-east/ip");
    let r = w.mem.watch_pass(Utc::now()).unwrap();
    assert_eq!(r.wake, WakeOutcome::CoreDown);
    assert_eq!(r.pending, 1);
    let r = w.mem.watch_pass(Utc::now()).unwrap();
    assert_eq!(r.wake, WakeOutcome::CoreDown);
    assert_eq!(w.mem.core_context(Utc::now()).unwrap().events.len(), 1);

    // Turn the watchdog on: the next pass restores, resumes, then wakes.
    w.config("");
    let r = w.mem.watch_pass(Utc::now()).unwrap();
    assert!(
        r.core.iter().any(|n| n.contains("restored")),
        "{:?}",
        r.core
    );
    assert!(
        matches!(r.wake, WakeOutcome::Woken { events: 1, .. }),
        "{:?}",
        r.wake
    );
    assert_eq!(w.starts().last().unwrap(), &format!("args: --resume {id}"));
    assert_eq!(w.wakes(&id).len(), 1);
}

#[test]
fn watchdog_restores_the_pane_and_resumes_the_pinned_id() {
    let Some(w) = world("") else { return };
    let id = started_id(w.mem.core_start(false).unwrap());
    // A newer, unrelated transcript in the same cwd: --continue would pick
    // it. The watchdog must resume the pinned id.
    let decoy = w.transcript("decoy-session");
    fs::write(
        &decoy,
        format!(
            "{{\"type\":\"user\",\"timestamp\":\"{}\",\"message\":{{\"content\":\"x\"}}}}\n",
            Utc::now().to_rfc3339()
        ),
    )
    .unwrap();
    // The whole tmux server goes away.
    w.tmux().kill_server();
    assert!(!w.mem.core_status(Utc::now()).unwrap().pane_up);
    let r = w.mem.core_check(Utc::now()).unwrap();
    assert!(r.restored, "{:?}", r.notes);
    let st = w.mem.core_status(Utc::now()).unwrap();
    assert!(st.pane_up && st.command_alive);
    assert_eq!(st.session_id.as_deref(), Some(id.as_str()));
    assert_eq!(st.failures, 0);
    let starts = w.starts();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[1], format!("args: --resume {id}"));
    assert!(w.starts().iter().all(|l| !l.contains("--continue")));
    let log = fs::read_to_string(w.root().join(".shards/core/core.log")).unwrap();
    assert!(log.contains("watchdog: pane gone") && log.contains(&format!("resumed session {id}")));
    // A healthy core: the check does nothing.
    let r = w.mem.core_check(Utc::now()).unwrap();
    assert!(!r.restored && r.notes.is_empty(), "{:?}", r.notes);
    assert_eq!(w.starts().len(), 2);
}

#[test]
fn a_deliberate_stop_stays_stopped() {
    let Some(w) = world("") else { return };
    w.mem.core_start(false).unwrap();
    w.mem.core_stop().unwrap();
    w.overlap("s1", "s2", "devices/kiln-east/ip");
    for _ in 0..2 {
        let r = w.mem.core_check(Utc::now()).unwrap();
        assert!(!r.restored);
        let r = w.mem.watch_pass(Utc::now()).unwrap();
        assert!(r.core.is_empty(), "{:?}", r.core);
        // Auto mode falls back to the command mode of brief 03: no command
        // set, so the events wait.
        assert_eq!(r.wake, WakeOutcome::NoCommand);
    }
    assert!(!w.mem.core_status(Utc::now()).unwrap().pane_up);
    assert_eq!(w.starts().len(), 1);
}

#[test]
fn three_failed_restores_stop_retrying() {
    let Some(w) = world("start_timeout_secs = 1.5") else {
        return;
    };
    w.mem.core_start(false).unwrap();
    w.flag("fail-all", true);
    w.kill_command();
    for n in 1..=3u32 {
        let r = w.mem.core_check(Utc::now()).unwrap();
        assert!(!r.restored);
        let st = w.mem.core_status(Utc::now()).unwrap();
        assert_eq!(st.failures, n);
        assert_eq!(st.gave_up, n == 3);
    }
    let starts = w.starts().len();
    assert_eq!(starts, 4, "one start, three restore attempts");
    // It has stopped trying, and says so.
    let r = w.mem.core_check(Utc::now()).unwrap();
    assert!(
        r.notes.iter().any(|n| n.contains("gave up")),
        "{:?}",
        r.notes
    );
    w.mem.watch_pass(Utc::now()).unwrap();
    assert_eq!(w.starts().len(), starts);
    let ctx = w.mem.core_context(Utc::now()).unwrap();
    assert!(ctx.events.iter().any(|e| e.kind == EventKind::CoreDown));
    let log = fs::read_to_string(w.root().join(".shards/core/core.log")).unwrap();
    assert!(log.contains("restore failed (3/3)") && log.contains("giving up"));
    // mem core start resets it.
    w.flag("fail-all", false);
    w.mem.core_start(true).unwrap();
    let st = w.mem.core_status(Utc::now()).unwrap();
    assert!(st.command_alive && !st.gave_up && st.failures == 0);
}

#[test]
fn rinit_hands_off_to_a_fresh_session_with_the_baton() {
    let Some(w) = world("") else { return };
    let old = started_id(w.mem.core_start(false).unwrap());
    let new = w
        .mem
        .core_rinit("BATON: watching kiln-east; Ada and Basil were told.\nSecond line.")
        .unwrap()
        .expect("new id");
    assert_ne!(new, old);
    let st = w.mem.core_status(Utc::now()).unwrap();
    assert!(st.command_alive);
    assert_eq!(st.session_id.as_deref(), Some(new.as_str()));
    let first = &w.prompts(&new)[0];
    assert!(first.contains("BATON: watching kiln-east") && first.contains("Second line"));
    assert!(first.contains("soul/core.md"));
    let pin: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(w.root().join(".shards/core/session.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(pin["previous"], serde_json::json!([old]));
    // Wakes now go to the new session.
    w.overlap("s1", "s2", "devices/kiln-east/ip");
    w.mem.watch_pass(Utc::now()).unwrap();
    assert_eq!(w.wakes(&new).len(), 1);
    assert!(w.wakes(&old).is_empty());
}

#[test]
fn a_failed_handoff_leaves_the_old_session() {
    let Some(w) = world("exit_timeout_secs = 1\nstart_timeout_secs = 1.5") else {
        return;
    };
    let old = started_id(w.mem.core_start(false).unwrap());

    // The exit step fails: nothing else happens.
    w.flag("ignore-exit", true);
    let e = w.mem.core_rinit("baton").unwrap_err().to_string();
    assert!(
        e.contains("did not exit") && e.contains("still running"),
        "{e}"
    );
    let st = w.mem.core_status(Utc::now()).unwrap();
    assert!(st.command_alive);
    assert_eq!(st.session_id.as_deref(), Some(old.as_str()));
    assert_eq!(w.starts().len(), 1);
    w.flag("ignore-exit", false);

    // The new session fails to start: the old one is resumed by id.
    w.flag("fail-fresh", true);
    let e = w.mem.core_rinit("baton").unwrap_err().to_string();
    assert!(e.contains("old session") && e.contains("resumed"), "{e}");
    let st = w.mem.core_status(Utc::now()).unwrap();
    assert!(st.command_alive);
    assert_eq!(st.session_id.as_deref(), Some(old.as_str()));
    assert_eq!(w.starts().last().unwrap(), &format!("args: --resume {old}"));
    let log = fs::read_to_string(w.root().join(".shards/core/core.log")).unwrap();
    assert!(log.contains("rinit: FAILED"));
}

#[test]
fn wakes_during_a_handoff_stay_pending() {
    let Some(w) = world("") else { return };
    let old = started_id(w.mem.core_start(false).unwrap());
    w.flag("slow-exit", true);
    let root = w.root();
    let h = std::thread::spawn(move || {
        let m = Memory::open_with_env(root, env()).unwrap();
        m.core_rinit("baton during wakes").unwrap()
    });
    std::thread::sleep(Duration::from_millis(500));
    w.overlap("s1", "s2", "devices/kiln-east/ip");
    let r = w.mem.watch_pass(Utc::now()).unwrap();
    assert_eq!(r.fired.len(), 1);
    assert_eq!(r.wake, WakeOutcome::Busy);
    assert_eq!(r.pending, 1);
    let new = h.join().unwrap().expect("new id");
    assert!(w.wakes(&old).is_empty());
    let r = w.mem.watch_pass(Utc::now()).unwrap();
    assert!(
        matches!(r.wake, WakeOutcome::Woken { events: 1, .. }),
        "{:?}",
        r.wake
    );
    assert_eq!(w.wakes(&new).len(), 1);
}

#[test]
fn keep_warm_pings_only_past_the_threshold_by_entry_time() {
    let Some(w) = world("keep_warm = true\nkeep_warm_mins = 50") else {
        return;
    };
    let id = started_id(w.mem.core_start(false).unwrap());
    let tx = w.transcript(&id);
    let now = Utc::now();

    // Fresh entries: no ping.
    assert!(!w.mem.core_keep_warm(now).unwrap().pinged);
    // An old mtime doesn't matter: the entries inside are fresh.
    std::process::Command::new("touch")
        .args(["-d", "3 hours ago"])
        .arg(&tx)
        .status()
        .unwrap();
    assert!(
        !w.mem
            .core_keep_warm(now + chrono::Duration::minutes(10))
            .unwrap()
            .pinged
    );
    // A fresh mtime from a bookkeeping line without a timestamp doesn't
    // matter either: 49 minutes after the last entry is below the threshold,
    let mut f = fs::OpenOptions::new().append(true).open(&tx).unwrap();
    std::io::Write::write_all(&mut f, b"{\"type\":\"last-prompt\"}\n").unwrap();
    drop(f);
    assert!(
        !w.mem
            .core_keep_warm(now + chrono::Duration::minutes(49))
            .unwrap()
            .pinged
    );
    // and 51 minutes after it is past it, though the file was just written.
    let later = now + chrono::Duration::minutes(51);
    let r = w.mem.core_keep_warm(later).unwrap();
    assert!(r.pinged, "{:?}", r.notes);
    let hhmm = later
        .with_timezone(&chrono::Local)
        .format("[%H:%M]")
        .to_string();
    assert_eq!(w.prompts(&id).last().unwrap(), &hhmm);
}

#[test]
fn keep_warm_stops_at_the_quiet_limit_and_resumes_on_real_activity() {
    let Some(w) = world("keep_warm = true\nkeep_warm_mins = 50\nkeep_warm_quiet_limit = 2") else {
        return;
    };
    let id = started_id(w.mem.core_start(false).unwrap());
    let h = chrono::Duration::hours;
    let now = Utc::now();
    assert!(w.mem.core_keep_warm(now + h(2)).unwrap().pinged);
    assert!(w.mem.core_keep_warm(now + h(4)).unwrap().pinged);
    assert!(!w.mem.core_keep_warm(now + h(6)).unwrap().pinged);
    assert!(!w.mem.core_keep_warm(now + h(8)).unwrap().pinged);
    let st = w.mem.core_status(now + h(8)).unwrap();
    assert!(
        st.keep_warm.state.starts_with("paused"),
        "{}",
        st.keep_warm.state
    );
    assert_eq!(st.keep_warm.quiet_pings, 2);

    // A person types to the core: real activity, pings resume.
    let t = w.tmux();
    t.type_text("hello core, any overlaps today?").unwrap();
    t.enter().unwrap();
    for _ in 0..100 {
        if w.prompts(&id).iter().any(|p| p.starts_with("hello core")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(w.mem.core_keep_warm(now + h(10)).unwrap().pinged);
    let pings = w
        .prompts(&id)
        .iter()
        .filter(|p| shards_core::coreadapter::is_heartbeat(p))
        .count();
    assert_eq!(pings, 3);

    // Each ping went to usage.jsonl with the reply's token counts.
    let usage = w.mem.core_usage(now + h(10)).unwrap();
    let pinged: Vec<&UsageEntry> = usage.iter().filter(|u| u.kind == "ping").collect();
    assert_eq!(pinged.len(), 3);
    assert!(pinged
        .iter()
        .all(|u| u.reply && u.usage.output == 1 && u.usage.cache_read == 5000));
    assert!(pinged[1].gap_secs.unwrap() > 7000.0);
    // Numbers only: nothing from the transcript's text.
    let files = core_files(&w);
    assert!(!files.contains("PLANTED"));
    assert!(!files.contains("hello core"));
}

#[test]
fn wakes_are_logged_with_usage() {
    let Some(w) = world("") else { return };
    w.mem.core_start(false).unwrap();
    w.flag("cold", true);
    w.overlap("s1", "s2", "devices/kiln-east/ip");
    w.mem.watch_pass(Utc::now()).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let u = w.mem.core_usage(Utc::now()).unwrap();
    assert_eq!(u.len(), 1);
    assert_eq!(u[0].kind, "wake");
    assert_eq!(u[0].events, Some(1));
    assert!(u[0].reply);
    assert_eq!(u[0].warm(), Some(false));
    assert_eq!(u[0].usage.cache_creation, 5200);
    assert!(!core_files(&w).contains("PLANTED"));
}

fn fixture(
    time: &str,
    kind: &str,
    gap: Option<f64>,
    read: u64,
    write: u64,
    out: u64,
) -> UsageEntry {
    let u = shards_core::coreadapter::Usage {
        input: 3,
        output: out,
        cache_read: read,
        cache_creation: write,
    };
    UsageEntry {
        time: time.into(),
        kind: kind.into(),
        gap_secs: gap,
        idle_secs: gap,
        events: (kind == "wake").then_some(1),
        reply: true,
        calls: 1,
        usage: u,
        first: Some(u),
        same_turn: false,
    }
}

#[test]
fn usage_summary_from_fixtures() {
    let entries = vec![
        fixture("2026-11-04T10:00:00Z", "wake", None, 0, 9000, 300),
        fixture("2026-11-04T10:02:00Z", "wake", Some(120.0), 9000, 100, 200),
        fixture("2026-11-04T10:03:00Z", "wake", Some(60.0), 9100, 300, 100),
        fixture("2026-11-04T12:00:00Z", "wake", Some(7000.0), 0, 9400, 400),
        fixture("2026-11-04T12:50:00Z", "ping", Some(3000.0), 9400, 0, 1),
        UsageEntry {
            same_turn: false,
            reply: false,
            first: None,
            ..fixture("2026-11-04T15:00:00Z", "wake", Some(7800.0), 0, 0, 0)
        },
    ];
    assert_eq!(gap_bucket(Some(120.0)), "<5m");
    assert_eq!(gap_bucket(Some(7000.0)), "1-2h");
    assert_eq!(gap_bucket(None), "first");
    let rows = summarize_usage(&entries);
    let find = |k: &str, g: &str, c: &str| {
        rows.iter()
            .find(|r| r.kind == k && r.gap == g && r.cache == c)
            .unwrap_or_else(|| panic!("no row {k} {g} {c}: {rows:?}"))
    };
    let warm = find("wake", "<5m", "warm");
    assert_eq!(warm.count, 2);
    assert_eq!(warm.avg_cache_read, 9050.0);
    assert_eq!(warm.avg_output, 150.0);
    assert_eq!(find("wake", "1-2h", "cold").avg_cache_creation, 9400.0);
    assert_eq!(find("wake", "first", "cold").count, 1);
    assert_eq!(find("wake", "2h+", "unknown").count, 1);
    assert_eq!(find("ping", "30-60m", "warm").count, 1);
    assert_eq!(rows.len(), 5);

    // Through the library: read from .shards/core/usage.jsonl.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("m");
    Memory::init_with_env(&root, &prov(""), env()).unwrap();
    let mem = Memory::open_with_env(&root, env()).unwrap();
    let dir = mem.core_dir().unwrap();
    fs::create_dir_all(&dir).unwrap();
    let text: String = entries
        .iter()
        .map(|e| serde_json::to_string(e).unwrap() + "\n")
        .collect();
    fs::write(dir.join("usage.jsonl"), text).unwrap();
    assert_eq!(mem.core_usage(Utc::now()).unwrap(), entries);
}

// Brief 06: behaviour seen on real Claude Code 2.1.319 (haiku), mirrored by
// the fake core.

/// A wake typed while the core is busy is queued, joins the busy turn at its
/// next tool call (not as a new prompt), and a second event in the same turn
/// wakes exactly once more. That turn is the reply both wakes are counted
/// against, once.
#[test]
fn wakes_while_busy_join_the_turn_once_each() {
    let Some(w) = world("") else { return };
    let id = started_id(w.mem.core_start(false).unwrap());
    w.busy(&id, 4);
    w.overlap("s1", "s2", "devices/kiln-east/ip");
    let r = w.mem.watch_pass(Utc::now()).unwrap();
    assert!(
        matches!(
            r.wake,
            WakeOutcome::Woken {
                events: 1,
                upto: 1,
                confirmed: true
            }
        ),
        "{:?}",
        r.wake
    );
    assert_eq!(
        w.mem.watch_pass(Utc::now()).unwrap().wake,
        WakeOutcome::AwaitingAck { events: 1 }
    );
    // A second event in the same busy turn: one more wake, then none.
    w.overlap("s3", "s4", "devices/press-02/ip");
    let r = w.mem.watch_pass(Utc::now()).unwrap();
    assert!(
        matches!(
            r.wake,
            WakeOutcome::Woken {
                events: 2,
                upto: 2,
                confirmed: true
            }
        ),
        "{:?}",
        r.wake
    );
    for _ in 0..2 {
        assert_eq!(
            w.mem.watch_pass(Utc::now()).unwrap().wake,
            WakeOutcome::AwaitingAck { events: 2 }
        );
    }
    w.wait_for(|| w.absorbed(&id).len() == 2, "both wakes to join the turn");
    w.wait_for(
        || w.mem.core_usage(Utc::now()).unwrap().len() == 2,
        "the busy turn to end",
    );
    assert_eq!(
        w.absorbed(&id),
        vec![
            "[WAKE] 1 event, up to #1: run mem core context",
            "[WAKE] 2 events, up to #2: run mem core context"
        ]
    );
    // Neither became a prompt of its own; nothing was lost or doubled.
    assert!(w.wakes(&id).is_empty());
    assert_eq!(w.mem.core_context(Utc::now()).unwrap().events.len(), 2);
    assert_eq!(w.mem.core_ack(None, Utc::now()).unwrap(), (2, 0));

    // Usage: the busy turn (tool calls and all) is the first wake's reply;
    // the second wake is marked as the same turn and not counted twice.
    let u = w.mem.core_usage(Utc::now()).unwrap();
    assert!(u[0].reply && !u[0].same_turn);
    assert!(u[0].calls >= 3, "{:?}", u[0]);
    assert!(u[1].reply && u[1].same_turn && u[1].calls == 0);
    let rows = summarize_usage(&u);
    assert_eq!(rows.iter().map(|r| r.count).sum::<usize>(), 1);
    // A later prompt is not mistaken for either wake's reply.
    w.tmux().type_text("hello core").unwrap();
    w.tmux().enter().unwrap();
    w.wait_for(
        || w.prompts(&id).contains(&"hello core".to_string()),
        "the prompt",
    );
    assert_eq!(w.mem.core_usage(Utc::now()).unwrap().len(), 2);
}

/// Killed (SIGKILL) mid-turn, after a wake joined the turn but before the
/// ack: the event stays pending, the watchdog resumes the pinned session in
/// a pane it can type into, and the restored session is woken once more.
#[test]
fn a_core_killed_mid_wake_is_woken_once_more() {
    let Some(w) = world("") else { return };
    let id = started_id(w.mem.core_start(false).unwrap());
    w.busy(&id, 30);
    w.overlap("s1", "s2", "devices/kiln-east/ip");
    let r = w.mem.watch_pass(Utc::now()).unwrap();
    assert!(
        matches!(r.wake, WakeOutcome::Woken { upto: 1, .. }),
        "{:?}",
        r.wake
    );
    w.wait_for(|| w.absorbed(&id).len() == 1, "the wake to join the turn");
    w.kill_command();

    let r = w.mem.watch_pass(Utc::now()).unwrap();
    assert!(
        r.core.iter().any(|n| n.contains("restored")),
        "{:?}",
        r.core
    );
    assert!(
        matches!(
            r.wake,
            WakeOutcome::Woken {
                events: 1,
                upto: 1,
                confirmed: true
            }
        ),
        "{:?}",
        r.wake
    );
    assert_eq!(w.starts().last().unwrap(), &format!("args: --resume {id}"));
    assert_eq!(
        w.mem.watch_pass(Utc::now()).unwrap().wake,
        WakeOutcome::AwaitingAck { events: 1 }
    );
    // Once in the dead turn, once as a prompt to the restored session.
    assert_eq!(w.absorbed(&id).len(), 1);
    assert_eq!(
        w.wakes(&id),
        vec!["[WAKE] 1 event, up to #1: run mem core context"]
    );
    assert_eq!(w.mem.core_ack(None, Utc::now()).unwrap(), (1, 0));
}
