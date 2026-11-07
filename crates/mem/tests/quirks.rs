// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! `mem quirk ...` as real processes, from two shard worktrees. Fake names.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use tempfile::TempDir;

fn cmd(dir: &Path, args: &[&str]) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_mem"));
    c.arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env_remove("SHARDS_MEMORY")
        .env_remove("SHARDS_HUMAN")
        .env_remove("SHARDS_AUTHOR")
        .env_remove("SHARDS_SHARD")
        .env_remove("SHARDS_DEBUG_HOLD_MS")
        .stdin(Stdio::null());
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

fn setup() -> (TempDir, PathBuf, PathBuf, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let m = tmp.path().join("m");
    ok(&run(
        tmp.path(),
        &["init", m.to_str().unwrap(), "--author", "core"],
    ));
    let a = PathBuf::from(ok(&run(&m, &["shard", "open", "--id", "s1", "--human", "Bob"])).trim());
    let b = PathBuf::from(ok(&run(&m, &["shard", "open", "--id", "s2", "--human", "Jim"])).trim());
    (tmp, m, a, b)
}

#[test]
fn post_from_one_shard_list_from_another() {
    let (_t, m, a, b) = setup();
    let out = ok(&run(
        &a,
        &[
            "quirk",
            "add",
            "--tool",
            "bandsaw-2",
            "--quirk",
            "file handle is loose",
            "--by",
            "Bob",
            "--details",
            "hold it with your left hand",
        ],
    ));
    assert!(out.starts_with("posted q1 for bandsaw-2"), "{out}");

    let list = ok(&run(&b, &["quirk", "list", "--tool", "bandsaw-2"]));
    assert!(
        list.contains("q1  [bandsaw-2]  file handle is loose  (open)"),
        "{list}"
    );
    assert!(list.contains("reported by Bob"), "{list}");
    assert!(list.contains("hold it with your left hand"), "{list}");
    assert!(
        ok(&run(&b, &["quirk", "list", "--tool", "lathe"])).contains("no open quirks for lathe")
    );

    ok(&run(
        &b,
        &[
            "quirk",
            "update",
            "q1",
            "--status",
            "worse when cold",
            "--by",
            "Jim",
        ],
    ));
    ok(&run(&b, &["quirk", "fix", "1", "--by", "Ada"]));
    assert!(ok(&run(&a, &["quirk", "list"])).contains("no open quirks"));
    let all = ok(&run(&a, &["quirk", "list", "--all"]));
    assert!(all.contains("status: fixed by Ada"), "{all}");

    let h = ok(&run(&m, &["quirk", "history", "q1"]));
    let lines: Vec<&str> = h.lines().collect();
    assert_eq!(lines.len(), 3, "{h}");
    assert!(
        lines[0].contains("quirk-fix") && lines[0].contains("by Ada (via s2)"),
        "{h}"
    );
    assert!(
        lines[1].contains("worse when cold") && lines[1].contains("by Jim"),
        "{h}"
    );
    assert!(
        lines[2].contains("quirk-add") && lines[2].contains("by Bob (via s1)"),
        "{h}"
    );

    ok(&run(&a, &["quirk", "mention", "q1", "--by", "Jim"]));
    // Fixed a moment ago: not archived at the default 30 days, archived at 0.
    assert!(ok(&run(&m, &["quirk", "archive", "--author", "core"])).contains("nothing to archive"));
    let ar = ok(&run(
        &m,
        &["quirk", "archive", "--days", "0", "--author", "core"],
    ));
    assert!(ar.contains("archived q1"), "{ar}");
    let json: serde_json::Value =
        serde_json::from_str(&ok(&run(&b, &["quirk", "list", "--all", "--json"]))).unwrap();
    assert_eq!(json[0]["state"], "archived");

    // Main's own author rule still applies.
    assert!(!run(
        &m,
        &["quirk", "add", "--tool", "x", "--quirk", "y", "--by", "Bob"]
    )
    .status
    .success());
}

#[test]
fn two_shards_adding_at_the_same_moment_lose_nothing() {
    let (_t, m, a, b) = setup();
    let mut kids = Vec::new();
    for (dir, who) in [(&a, "Bob"), (&b, "Jim"), (&a, "Bob"), (&b, "Jim")] {
        for i in 0..3 {
            kids.push(
                cmd(
                    dir,
                    &[
                        "quirk",
                        "add",
                        "--tool",
                        "drill-press",
                        "--quirk",
                        &format!("{who} report {i} from {}", dir.display()),
                        "--by",
                        who,
                    ],
                )
                // Hold the lock a little so the writers really overlap.
                .env("SHARDS_DEBUG_HOLD_MS", "40")
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
            );
        }
    }
    for k in kids {
        ok(&k.wait_with_output().unwrap());
    }
    let json: serde_json::Value =
        serde_json::from_str(&ok(&run(&m, &["quirk", "list", "--json"]))).unwrap();
    let ids: std::collections::BTreeSet<String> = json
        .as_array()
        .unwrap()
        .iter()
        .map(|q| q["id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(ids.len(), 12, "{json}");
    assert!((1..=12).all(|n| ids.contains(&format!("q{n}"))));
}

#[test]
fn in_a_shard_pane_the_author_is_the_shard() {
    // A shard's pane points SHARDS_MEMORY at the main root and sets SHARDS_SHARD.
    let (_t, m, _a, _b) = setup();
    let o = cmd(
        &m,
        &[
            "quirk",
            "add",
            "--tool",
            "jointer",
            "--quirk",
            "fence creeps",
            "--by",
            "Bob",
        ],
    )
    .env("SHARDS_SHARD", "s1")
    .output()
    .unwrap();
    ok(&o);
    let h = ok(&run(&m, &["quirk", "history", "q1"]));
    assert!(h.contains("by Bob (via s1)"), "{h}");
}
