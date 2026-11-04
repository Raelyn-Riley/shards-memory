// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! `mem board`, `mem watch` and `mem core context` as real processes:
//! privacy of everything printed, reading during a held main lock,
//! restarts, and a long-running watcher that outlives a failing wake.
//! All data is fake; the wake command never calls a model.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use shards_core::sample::{fake_transcript, PLANTED};
use tempfile::TempDir;

fn cmd(dir: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mem"));
    c.arg("-C")
        .arg(dir)
        .args(args)
        .env("SHARDS_AUTHOR", "core")
        .env("SHARDS_HUMAN", "Ada")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("SHARDS_DEBUG_HOLD_MS")
        .env_remove("SHARDS_MEMORY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    c
}

fn run(dir: &Path, args: &[&str]) -> Output {
    cmd(dir, args).output().unwrap()
}

fn ok(o: &Output) -> String {
    assert!(
        o.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

struct Setup {
    tmp: TempDir,
    m: PathBuf,
    wt: Vec<PathBuf>,
}

impl Setup {
    fn tx(&self) -> PathBuf {
        self.tmp.path().join("tx")
    }
    fn out(&self) -> PathBuf {
        self.tmp.path().join("out")
    }
    fn config(&self, wake: &str, extra: &str) {
        let text = format!(
            "[activity]\ntranscript_dir = \"{}\"\n[awareness]\npoll_secs = 0.2\n[wake]\ncommand = '''{}'''\ncooldown_secs = 0\n{extra}",
            self.tx().display(),
            wake.replace("{out}", &self.out().to_string_lossy()),
        );
        std::fs::write(self.m.join("shards.toml"), text).unwrap();
    }
    fn wakes(&self) -> usize {
        std::fs::read_dir(self.out())
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
                    .count()
            })
            .unwrap_or(0)
    }
    fn wake_log(&self) -> String {
        std::fs::read_to_string(self.m.join(".shards/wake.log")).unwrap_or_default()
    }
}

const RECORD: &str = r#"mkdir -p {out}; n=$(ls {out} | grep -c json); cat > {out}/wake-$n.json"#;

/// Two shards: s1 (Ada) committed a change to devices/kiln-east/ip, s2
/// (Basil) has the same fact changed but uncommitted.
fn setup() -> Setup {
    let tmp = TempDir::new().unwrap();
    let m = tmp.path().join("m");
    ok(&run(tmp.path(), &["init", m.to_str().unwrap()]));
    for (a, v) in [
        ("devices/kiln-east/ip", "10.0.4.40"),
        ("network/dns", "10.0.0.53"),
    ] {
        ok(&run(&m, &["set", a, v, "--why", "seed"]));
    }
    let mut wt = Vec::new();
    for (id, human) in [("s1", "Ada"), ("s2", "Basil")] {
        let p = ok(&run(&m, &["shard", "open", "--id", id, "--human", human]));
        wt.push(PathBuf::from(p.trim()));
    }
    ok(&cmd(
        &wt[0],
        &[
            "supersede",
            "devices/kiln-east/ip",
            "10.0.4.50",
            "--why",
            "rewired",
        ],
    )
    .env_remove("SHARDS_AUTHOR")
    .output()
    .unwrap());
    let f = wt[1].join("facts/devices/kiln-east/ip.md");
    let text = std::fs::read_to_string(&f).unwrap();
    std::fs::write(&f, text.replace("10.0.4.40", "10.0.4.60")).unwrap();
    let s = Setup { tmp, m, wt };
    s.config(RECORD, "");
    s
}

fn all_files(dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.filter_map(|e| e.ok()) {
        let p = e.path();
        if e.file_name() == "worktrees" {
            continue;
        }
        if p.is_dir() {
            all_files(&p, out);
        } else if let Ok(b) = std::fs::read(&p) {
            out.push(format!("{}:\n{}", p.display(), String::from_utf8_lossy(&b)));
        }
    }
}

// (f) nothing planted in a transcript reaches any output or state file.
#[test]
fn nothing_from_a_transcript_but_summaries_is_printed_or_stored() {
    let s = setup();
    let start = chrono::Utc::now() - chrono::Duration::minutes(30);
    for wt in &s.wt {
        fake_transcript(&s.tx(), wt, "sess-1", start).unwrap();
    }
    let mut texts = Vec::new();
    let mut outputs = Vec::new();
    for args in [
        &["board"][..],
        &["board", "--json"],
        &["watch", "--once"],
        &["watch", "--once"],
        &["core", "context"],
        &["shard", "list"],
    ] {
        let o = run(&s.m, args);
        let stdout = ok(&o);
        texts.push(format!("{args:?} stdout:\n{stdout}"));
        texts.push(format!(
            "{args:?} stderr:\n{}",
            String::from_utf8_lossy(&o.stderr)
        ));
        outputs.push(stdout);
    }
    all_files(&s.m.join(".shards"), &mut texts);
    all_files(&s.out(), &mut texts);
    for t in &texts {
        for p in PLANTED {
            assert!(!t.contains(p), "`{p}` leaked into:\n{t}");
        }
    }
    // What *is* shown: tools, basenames, redacted commands, the overlap.
    let board = &outputs[0];
    assert!(board.contains("Grep"), "{board}");
    assert!(board.contains("echo [REDACTED] | jwt-decode"), "{board}");
    let state = std::fs::read_to_string(s.m.join(".shards/activity/s1.json")).unwrap();
    assert!(state.contains("\"summary\": \"ip.md\""), "{state}");
    assert!(state.contains("Bearer [REDACTED]"), "{state}");
    assert!(board.contains("same-fact"), "{board}");
    assert!(outputs[2].contains("event 1 same-fact"), "{}", outputs[2]);
    assert_eq!(s.wakes(), 1);

    // The documented JSON format.
    let b: serde_json::Value = serde_json::from_str(&outputs[1]).unwrap();
    assert_eq!(b["format"], 1);
    assert_eq!(b["activity_source"], "claude-code");
    for key in [
        "generated",
        "memory",
        "thresholds",
        "overlaps",
        "conflicts",
        "soul_proposals",
    ] {
        assert!(b.get(key).is_some(), "missing {key}");
    }
    let s1 = &b["shards"][0];
    for key in [
        "id",
        "human",
        "branch",
        "state",
        "gist",
        "gist_at",
        "opened",
        "last_activity",
        "age_secs",
        "last_activity_from",
        "in_flight",
        "facts_in_flight",
        "recent_actions",
        "soul_pending",
    ] {
        assert!(s1.get(key).is_some(), "missing shards[].{key}");
    }
    assert_eq!(s1["recent_actions"].as_array().unwrap().len(), 5);
    assert_eq!(s1["in_flight"][0]["address"], "devices/kiln-east/ip");
    assert_eq!(b["overlaps"][0]["kind"], "same-fact");
    let ctx: serde_json::Value = serde_json::from_str(&outputs[4]).unwrap();
    assert_eq!(ctx["board"]["format"], 1);
    assert!(ctx["events"].as_array().unwrap().is_empty(), "delivered");
}

// (g) reading the board doesn't block while the main lock is held.
#[test]
fn board_and_watch_do_not_wait_for_a_held_main_lock() {
    let s = setup();
    s.config(RECORD, "[lock]\nwhen_busy = \"wait\"\ntimeout_secs = 60\n");
    let mut holder = cmd(&s.m, &["debug", "hold-lock", "--secs", "60"])
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(holder.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.starts_with("held"), "{line}");

    // The lock really is held: a fail-fast writer is refused.
    s.config(RECORD, "[lock]\nwhen_busy = \"fail\"\n");
    let w = run(&s.m, &["set", "network/ntp", "10.0.0.123", "--why", "x"]);
    assert!(!w.status.success());
    assert!(String::from_utf8_lossy(&w.stderr).contains("locked by"));
    s.config(RECORD, "[lock]\nwhen_busy = \"wait\"\ntimeout_secs = 60\n");

    for args in [
        &["board"][..],
        &["board", "--json"],
        &["watch", "--once"],
        &["core", "context"],
    ] {
        let t = Instant::now();
        ok(&run(&s.m, args));
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "{args:?} took {:?} under a held lock",
            t.elapsed()
        );
    }
    assert!(holder.try_wait().unwrap().is_none(), "holder still holds");
    assert_eq!(s.wakes(), 1, "the wake ran too, without the main lock");
    holder.kill().unwrap();
    holder.wait().unwrap();
}

// (h) a restarted watcher doesn't re-fire.
#[test]
fn watch_once_twice_is_a_restart_and_does_not_refire() {
    let s = setup();
    let first = ok(&run(&s.m, &["watch", "--once"]));
    assert!(first.contains("event 1 same-fact"), "{first}");
    assert!(first.contains("woke the core: 1 event(s)"), "{first}");
    for _ in 0..3 {
        let again = ok(&run(&s.m, &["watch", "--once"]));
        assert_eq!(again, "", "restart re-fired: {again}");
    }
    let events = std::fs::read_to_string(s.m.join(".shards/events.jsonl")).unwrap();
    assert_eq!(events.lines().count(), 1);
    assert_eq!(s.wakes(), 1);
}

// (d) a long-running watcher stays alive through failing wakes.
#[test]
fn a_running_watcher_survives_a_failing_wake_and_delivers_later() {
    let s = setup();
    s.config("cat >/dev/null; exit 7", "");
    let mut w = cmd(&s.m, &["watch"]).stdout(Stdio::null()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while s.wake_log().matches("wake FAILED").count() < 2 {
        assert!(Instant::now() < deadline, "log: {}", s.wake_log());
        assert!(w.try_wait().unwrap().is_none(), "watcher died");
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(w.try_wait().unwrap().is_none(), "watcher died");
    let ctx: serde_json::Value =
        serde_json::from_str(&ok(&run(&s.m, &["core", "context"]))).unwrap();
    assert_eq!(ctx["events"].as_array().unwrap().len(), 1, "still pending");

    // Fix the command; the running watcher picks up the new config.
    s.config(RECORD, "");
    while s.wakes() < 1 {
        assert!(Instant::now() < deadline, "log: {}", s.wake_log());
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(s.wakes(), 1, "delivered once, not again");
    assert!(w.try_wait().unwrap().is_none());
    w.kill().unwrap();
    w.wait().unwrap();
    let delivered: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(s.out().join("wake-0.json")).unwrap())
            .unwrap();
    assert_eq!(delivered[0]["id"], 1);
    assert_eq!(delivered[0]["kind"], "same-fact");
}

// (e) at the CLI: no transcripts anywhere, board still works.
#[test]
fn board_without_transcripts() {
    let s = setup();
    s.config(RECORD, "");
    std::fs::write(s.m.join("shards.toml"), "[activity]\nsource = \"none\"\n").unwrap();
    let out = ok(&run(&s.m, &["board"]));
    assert!(out.contains("s1  Ada  active"), "{out}");
    assert!(out.contains("devices/kiln-east/ip*"), "{out}");
    assert!(out.contains("same-fact"), "{out}");
}
