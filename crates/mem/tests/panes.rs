// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Brief 08: a shard's writes stay on its own branch until dinit, when they
//! are run the way a shard's agent runs them, from its pane.
//!
//! Each shard is started for real (`mem shard start`) on the default
//! `[shards] command`, `claude`. That `claude` is a stand-in found only in
//! `$HOME/.local/bin` (the launcher's PATH lacks it, as on a real machine): it
//! records the exact environment the pane gave it, then runs the fake core
//! from `shards_core::sample`, so no model is called. `mem` is then run with
//! that environment and nothing else, in the pane's directory. One test runs
//! `mem dinit` inside the pane itself. Without tmux the tests pass with a note.
//!
//! Brief 09 adds notes with owners and `[intervals]`, run the same way.
//! Brief 10 adds people in any case and a shard finding main's newer notes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use shards_core::sample::write_fake_core;
use shards_core::tmux::{self, Tmux};
use tempfile::TempDir;

/// The launcher's PATH: system dirs only, no `~/.local/bin`, no `mem`.
const BARE_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

struct World {
    tmp: TempDir,
    m: PathBuf,
}

impl Drop for World {
    fn drop(&mut self) {
        Tmux::new(self.tmp.path().join("shards.sock"), "x").kill_server();
    }
}

impl World {
    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }
    fn ctl(&self) -> PathBuf {
        self.tmp.path().join("ctl")
    }

    /// `mem` as a person at a terminal runs it: in the main root.
    fn mem(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_mem"))
            .args(args)
            .current_dir(if self.m.exists() {
                &self.m
            } else {
                self.tmp.path()
            })
            .env_clear()
            .env("PATH", BARE_PATH)
            .env("HOME", self.home())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("SHARDS_TMUX_SCOPE", "0")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    /// Start a shard on the default command and return its pane's
    /// environment, exactly as the pane gave it to `claude`.
    fn start(&self, id: &str, human: &str) -> BTreeMap<String, String> {
        ok(&self.mem(&["shard", "start", "--id", id, "--human", human]));
        let p = self.ctl().join(format!("env.{id}"));
        let start = Instant::now();
        while !p.exists() {
            assert!(start.elapsed() < Duration::from_secs(5), "no env from {id}");
            std::thread::sleep(Duration::from_millis(50));
        }
        let raw = std::fs::read(p).unwrap();
        raw.split(|b| *b == 0)
            .filter(|kv| !kv.is_empty())
            .filter_map(|kv| {
                let kv = String::from_utf8_lossy(kv);
                kv.split_once('=').map(|(k, v)| (k.into(), v.into()))
            })
            .collect()
    }

    /// `mem` exactly as the shard's agent runs it: the pane's environment
    /// and nothing more, in the pane's directory, found on the pane's PATH.
    fn pane(&self, env: &BTreeMap<String, String>, args: &[&str]) -> Output {
        Command::new("mem")
            .args(args)
            .current_dir(&env["PWD"])
            .env_clear()
            .envs(env)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn main_head(&self) -> String {
        git(&self.m, &["rev-parse", "main"])
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let o = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    String::from_utf8_lossy(&o.stdout).trim().to_string()
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

fn fails(o: &Output) -> String {
    assert!(
        !o.status.success(),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// `claude` in `$HOME/.local/bin`: save the environment, run any script
/// dropped at `ctl/run.<shard>` from inside the pane, and be the fake core.
const STAND_IN: &str = r#"#!/bin/sh
ctl=@CTL@
env -0 > "$ctl/env.$SHARDS_SHARD"
(
  end=$(( $(date +%s) + 120 ))
  while [ -d "$ctl" ] && [ "$(date +%s)" -lt "$end" ]; do
    f="$ctl/run.$SHARDS_SHARD"
    if [ -f "$f" ]; then
      mv "$f" "$f.go"
      sh "$f.go" > "$ctl/out.$SHARDS_SHARD" 2>&1
      echo $? > "$ctl/rc.$SHARDS_SHARD"
    fi
    sleep 0.1
  done
) &
exec sh @FAKE@ @TX@ "$ctl" "$@"
"#;

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

    ok(&w.mem(&["init", m.to_str().unwrap(), "--author", "core"]));
    let root = std::fs::canonicalize(&m).unwrap();
    let cc = w.tmp.path().join("claude.json");
    std::fs::write(
        &cc,
        serde_json::json!({"projects": {root.to_string_lossy(): {"hasTrustDialogAccepted": true}}})
            .to_string(),
    )
    .unwrap();
    // `[shards] command` is left at its default, `claude`.
    let toml = format!(
        "[activity]\ntranscript_dir = \"{}\"\n[shards]\nsocket = \"{}\"\nclaude_config = \"{}\"\n",
        tx.display(),
        w.tmp.path().join("shards.sock").display(),
        cc.display()
    );
    std::fs::write(m.join("shards.toml"), toml).unwrap();
    git(
        &m,
        &[
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@t",
            "commit",
            "-qam",
            "test config",
        ],
    );
    ok(&w.mem(&[
        "set",
        "devices/kiln-east/ip",
        "10.0.4.40",
        "--why",
        "seed",
        "--author",
        "core",
    ]));
    Some(w)
}

#[test]
fn the_default_claude_starts_from_home_local_bin_and_the_pane_knows_its_shard() {
    let Some(w) = world() else { return };
    let env = w.start("s1", "Ada");
    let wt = std::fs::canonicalize(w.m.join(".shards/worktrees/s1")).unwrap();
    assert_eq!(Path::new(&env["SHARDS_MEMORY"]), wt);
    assert_eq!(Path::new(&env["PWD"]), wt);
    assert_eq!(env["SHARDS_SHARD"], "s1");
    assert_eq!(env["SHARDS_AUTHOR"], "s1");
    assert_eq!(env["SHARDS_HUMAN"], "");
    let path = &env["PATH"];
    assert!(
        path.contains(&*w.home().join(".local/bin").to_string_lossy()),
        "{path}"
    );
    let mem_dir = Path::new(env!("CARGO_BIN_EXE_mem")).parent().unwrap();
    assert!(path.contains(&*mem_dir.to_string_lossy()), "{path}");
    assert!(ok(&w.mem(&["shard", "list"])).contains("pane: up"));
}

#[test]
fn a_write_from_the_pane_stays_on_its_branch_until_dinit() {
    let Some(w) = world() else { return };
    let a = w.start("s1", "Ada");
    let b = w.start("s2", "Basil");
    let main0 = w.main_head();

    // set, supersede and put from s1's pane, with no --author or --human.
    let out = ok(&w.pane(
        &a,
        &[
            "set",
            "birds/heron/perch",
            "north post",
            "--why",
            "seen there",
        ],
    ));
    assert!(out.starts_with("set birds/heron/perch"), "{out}");
    ok(&w.pane(
        &a,
        &[
            "supersede",
            "devices/kiln-east/ip",
            "10.0.4.41",
            "--why",
            "new switch",
        ],
    ));
    let note = Path::new(&a["PWD"]).join("visit.md");
    std::fs::write(&note, "The heron came at dawn.\n").unwrap();
    ok(&w.pane(
        &a,
        &[
            "put",
            "notes/visit.md",
            note.to_str().unwrap(),
            "--why",
            "log",
        ],
    ));
    std::fs::remove_file(&note).unwrap();

    assert_eq!(w.main_head(), main0, "main moved");
    let log = git(
        &w.m,
        &[
            "log",
            "--format=%an|%(trailers:key=Shards-Human,valueonly)",
            "-3",
            "shard/s1",
        ],
    );
    for line in log.lines().filter(|l| !l.trim().is_empty()) {
        assert!(line.starts_with("s1|Ada"), "{log}");
    }

    // s1 reads its own branch.
    assert!(ok(&w.pane(&a, &["get", "birds/heron/perch"])).starts_with("north post\n"));
    assert!(ok(&w.pane(&a, &["get", "devices/kiln-east/ip"])).starts_with("10.0.4.41\n"));
    let h = ok(&w.pane(&a, &["history", "devices/kiln-east/ip"]));
    assert!(h.contains("author: s1") && h.contains("human:  Ada"), "{h}");
    let mut c = Command::new("mem")
        .args(["resolve", "-"])
        .current_dir(&a["PWD"])
        .env_clear()
        .envs(&a)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(
        &mut c.stdin.take().unwrap(),
        b"at {{fact:birds/heron/perch}}",
    )
    .unwrap();
    assert_eq!(ok(&c.wait_with_output().unwrap()), "at north post");

    // s2 and main can't see it.
    fails(&w.pane(&b, &["get", "birds/heron/perch"]));
    assert!(ok(&w.pane(&b, &["get", "devices/kiln-east/ip"])).starts_with("10.0.4.40\n"));
    fails(&w.mem(&["get", "birds/heron/perch"]));

    // The commands that need main still find it from the pane.
    let board = ok(&w.pane(&b, &["board", "--json"]));
    assert!(board.contains("birds/heron/perch"), "{board}");
    assert!(ok(&w.pane(&b, &["shard", "list"])).contains("s1  human: Ada"));
    ok(&w.pane(&b, &["shard", "status", "--id", "s2", "reading"]));
    let ctx = ok(&w.pane(&b, &["core", "context"]));
    assert!(ctx.contains("\"shards\""), "{ctx}");
    assert!(ok(&w.pane(&b, &["core", "status"])).contains("core"));

    // Dinit, from s2's pane: s1's note lands on main, its facts wait for
    // the core in a proposal.
    let out = ok(&w.pane(&b, &["dinit", "--id", "s1"]));
    assert!(out.contains("merged    notes/visit.md"), "{out}");
    assert!(
        out.contains("PROPOSED  facts/birds/heron/perch.md  (p1"),
        "{out}"
    );
    assert!(out.contains("shard s1 closed"), "{out}");
    assert_ne!(w.main_head(), main0);
    fails(&w.mem(&["get", "birds/heron/perch"]));
    ok(&w.mem(&["proposal", "accept", "p1", "--why", "both look right"]));
    assert!(ok(&w.mem(&["get", "birds/heron/perch"])).starts_with("north post\n"));
    let h = ok(&w.mem(&["history", "birds/heron/perch"]));
    assert!(h.contains("author: s1") && h.contains("human:  Ada"), "{h}");
    assert!(h.contains("review: core accepted it from p1#"), "{h}");
    // s2 branched before the dinit: still its own view until it dinits.
    fails(&w.pane(&b, &["get", "birds/heron/perch"]));
}

#[test]
fn the_same_fact_changed_in_two_panes_is_a_recorded_conflict() {
    let Some(w) = world() else { return };
    let a = w.start("s1", "Ada");
    let b = w.start("s2", "Basil");
    ok(&w.pane(
        &a,
        &[
            "supersede",
            "devices/kiln-east/ip",
            "10.0.4.41",
            "--why",
            "A's switch",
        ],
    ));
    ok(&w.pane(
        &b,
        &[
            "supersede",
            "devices/kiln-east/ip",
            "10.0.4.99",
            "--why",
            "B's switch",
        ],
    ));
    let main0 = w.main_head();
    assert!(ok(&w.mem(&["get", "devices/kiln-east/ip"])).starts_with("10.0.4.40\n"));

    let out = ok(&w.mem(&["dinit", "--id", "s1"]));
    assert!(
        out.contains("PROPOSED  facts/devices/kiln-east/ip.md"),
        "{out}"
    );
    ok(&w.mem(&["proposal", "accept", "p1", "--why", "A's switch is live"]));
    assert_ne!(w.main_head(), main0);
    let out = ok(&w.pane(&b, &["dinit", "--id", "s2"]));
    assert!(out.contains("CONFLICT  devices/kiln-east/ip"), "{out}");
    assert!(out.contains("shard: 10.0.4.99  (by s2 / Basil"), "{out}");
    assert!(ok(&w.mem(&["get", "devices/kiln-east/ip"])).starts_with("10.0.4.41\n"));
    assert!(w.m.join("conflicts/s2/devices/kiln-east/ip.md").exists());
}

#[test]
fn a_quirk_from_the_pane_goes_straight_to_main() {
    let Some(w) = world() else { return };
    let a = w.start("s1", "Ada");
    let b = w.start("s2", "Basil");
    let main0 = w.main_head();
    let out = ok(&w.pane(
        &a,
        &[
            "quirk",
            "add",
            "--tool",
            "bandsaw-2",
            "--quirk",
            "loose handle",
            "--by",
            "Ada",
        ],
    ));
    assert!(out.contains("q1"), "{out}");
    assert_ne!(w.main_head(), main0, "a quirk goes to main");
    assert!(ok(&w.pane(&b, &["quirk", "list"])).contains("loose handle"));
    let h = ok(&w.pane(&b, &["quirk", "history", "q1"]));
    assert!(h.contains("s1"), "{h}");
}

/// The shard's agent dinits its own shard: the merge lands, the shard
/// closes, and only then does its session end.
#[test]
fn a_shard_dinits_itself_from_inside_its_pane() {
    let Some(w) = world() else { return };
    let a = w.start("s1", "Ada");
    ok(&w.pane(
        &a,
        &[
            "set",
            "birds/heron/perch",
            "north post",
            "--why",
            "seen there",
        ],
    ));
    let main0 = w.main_head();
    std::fs::write(w.ctl().join("run.s1"), "mem dinit --id s1\n").unwrap();

    let start = Instant::now();
    loop {
        let list = ok(&w.mem(&["shard", "list"]));
        if list.trim() == "no open shards"
            && !Tmux::new(w.tmp.path().join("shards.sock"), "s1").has_session()
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "not closed: {list}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_ne!(w.main_head(), main0);
    // The fact waits for the core; accepted, it is on main.
    fails(&w.mem(&["get", "birds/heron/perch"]));
    ok(&w.mem(&["proposal", "accept", "p1", "--why", "seen there too"]));
    assert!(ok(&w.mem(&["get", "birds/heron/perch"])).starts_with("north post\n"));
    assert!(!w.m.join(".shards/worktrees/s1").exists());
    assert!(
        !w.m.join(".shards/locks/shard.s1.lock").exists(),
        "shard lock left behind"
    );
    let log = std::fs::read_to_string(w.m.join(".shards/sessions/sessions.log")).unwrap();
    assert!(
        log.contains("s1: its own clean dinit closed it; session ended"),
        "{log}"
    );
    let out = std::fs::read_to_string(w.ctl().join("out.s1")).unwrap_or_default();
    assert!(out.is_empty() || out.contains("dinit s1 -> main"), "{out}");
}

/// `mem` from a pane with a note body in a file in the pane's directory.
fn put_from_pane(
    w: &World,
    env: &BTreeMap<String, String>,
    path: &str,
    text: &str,
    extra: &[&str],
) -> String {
    let f = Path::new(&env["PWD"]).join("body.md");
    std::fs::write(&f, text).unwrap();
    let mut args = vec!["put", path, f.to_str().unwrap(), "--why", "log"];
    args.extend_from_slice(extra);
    let out = ok(&w.pane(env, &args));
    std::fs::remove_file(&f).unwrap();
    out
}

#[test]
fn notes_with_owners_from_the_pane_ride_the_branch_and_find_ranks_them() {
    let Some(w) = world() else { return };
    let a = w.start("s1", "Ada");
    let b = w.start("s2", "Basil");
    let main0 = w.main_head();

    let out = put_from_pane(
        &w,
        &a,
        "notes/kiln.md",
        "Kiln east runs hot.\n",
        &["--for", "ada"],
    );
    assert!(out.starts_with("wrote notes/ada/kiln.md"), "{out}");
    let out = put_from_pane(&w, &a, "notes/runbook.md", "Check the kiln vent.\n", &[]);
    assert!(out.starts_with("wrote notes/runbook.md"), "{out}");
    let out = put_from_pane(
        &w,
        &b,
        "kiln.md",
        "Basil: kiln shelf cracked.\n",
        &["--for", "basil"],
    );
    assert!(out.starts_with("wrote notes/basil/kiln.md"), "{out}");
    // A person's note can't be put outside notes/.
    let e = fails(&w.pane(
        &a,
        &["put", "people/ada.md", "-", "--why", "x", "--for", "ada"],
    ));
    assert!(e.contains("notes/<person>/"), "{e}");

    // On each shard's own branch only.
    assert_eq!(w.main_head(), main0, "main moved");
    let mine = ok(&w.pane(&a, &["notes", "find", "kiln", "--for", "ada"]));
    assert!(
        mine.contains("notes/ada/kiln.md  (ada's own note, "),
        "{mine}"
    );
    assert!(!mine.contains("basil"), "{mine}");
    assert!(ok(&w.mem(&["notes", "find", "kiln", "--for", "ada"])).contains("no notes mention"));

    // Both dinit with no core review: merged, nothing held.
    for id in ["s1", "s2"] {
        let out = ok(&w.mem(&["dinit", "--id", id]));
        assert!(out.contains("merged    notes/"), "{out}");
        assert!(
            !out.contains("PROPOSAL") && !out.contains("CONFLICT"),
            "{out}"
        );
        assert!(out.contains(&format!("shard {id} closed")), "{out}");
    }

    // From a fresh pane: Basil's own first, then shared, then everyone else's,
    // each named with whose it is and its date. The kiln fact is not a note.
    let c = w.start("s3", "Basil");
    let out = ok(&w.pane(&c, &["notes", "find", "kiln", "--for", "basil"]));
    let day = |p: &str| git(&w.m, &["log", "-1", "--format=%as", "main", "--", p]);
    let want = [
        format!(
            "notes/basil/kiln.md  (basil's own note, {})",
            day("notes/basil/kiln.md")
        ),
        "    Basil: kiln shelf cracked.".to_string(),
        format!(
            "notes/runbook.md  (shared note, {})",
            day("notes/runbook.md")
        ),
        "    Check the kiln vent.".to_string(),
        format!(
            "notes/ada/kiln.md  (ada's note, {})",
            day("notes/ada/kiln.md")
        ),
        "    Kiln east runs hot.".to_string(),
    ];
    assert_eq!(out.lines().collect::<Vec<_>>(), want, "{out}");
    assert!(!out.contains("facts/"), "{out}");
    let json = ok(&w.pane(&c, &["notes", "find", "kiln", "--for", "basil", "--json"]));
    assert!(json.contains("\"rank\": \"own\""), "{json}");
}

#[test]
fn intervals_are_read_from_the_main_root_by_a_pane() {
    let Some(w) = world() else { return };
    let a = w.start("s1", "Ada");
    let archive = |w: &World| ok(&w.pane(&a, &["quirk", "archive"]));
    assert!(
        archive(&w).contains("quiet for 90 day(s)"),
        "{}",
        archive(&w)
    );

    let base = std::fs::read_to_string(w.m.join("shards.toml")).unwrap();
    // Old place alone: still read.
    std::fs::write(
        w.m.join("shards.toml"),
        format!("{base}[quirks]\narchive_after_days = 7\n"),
    )
    .unwrap();
    assert!(
        archive(&w).contains("quiet for 7 day(s)"),
        "{}",
        archive(&w)
    );
    // Both: [intervals] wins.
    std::fs::write(
        w.m.join("shards.toml"),
        format!("{base}[intervals]\nquirk_archive_days = 14\n[quirks]\narchive_after_days = 7\n"),
    )
    .unwrap();
    assert!(
        archive(&w).contains("quiet for 14 day(s)"),
        "{}",
        archive(&w)
    );
}

/// Brief 10, from a shard's pane: a person in any case is one folder, and
/// `notes find` sees notes merged to main after the shard opened, with the
/// shard's own version of a note that is on both.
#[test]
fn a_pane_finds_main_notes_merged_after_it_opened_and_people_match_in_any_case() {
    let Some(w) = world() else { return };
    let a = w.start("s1", "Ada");

    // After s1 opened: Basil's shard writes and merges, and a folder named
    // DAVE/ by hand reaches main.
    let b = w.start("s2", "Basil");
    let out = put_from_pane(
        &w,
        &b,
        "kiln.md",
        "Basil: kiln shelf cracked.\n",
        &["--for", "Basil"],
    );
    assert!(out.starts_with("wrote notes/basil/kiln.md"), "{out}");
    put_from_pane(
        &w,
        &b,
        "notes/runbook.md",
        "Kiln key is in the drawer.\n",
        &[],
    );
    put_from_pane(
        &w,
        &b,
        "notes/DAVE/lathe.md",
        "Kiln and lathe share a fuse.\n",
        &[],
    );
    let out = ok(&w.mem(&["dinit", "--id", "s2"]));
    assert!(out.contains("shard s2 closed"), "{out}");

    // s1 writes as Ada in two cases, as dave, and its own runbook.
    let out = put_from_pane(
        &w,
        &a,
        "notes/kiln.md",
        "Kiln east runs hot.\n",
        &["--for", "Ada"],
    );
    assert!(out.starts_with("wrote notes/ada/kiln.md"), "{out}");
    let out = put_from_pane(
        &w,
        &a,
        "notes/ADA/vent.md",
        "Kiln vent left.\n",
        &["--for", "ADA"],
    );
    assert!(out.starts_with("wrote notes/ada/vent.md"), "{out}");
    let out = put_from_pane(&w, &a, "kiln.md", "Dave: kiln timer.\n", &["--for", "dave"]);
    assert!(out.starts_with("wrote notes/DAVE/kiln.md"), "{out}");
    put_from_pane(&w, &a, "notes/runbook.md", "Kiln key: ask Ada.\n", &[]);

    let day = |rev: &str, p: &str| git(&w.m, &["log", "-1", "--format=%as", rev, "--", p]);
    for who in ["ada", "Ada", "ADA"] {
        let out = ok(&w.pane(&a, &["notes", "find", "kiln", "--for", who]));
        let paths: Vec<&str> = out
            .lines()
            .filter(|l| !l.starts_with(' '))
            .map(|l| l.split("  ").next().unwrap())
            .collect();
        // Own, shared, then everyone else's. (Writes here can share a
        // second, so order within a group is not checked.)
        let group = |r: std::ops::Range<usize>| {
            let mut g = paths[r].to_vec();
            g.sort();
            g
        };
        assert_eq!(paths.len(), 6, "{who}: {out}");
        assert_eq!(
            group(0..2),
            ["notes/ada/kiln.md", "notes/ada/vent.md"],
            "{out}"
        );
        assert_eq!(paths[2], "notes/runbook.md", "{out}");
        assert_eq!(
            group(3..6),
            [
                "notes/DAVE/kiln.md",
                "notes/DAVE/lathe.md",
                "notes/basil/kiln.md"
            ],
            "{out}"
        );
        assert!(
            out.contains("notes/ada/vent.md  (ada's own note, "),
            "{out}"
        );
        // The runbook once, the shard's version and date.
        assert_eq!(out.matches("notes/runbook.md").count(), 1, "{out}");
        assert!(out.contains("Kiln key: ask Ada."), "{out}");
        assert!(!out.contains("drawer"), "{out}");
        assert!(
            out.contains(&format!(
                "notes/runbook.md  (shared note, {})",
                day("shard/s1", "notes/runbook.md")
            )),
            "{out}"
        );
        // Basil's, from main only, with main's date.
        assert!(
            out.contains(&format!(
                "notes/basil/kiln.md  (basil's note, {})",
                day("main", "notes/basil/kiln.md")
            )),
            "{out}"
        );
    }
    // Dave in any case owns DAVE/.
    let out = ok(&w.pane(&a, &["notes", "find", "kiln", "--for", "Dave"]));
    assert!(
        out.contains("notes/DAVE/kiln.md  (DAVE's own note, "),
        "{out}"
    );
    assert!(
        out.contains("notes/DAVE/lathe.md  (DAVE's own note, "),
        "{out}"
    );

    // Main still sees only main.
    let out = ok(&w.mem(&["notes", "find", "kiln", "--for", "ada"]));
    assert!(!out.contains("notes/ada/"), "{out}");

    // At dinit dave's note joins DAVE/, no second folder; the runbook,
    // changed on both, is a conflict like any other.
    let out = ok(&w.mem(&["dinit", "--id", "s1"]));
    assert!(out.contains("merged    notes/DAVE/kiln.md"), "{out}");
    assert!(out.contains("CONFLICT  notes/runbook.md"), "{out}");
    let tree = git(&w.m, &["ls-tree", "-d", "--name-only", "main", "notes/"]);
    let dirs: Vec<&str> = tree.lines().collect();
    assert_eq!(dirs, ["notes/DAVE", "notes/ada", "notes/basil"], "{tree}");
}
