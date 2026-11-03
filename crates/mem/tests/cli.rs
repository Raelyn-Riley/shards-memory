// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! End-to-end tests of the `mem` binary.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

fn mem(dir: &Path, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_mem"))
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("SHARDS_AUTHOR", "shard-cli")
        .env("SHARDS_HUMAN", "Tess")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let Some(s) = stdin {
        child.stdin.take().unwrap().write_all(s.as_bytes()).unwrap();
    }
    drop(child.stdin.take());
    child.wait_with_output().unwrap()
}

fn ok(o: &Output) -> String {
    assert!(
        o.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&o.stderr)
    );
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    assert!(
        !o.status.success(),
        "expected failure, stdout: {}",
        String::from_utf8_lossy(&o.stdout)
    );
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn the_whole_slice_through_the_cli() {
    let t = TempDir::new().unwrap();
    let m = t.path().join("m");
    ok(&mem(t.path(), &["init", m.to_str().unwrap()], None));
    for d in ["facts", "soul", "people"] {
        assert!(m.join(d).is_dir());
    }
    assert!(m.join("README.md").is_file());

    ok(&mem(
        &m,
        &[
            "set",
            "devices/rotoforge-ctl/ip",
            "10.0.4.40",
            "--why",
            "first",
        ],
        None,
    ));
    let e = err(&mem(
        &m,
        &["set", "devices/rotoforge-ctl/ip", "10.0.4.41", "--why", "x"],
        None,
    ));
    assert!(e.contains("already exists"), "{e}");
    // Same name elsewhere is a different fact; only the exact address is refused.
    ok(&mem(
        &m,
        &["set", "devices/other/ip", "10.0.4.41", "--why", "x"],
        None,
    ));

    let out = ok(&mem(&m, &["get", "devices/rotoforge-ctl/ip"], None));
    assert_eq!(out, "10.0.4.40\nhome: facts/devices/rotoforge-ctl/ip.md\n");
    let e = err(&mem(&m, &["get", "devices/nope/ip"], None));
    assert!(e.contains("devices/nope/ip"), "{e}");

    ok(&mem(
        &m,
        &[
            "supersede",
            "devices/rotoforge-ctl/ip",
            "10.0.4.42",
            "--why",
            "new switch",
        ],
        None,
    ));
    let h = ok(&mem(&m, &["history", "devices/rotoforge-ctl/ip"], None));
    let new_at = h.find("10.0.4.42").unwrap();
    let old_at = h.find("10.0.4.40").unwrap();
    assert!(new_at < old_at, "newest first:\n{h}");
    assert!(
        h.contains("author: shard-cli")
            && h.contains("human:  Tess")
            && h.contains("why:    new switch")
    );

    let note = "ctl {{fact:devices/rotoforge-ctl/ip}}\n";
    assert_eq!(ok(&mem(&m, &["resolve"], Some(note))), "ctl 10.0.4.42\n");
    assert_eq!(
        ok(&mem(&m, &["resolve", "--annotate", "-"], Some(note))),
        "ctl 10.0.4.42 [devices/rotoforge-ctl/ip]\n"
    );
    let e = err(&mem(&m, &["resolve"], Some("{{fact:devices/ghost/ip}}")));
    assert!(e.contains("{{fact:devices/ghost/ip}}"), "{e}");

    // Value from stdin.
    ok(&mem(
        &m,
        &["set", "notes-src/multi", "-", "--why", "stdin"],
        Some("a\nb\n"),
    ));
    assert_eq!(
        ok(&mem(&m, &["get", "notes-src/multi"], None)),
        "a\nb\nhome: facts/notes-src/multi.md\n"
    );
}

#[test]
fn writes_need_an_author() {
    let t = TempDir::new().unwrap();
    let m = t.path().join("m");
    ok(&mem(t.path(), &["init", m.to_str().unwrap()], None));
    let o = Command::new(env!("CARGO_BIN_EXE_mem"))
        .args(["-C", m.to_str().unwrap(), "set", "a/b", "1", "--why", "w"])
        .env_remove("SHARDS_AUTHOR")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(err(&o).contains("SHARDS_AUTHOR"));
}

#[test]
fn gen_sample_builds_a_memory() {
    let t = TempDir::new().unwrap();
    let s = t.path().join("s");
    let out = ok(&mem(
        t.path(),
        &["gen-sample", s.to_str().unwrap(), "--seed", "3"],
        None,
    ));
    assert!(out.contains("30 facts"), "{out}");
    let r = ok(&mem(
        &s,
        &["resolve", s.join("notes/network.md").to_str().unwrap()],
        None,
    ));
    assert!(r.contains("Subnet 10.") && !r.contains("{{fact:"), "{r}");
}

/// The CLI is thin: it never talks to git itself, so every write goes through
/// shards-core (whose git wrapper refuses history-rewriting commands).
#[test]
fn cli_does_not_call_git_directly() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs")).unwrap();
    assert!(
        !src.contains("Command::new"),
        "mem must not spawn processes itself"
    );
}
