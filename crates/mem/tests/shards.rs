// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! `mem shard start | list | send | attach | stop | start --resume`,
//! `mem board` and a clean `mem dinit`, as real processes. Each shard's
//! session is the fake core from `shards_core::sample`; no model is called.
//! Without tmux the test prints a note and passes.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use shards_core::sample::write_fake_core;
use shards_core::tmux::{self, Tmux};
use tempfile::TempDir;

fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mem"))
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("SHARDS_MEMORY")
        .env_remove("SHARDS_HUMAN")
        .env_remove("SHARDS_AUTHOR")
        .stdin(Stdio::null())
        .output()
        .unwrap()
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
        Tmux::new(self.tmp.path().join("shards.sock"), "x").kill_server();
    }
}

fn setup() -> Setup {
    let tmp = TempDir::new().unwrap();
    let m = tmp.path().join("m");
    ok(&run(
        tmp.path(),
        &["init", m.to_str().unwrap(), "--author", "core"],
    ));
    let fake = write_fake_core(&tmp.path().join("bin")).unwrap();
    std::fs::create_dir_all(tmp.path().join("ctl")).unwrap();
    let root = std::fs::canonicalize(&m).unwrap();
    let cc = tmp.path().join("claude.json");
    std::fs::write(
        &cc,
        serde_json::json!({"projects": {root.to_string_lossy(): {"hasTrustDialogAccepted": true}}})
            .to_string(),
    )
    .unwrap();
    let text = format!(
        "[activity]\ntranscript_dir = \"{tx}\"\n\
         [shards]\ncommand = \"sh {fake} {tx} {ctl}\"\nsocket = \"{sock}\"\nclaude_config = \"{cc}\"\n\
         [core]\nstart_timeout_secs = 5\nexit_timeout_secs = 4\nland_timeout_secs = 3\n",
        tx = tmp.path().join("tx").display(),
        fake = fake.display(),
        ctl = tmp.path().join("ctl").display(),
        sock = tmp.path().join("shards.sock").display(),
        cc = cc.display(),
    );
    std::fs::write(m.join("shards.toml"), text).unwrap();
    Setup { tmp, m }
}

fn list(m: &Path) -> serde_json::Value {
    serde_json::from_str(&ok(&run(m, &["shard", "list", "--json"]))).unwrap()
}

#[test]
fn start_talk_stop_resume_and_close_from_the_cli() {
    if !tmux::available() {
        eprintln!("tmux is not installed; skipping");
        return;
    }
    let s = setup();
    let m = &s.m;
    let out = ok(&run(m, &["shard", "start", "--human", "Ada"]));
    assert!(
        out.starts_with("shard s1 started in its pane; session "),
        "{out}"
    );
    assert!(out.contains("attach: tmux -S "), "{out}");
    ok(&run(
        m,
        &["shard", "start", "--id", "s2", "--human", "Basil"],
    ));

    let l = list(m);
    let rows = l["shards"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["pane"], "up");
    assert_eq!(rows[1]["human"], "Basil");
    let sid = rows[0]["session"]["session_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(rows[0]["transcript"].is_null());

    assert_eq!(
        ok(&run(
            m,
            &["shard", "send", "--id", "s1", "the heron is back"]
        ))
        .trim(),
        "sent to s1"
    );
    let l = list(m);
    let tx = l["shards"][0]["transcript"].as_str().unwrap().to_string();
    assert!(tx.ends_with(&format!("{sid}.jsonl")));
    assert!(l["shards"][1]["transcript"].is_null());

    let board = ok(&run(m, &["board"]));
    assert!(board.contains("s1  Ada  active  pane up"), "{board}");
    let bj: serde_json::Value = serde_json::from_str(&ok(&run(m, &["board", "--json"]))).unwrap();
    assert_eq!(bj["shards"][0]["pane"], "up");
    assert_eq!(bj["shards"][0]["session_id"], sid.as_str());

    let att = ok(&run(m, &["shard", "attach", "--id", "s1"]));
    assert!(att.trim().ends_with("attach -t =s1:"), "{att}");

    let out = ok(&run(m, &["shard", "stop", "--id", "s1"]));
    assert!(out.contains("--resume"), "{out}");
    assert_eq!(list(m)["shards"][0]["pane"], "down");
    assert!(ok(&run(m, &["board"])).contains("pane down"));
    let o = run(m, &["shard", "send", "--id", "s1", "hello?"]);
    assert!(!o.status.success());

    let out = ok(&run(m, &["shard", "start", "--id", "s1", "--resume"]));
    assert!(
        out.starts_with("shard s1 resumed in its pane; session ") && out.contains(&sid),
        "{out}"
    );
    // `-` reads the message from stdin; its line breaks fold into one line.
    let mut c = Command::new(env!("CARGO_BIN_EXE_mem"))
        .arg("-C")
        .arg(m)
        .args(["shard", "send", "--id", "s1", "-"])
        .env_remove("SHARDS_MEMORY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut c.stdin.take().unwrap(), b"still\nthere?\n").unwrap();
    ok(&c.wait_with_output().unwrap());
    let text = std::fs::read_to_string(&tx).unwrap();
    assert_eq!(text.matches("\"type\":\"user\"").count(), 2, "{text}");
    assert!(text.contains("\"content\":\"still there?\""), "{text}");

    let out = ok(&run(m, &["dinit", "--id", "s2"]));
    assert!(
        out.contains("shard s2's session was stopped first"),
        "{out}"
    );
    assert!(out.contains("shard s2 closed"), "{out}");
    let l = list(m);
    assert_eq!(l["shards"].as_array().unwrap().len(), 1);
    assert_eq!(l["shards"][0]["pane"], "up");
}

#[test]
fn an_untrusted_root_is_refused_with_how_to_fix_it() {
    if !tmux::available() {
        eprintln!("tmux is not installed; skipping");
        return;
    }
    let s = setup();
    std::fs::write(s.tmp.path().join("claude.json"), r#"{"projects":{}}"#).unwrap();
    let o = run(&s.m, &["shard", "start", "--human", "Ada"]);
    assert!(!o.status.success());
    let err = String::from_utf8_lossy(&o.stderr);
    assert!(
        err.contains("doesn't trust") && err.contains("once"),
        "{err}"
    );
    assert_eq!(ok(&run(&s.m, &["shard", "list"])).trim(), "no open shards");
}
