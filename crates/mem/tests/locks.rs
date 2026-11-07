// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Locking, with real concurrent `mem` processes: serialization, scope,
//! wait vs fail, timeout, and crash recovery (SIGKILL).

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

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

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A memory with a few facts and an open shard `s1` that changed one fact.
fn setup(config: &str) -> (TempDir, PathBuf) {
    let t = TempDir::new().unwrap();
    let m = t.path().join("m");
    ok(&run(t.path(), &["init", m.to_str().unwrap()]));
    for (a, v) in [
        ("devices/ctl/ip", "10.0.0.1"),
        ("devices/ctl/port", "8080"),
        ("network/dns", "10.0.0.53"),
    ] {
        ok(&run(&m, &["set", a, v, "--why", "seed"]));
    }
    let wt = ok(&run(
        &m,
        &["shard", "open", "--id", "s1", "--human", "Basil"],
    ));
    let wt = PathBuf::from(wt.trim());
    let o = cmd(
        &wt,
        &["supersede", "devices/ctl/ip", "10.0.0.2", "--why", "s1"],
    )
    .env_remove("SHARDS_AUTHOR")
    .output()
    .unwrap();
    ok(&o);
    write_config(&m, config);
    (t, m)
}

fn write_config(m: &Path, config: &str) {
    std::fs::write(m.join("shards.toml"), config).unwrap();
}

fn spawn_held(m: &Path, args: &[&str], hold_ms: u64) -> Child {
    cmd(m, args)
        .env("SHARDS_DEBUG_HOLD_MS", hold_ms.to_string())
        .spawn()
        .unwrap()
}

fn get(m: &Path, a: &str) -> String {
    ok(&run(m, &["get", a])).lines().next().unwrap().to_string()
}

fn fsck(m: &Path) {
    let o = Command::new("git")
        .arg("-C")
        .arg(m)
        .args(["fsck", "--strict", "--no-dangling"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(o.status.success(), "fsck: {}", stderr(&o));
}

fn no_lock_files_left(m: &Path) {
    let dir = m.join(".shards/locks");
    let left: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".lock"))
        .collect();
    assert!(left.is_empty(), "locks left behind: {left:?}");
}

fn holder_msg(s: &str) -> bool {
    // "locked by <holder> (<human>) since <time>"
    s.lines()
        .any(|l| l.contains("locked by ") && l.contains(" (") && l.contains(") since 20"))
}

const THREE: [&[&str]; 3] = [
    &["set", "network/ntp", "10.0.0.123", "--why", "writer A"],
    &["supersede", "devices/ctl/port", "9090", "--why", "writer B"],
    &["dinit", "--id", "s1"],
];

#[test]
fn three_writers_wait_mode_are_serialized_and_nothing_is_lost() {
    let (_t, m) = setup("[lock]\nwhen_busy = \"wait\"\ntimeout_secs = 60\n");
    let before = ok(&run(&m, &["history", "devices/ctl/port"]));
    let kids: Vec<Child> = THREE.iter().map(|a| spawn_held(&m, a, 800)).collect();
    let outs: Vec<Output> = kids
        .into_iter()
        .map(|k| k.wait_with_output().unwrap())
        .collect();
    for o in &outs {
        ok(o);
    }
    let blocked = outs.iter().filter(|o| holder_msg(&stderr(o))).count();
    assert!(
        blocked >= 2,
        "expected the 2 later writers to be told who holds the lock; got {blocked}:\n{}",
        outs.iter().map(stderr).collect::<Vec<_>>().join("\n---\n")
    );

    assert_eq!(get(&m, "network/ntp"), "10.0.0.123");
    assert_eq!(get(&m, "devices/ctl/port"), "9090");
    assert_eq!(get(&m, "devices/ctl/ip"), "10.0.0.2");
    let after = ok(&run(&m, &["history", "devices/ctl/port"]));
    assert!(after.contains("writer B") && after.contains(before.lines().nth(2).unwrap()));
    fsck(&m);
    no_lock_files_left(&m);
}

#[test]
fn three_writers_fail_mode_one_wins_two_fail_cleanly() {
    let (_t, m) = setup("[lock]\nwhen_busy = \"fail\"\n");
    let head = |m: &Path| {
        String::from_utf8(
            Command::new("git")
                .arg("-C")
                .arg(m)
                .args(["rev-parse", "main"])
                .output()
                .unwrap()
                .stdout,
        )
        .unwrap()
    };
    let h0 = head(&m);
    let kids: Vec<Child> = THREE.iter().map(|a| spawn_held(&m, a, 1500)).collect();
    let outs: Vec<Output> = kids
        .into_iter()
        .map(|k| k.wait_with_output().unwrap())
        .collect();
    let won: Vec<usize> = (0..3).filter(|i| outs[*i].status.success()).collect();
    assert_eq!(
        won.len(),
        1,
        "{:?}",
        outs.iter().map(stderr).collect::<Vec<_>>()
    );
    for (i, o) in outs.iter().enumerate() {
        if !won.contains(&i) {
            let e = stderr(o);
            assert!(holder_msg(&e), "loser {i} not told the holder: {e}");
        }
    }
    // Only the winner's change landed.
    let ntp = run(&m, &["get", "network/ntp"]).status.success();
    let port = get(&m, "devices/ctl/port") == "9090";
    let ip = get(&m, "devices/ctl/ip") == "10.0.0.2";
    assert_eq!([ntp, port, ip], [won[0] == 0, won[0] == 1, won[0] == 2]);
    assert_ne!(head(&m), h0);
    fsck(&m);
    no_lock_files_left(&m);
}

#[test]
fn lock_scope_comes_from_config() {
    let a: &[&str] = &["supersede", "devices/ctl/port", "1", "--why", "a"];
    let b: &[&str] = &["supersede", "network/dns", "10.0.0.54", "--why", "b"];
    let c: &[&str] = &["supersede", "devices/ctl/ip", "10.0.0.9", "--why", "c"];

    // Section scope: different sections run side by side.
    let (_t, m) = setup("[lock]\nscope = \"section\"\nwhen_busy = \"fail\"\n");
    let (x, y) = (spawn_held(&m, a, 1000), spawn_held(&m, b, 1000));
    let (x, y) = (x.wait_with_output().unwrap(), y.wait_with_output().unwrap());
    ok(&x);
    ok(&y);
    // ... but the same section is still exclusive.
    let a2: &[&str] = &["supersede", "devices/ctl/port", "2", "--why", "a2"];
    let (x, y) = (spawn_held(&m, a2, 1000), spawn_held(&m, c, 1000));
    let (x, y) = (x.wait_with_output().unwrap(), y.wait_with_output().unwrap());
    assert_eq!(x.status.success() as u8 + y.status.success() as u8, 1);

    // Store scope (same memory, only the config changes): different
    // sections now exclude each other.
    write_config(&m, "[lock]\nscope = \"store\"\nwhen_busy = \"fail\"\n");
    let a3: &[&str] = &["supersede", "devices/ctl/port", "3", "--why", "a3"];
    let b3: &[&str] = &["supersede", "network/dns", "10.0.0.55", "--why", "b3"];
    let (x, y) = (spawn_held(&m, a3, 1000), spawn_held(&m, b3, 1000));
    let (x, y) = (x.wait_with_output().unwrap(), y.wait_with_output().unwrap());
    assert_eq!(x.status.success() as u8 + y.status.success() as u8, 1);
    fsck(&m);
}

/// Start `mem debug hold-lock` and wait until it holds the lock.
fn hold_lock(m: &Path, secs: f64) -> Child {
    let mut child = cmd(m, &["debug", "hold-lock", "--secs", &secs.to_string()])
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.as_mut().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.starts_with("held"), "{line}");
    child
}

#[test]
fn wait_vs_fail_and_timeout_come_from_config() {
    let (_t, m) = setup("");
    let w: &[&str] = &["supersede", "devices/ctl/port", "1", "--why", "w"];

    // fail: immediate refusal naming the holder.
    write_config(&m, "[lock]\nwhen_busy = \"fail\"\n");
    let mut holder = hold_lock(&m, 2.5);
    let t0 = Instant::now();
    let o = run(&m, w);
    assert!(!o.status.success());
    assert!(holder_msg(&stderr(&o)), "{}", stderr(&o));
    assert!(t0.elapsed() < Duration::from_millis(1500));

    // wait with a short timeout: gives up after ~1s.
    write_config(&m, "[lock]\nwhen_busy = \"wait\"\ntimeout_secs = 1\n");
    let t0 = Instant::now();
    let o = run(&m, w);
    let waited = t0.elapsed();
    assert!(!o.status.success());
    assert!(stderr(&o).contains("gave up waiting"), "{}", stderr(&o));
    assert!(waited >= Duration::from_millis(900), "{waited:?}");
    holder.wait().unwrap();

    // wait with a long timeout: succeeds once the holder lets go.
    let mut holder = hold_lock(&m, 1.5);
    write_config(&m, "[lock]\nwhen_busy = \"wait\"\ntimeout_secs = 20\n");
    let t0 = Instant::now();
    let o = run(&m, w);
    ok(&o);
    assert!(holder_msg(&stderr(&o)));
    assert!(t0.elapsed() >= Duration::from_millis(500));
    holder.wait().unwrap();
    assert_eq!(get(&m, "devices/ctl/port"), "1");
}

#[test]
fn killed_holder_is_recovered_after_the_lease_and_logged() {
    let (_t, m) = setup("[lock]\nwhen_busy = \"wait\"\ntimeout_secs = 20\nlease_secs = 1\n");

    // A live holder keeps its lock past the lease (heartbeat).
    let mut live = hold_lock(&m, 2.5);
    std::thread::sleep(Duration::from_millis(1500));
    write_config(&m, "[lock]\nwhen_busy = \"fail\"\nlease_secs = 1\n");
    let o = run(&m, &["supersede", "devices/ctl/port", "1", "--why", "x"]);
    assert!(
        !o.status.success(),
        "a live holder's lock must not be recovered"
    );
    live.wait().unwrap();

    // A killed holder's lock is recovered once its lease runs out.
    write_config(
        &m,
        "[lock]\nwhen_busy = \"wait\"\ntimeout_secs = 20\nlease_secs = 1\n",
    );
    let mut crasher = hold_lock(&m, 60.0);
    crasher.kill().unwrap(); // SIGKILL on unix: no cleanup, lock file stays
    crasher.wait().unwrap();
    assert!(m.join(".shards/locks/main.lock").exists());

    let o = run(
        &m,
        &["supersede", "devices/ctl/port", "2", "--why", "after crash"],
    );
    ok(&o);
    let e = stderr(&o);
    assert!(e.contains("recovered lock `main` from debug-holder"), "{e}");
    let log = std::fs::read_to_string(m.join(".shards/lock.log")).unwrap();
    assert!(
        log.contains("recovered lock `main` from debug-holder"),
        "{log}"
    );
    assert!(log.contains("process gone"), "{log}");
    assert_eq!(get(&m, "devices/ctl/port"), "2");
    fsck(&m);
    no_lock_files_left(&m);
}

/// Brief 09: the lock timeout read from `[intervals]`, which wins over an old
/// `[lock] timeout_secs` in the same file.
#[test]
fn the_lock_timeout_in_intervals_wins_over_the_old_place() {
    let (_t, m) = setup("");
    let w: &[&str] = &["supersede", "devices/ctl/port", "1", "--why", "w"];
    write_config(
        &m,
        "[lock]\nwhen_busy = \"wait\"\ntimeout_secs = 60\n[intervals]\nlock_timeout_secs = 1\n",
    );
    let mut holder = hold_lock(&m, 3.0);
    let t0 = Instant::now();
    let o = run(&m, w);
    let waited = t0.elapsed();
    assert!(!o.status.success());
    assert!(stderr(&o).contains("gave up waiting"), "{}", stderr(&o));
    assert!(waited >= Duration::from_millis(900), "{waited:?}");
    assert!(waited < Duration::from_millis(2500), "{waited:?}");
    holder.wait().unwrap();

    // The old place alone still works: a long wait outlasts the holder.
    write_config(&m, "[lock]\nwhen_busy = \"wait\"\ntimeout_secs = 20\n");
    let mut holder = hold_lock(&m, 1.5);
    ok(&run(&m, w));
    holder.wait().unwrap();
    assert_eq!(get(&m, "devices/ctl/port"), "1");
}
