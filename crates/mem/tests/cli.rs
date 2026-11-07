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

/// The `shards.toml` that `mem init` wrote before brief 10: durations in
/// their old sections, no `[intervals]`.
const OLD_INIT_TOML: &str = r#"# shards configuration. Edit freely; no code changes needed.

[lock]
scope = "store"        # "store" (one lock for main) | "section" (one per top-level section)
when_busy = "wait"     # "wait" (queue up to timeout_secs) | "fail" (fail fast)
timeout_secs = 30
lease_secs = 30        # a lock not renewed for this long, whose process is gone, is recovered

[soul]
admins = []            # humans allowed to approve soul changes, e.g. ["Ada"]

[shards]
keep_branches = true   # keep shard/<id> after a clean dinit (history); false deletes it
worktree_dir = ".shards/worktrees"
command = "claude"     # typed into each shard's pane by `mem shard start`
adapter = "claude-code" # "claude-code": pinned --session-id, --resume <id> | "plain"
socket = ".shards/sessions/tmux.sock"  # the shards' one tmux server
claude_config = ""     # empty: ~/.claude.json (read to check the memory root is trusted)

[activity]
source = "claude-code" # "claude-code" (read session transcripts) | "none" (git only)
transcript_dir = ""    # empty: ~/.claude/projects (or $CLAUDE_CONFIG_DIR/projects)

[awareness]
idle_secs = 600        # no activity for this long: idle
stale_secs = 3600      # no activity for this long: stale
poll_secs = 30         # how often `mem watch` looks

[triggers]
same_fact = true        # two open shards have the same fact in flight
same_section = false    # two open shards are in flight in the same section
stale_with_work = true  # stale shard with facts in flight
open_conflict = true    # a conflict record waits on main
soul_proposal = true    # a soul proposal waits for an admin

[wake]
mode = "auto"          # "auto": "pane" once `mem core start` has run, else "command" | "command" | "pane"
command = ""           # command mode: run this (sh -c, events as JSON on stdin); empty: record only
timeout_secs = 300
cooldown_secs = 0      # wake the core at most once per this long; 0: no rate limit

[core]                 # the reasoning core: one long-lived session in a tmux pane
command = "claude"     # typed into the pane's shell by `mem core start`
adapter = "claude-code" # "claude-code" (session ids, --resume <id>) | "plain"
socket = ".shards/core/tmux.sock"  # its own tmux server
session = "core"
cwd = ""               # empty: the memory root
watchdog = true        # mem watch restores a dead core and resumes its pinned session
start_timeout_secs = 60
exit_timeout_secs = 30
land_timeout_secs = 10
keep_warm = false      # ping an idle core to keep its prompt cache warm (for measuring)
keep_warm_mins = 50
keep_warm_quiet_limit = 8  # stop after this many pings with no real activity
"#;

/// Brief 10: `mem init` writes every duration in `[intervals]` and none in an
/// old place; a file in the old layout still loads and is still read.
#[test]
fn init_writes_intervals_and_old_files_still_load() {
    let t = TempDir::new().unwrap();
    let m = t.path().join("m");
    ok(&mem(t.path(), &["init", m.to_str().unwrap()], None));
    let toml = std::fs::read_to_string(m.join("shards.toml")).unwrap();
    assert!(toml.contains("\n[intervals]"), "{toml}");
    for k in [
        "lock_timeout_secs = 30",
        "lock_lease_secs = 30",
        "idle_secs = 600",
        "stale_secs = 3600",
        "watch_poll_secs = 30",
        "wake_timeout_secs = 300",
        "core_cooldown_secs = 0",
        "core_start_timeout_secs = 60",
        "core_exit_timeout_secs = 30",
        "core_land_timeout_secs = 10",
        "keep_warm_mins = 50",
        "quirk_archive_days = 90",
    ] {
        assert!(
            toml.lines().any(|l| l.starts_with(k)),
            "{k} missing:\n{toml}"
        );
    }
    for old in [
        "timeout_secs =",
        "lease_secs =",
        "poll_secs =",
        "cooldown_secs =",
        "archive_after_days =",
        "[awareness]",
        "[quirks]",
    ] {
        assert!(
            !toml.lines().any(|l| l.starts_with(old)),
            "{old} in:\n{toml}"
        );
    }
    let archive = || ok(&mem(&m, &["quirk", "archive"], None));
    assert!(archive().contains("quiet for 90 day(s)"), "{}", archive());
    let edited = toml.replace("quirk_archive_days = 90", "quirk_archive_days = 30");
    std::fs::write(m.join("shards.toml"), edited).unwrap();
    assert!(archive().contains("quiet for 30 day(s)"), "{}", archive());

    // The old layout, as an older `mem init` wrote it, still loads...
    std::fs::write(m.join("shards.toml"), OLD_INIT_TOML).unwrap();
    assert!(archive().contains("quiet for 90 day(s)"), "{}", archive());
    ok(&mem(
        &m,
        &["put", "notes/kiln.md", "--why", "old toml"],
        Some("kiln at 6\n"),
    ));
    // ...and an old-place duration in it is still read.
    std::fs::write(
        m.join("shards.toml"),
        format!("{OLD_INIT_TOML}[quirks]\narchive_after_days = 7\n"),
    )
    .unwrap();
    assert!(archive().contains("quiet for 7 day(s)"), "{}", archive());
}
