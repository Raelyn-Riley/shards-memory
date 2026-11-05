// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! `mem core start | status | attach | ack | rinit | usage | stop` and
//! `mem watch --once` in pane mode, as real processes. The core is the fake
//! core from `shards_core::sample`; no model is called. Tests that need tmux
//! print a note and pass without it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use shards_core::sample::write_fake_core;
use shards_core::tmux::{self, Tmux};
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
}

impl Drop for Setup {
    fn drop(&mut self) {
        Tmux::new(self.tmp.path().join("core.sock"), "core").kill_server();
    }
}

fn setup() -> Setup {
    let tmp = TempDir::new().unwrap();
    let m = tmp.path().join("m");
    ok(&run(tmp.path(), &["init", m.to_str().unwrap()]));
    let fake = write_fake_core(&tmp.path().join("bin")).unwrap();
    std::fs::create_dir_all(tmp.path().join("ctl")).unwrap();
    let text = format!(
        "[activity]\ntranscript_dir = \"{tx}\"\n[core]\ncommand = \"sh {fake} {tx} {ctl}\"\n\
         socket = \"{sock}\"\nstart_timeout_secs = 5\nexit_timeout_secs = 4\nland_timeout_secs = 3\n",
        tx = tmp.path().join("tx").display(),
        fake = fake.display(),
        ctl = tmp.path().join("ctl").display(),
        sock = tmp.path().join("core.sock").display(),
    );
    std::fs::write(m.join("shards.toml"), text).unwrap();
    ok(&run(
        &m,
        &["set", "devices/kiln-east/ip", "10.0.4.40", "--why", "seed"],
    ));
    Setup { tmp, m }
}

#[test]
fn init_writes_soul_core_and_cooldown_zero() {
    let tmp = TempDir::new().unwrap();
    let m = tmp.path().join("m");
    ok(&run(tmp.path(), &["init", m.to_str().unwrap()]));
    let soul = std::fs::read_to_string(m.join("soul/core.md")).unwrap();
    assert!(soul.contains("[HH:MM]") && soul.contains("one character"));
    assert!(soul.contains("baton") && soul.contains("mem core context"));
    let toml = std::fs::read_to_string(m.join("shards.toml")).unwrap();
    assert!(toml.contains("cooldown_secs = 0 "));
    assert!(toml.contains("[core]") && toml.contains("keep_warm = false"));
}

#[test]
fn usage_prints_numbers_by_gap_warm_and_cold() {
    let tmp = TempDir::new().unwrap();
    let m = tmp.path().join("m");
    ok(&run(tmp.path(), &["init", m.to_str().unwrap()]));
    let dir = m.join(".shards/core");
    std::fs::create_dir_all(&dir).unwrap();
    let line = |t: &str, kind: &str, gap: &str, read: u64, write: u64| {
        format!(
            "{{\"time\":\"{t}\",\"kind\":\"{kind}\",\"gap_secs\":{gap},\"idle_secs\":{gap},\"reply\":true,\"calls\":1,\
             \"usage\":{{\"input\":3,\"output\":50,\"cache_read\":{read},\"cache_creation\":{write}}},\
             \"first\":{{\"input\":3,\"output\":50,\"cache_read\":{read},\"cache_creation\":{write}}}}}\n"
        )
    };
    let mut f = std::fs::File::create(dir.join("usage.jsonl")).unwrap();
    f.write_all(line("2026-11-04T10:00:00Z", "wake", "null", 0, 8000).as_bytes())
        .unwrap();
    f.write_all(line("2026-11-04T10:01:00Z", "wake", "60", 8000, 20).as_bytes())
        .unwrap();
    f.write_all(line("2026-11-04T12:30:00Z", "wake", "8940", 0, 8100).as_bytes())
        .unwrap();
    f.write_all(line("2026-11-04T13:20:00Z", "ping", "3000", 8100, 0).as_bytes())
        .unwrap();
    drop(f);
    let out = ok(&run(&m, &["core", "usage"]));
    assert!(
        out.contains("4 entries: 3 wake(s), 1 ping(s), 4 with a reply read"),
        "{out}"
    );
    let row = |kind: &str, gap: &str, cache: &str| {
        out.lines()
            .find(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                f.len() >= 3 && f[0] == kind && f[1] == gap && f[2] == cache
            })
            .unwrap_or_else(|| panic!("no {kind} {gap} {cache} in\n{out}"))
            .to_string()
    };
    assert!(row("wake", "<5m", "warm").contains("8000"));
    assert!(row("wake", "2h+", "cold").contains("8100"));
    assert!(row("wake", "first", "cold").contains("8000"));
    row("ping", "30-60m", "warm");
    let json: serde_json::Value =
        serde_json::from_str(&ok(&run(&m, &["core", "usage", "--json"]))).unwrap();
    assert_eq!(json["entries"], 4);
    assert_eq!(json["rows"].as_array().unwrap().len(), 4);
}

#[test]
fn status_attach_and_ack_without_a_core() {
    let s = setup();
    let out = ok(&run(&s.m, &["core", "status"]));
    assert!(
        out.contains("not started") && out.contains("not pinned"),
        "{out}"
    );
    assert!(out.contains("keep-warm:      off"), "{out}");
    let a = ok(&run(&s.m, &["core", "attach"]));
    assert!(
        a.starts_with("tmux -S ") && a.contains("attach -t =core:"),
        "{a}"
    );
    let e = run(&s.m, &["core", "ack"]);
    assert!(!e.status.success());
    assert!(String::from_utf8_lossy(&e.stderr).contains("--upto"));
    // Pane mode with no core: events stay pending, nothing runs.
    std::fs::write(
        s.m.join("shards.toml"),
        std::fs::read_to_string(s.m.join("shards.toml")).unwrap() + "[wake]\nmode = \"pane\"\n",
    )
    .unwrap();
    ok(&run(&s.m, &["watch", "--once"]));
}

#[test]
fn start_wake_ack_rinit_and_stop_from_the_cli() {
    if !tmux::available() {
        eprintln!("tmux is not installed; skipping");
        return;
    }
    let s = setup();
    let out = ok(&run(&s.m, &["core", "start"]));
    assert!(out.contains("core started in its pane; session "), "{out}");
    let again = ok(&run(&s.m, &["core", "start"]));
    assert!(again.contains("already running"), "{again}");
    let st: serde_json::Value =
        serde_json::from_str(&ok(&run(&s.m, &["core", "status", "--json"]))).unwrap();
    assert_eq!(st["command_alive"], true);
    let id = st["session_id"].as_str().unwrap().to_string();

    // Two shards on the same fact: one event, one wake line in the pane.
    for sh in ["s1", "s2"] {
        let wt = ok(&run(&s.m, &["shard", "open", "--id", sh]));
        let p = Path::new(wt.trim()).join("facts/devices/kiln-east/ip.md");
        let t = std::fs::read_to_string(&p).unwrap();
        std::fs::write(&p, t.replace("10.0.4.40", &format!("10.0.9.{}", sh.len()))).unwrap();
    }
    let w = ok(&run(&s.m, &["watch", "--once"]));
    assert!(
        w.contains("woke the core in its pane: 1 event(s) up to #1"),
        "{w}"
    );
    let w = ok(&run(&s.m, &["watch", "--once"]));
    assert!(!w.contains("woke"), "{w}");
    let pane = Tmux::new(s.tmp.path().join("core.sock"), "core")
        .capture()
        .unwrap();
    assert_eq!(
        pane.matches("[WAKE] 1 event, up to #1").count(),
        1,
        "{pane}"
    );

    let a = ok(&run(&s.m, &["core", "ack"]));
    assert!(
        a.contains("acknowledged 1 event(s); 0 still pending"),
        "{a}"
    );

    let mut c = cmd(&s.m, &["core", "rinit", "--baton", "-"]);
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
    assert!(!r.contains(&id));

    let stop = ok(&run(&s.m, &["core", "stop"]));
    assert!(stop.contains("core stopped"), "{stop}");
    let chk = ok(&run(&s.m, &["core", "check"]));
    assert!(chk.contains("nothing to do"), "{chk}");
    let st = ok(&run(&s.m, &["core", "status"]));
    assert!(st.contains("pane up:        no"), "{st}");
}
