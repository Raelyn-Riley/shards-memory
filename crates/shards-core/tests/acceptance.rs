// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Acceptance tests for brief 01: one home, pointers, supersede, history.

use std::path::Path;
use std::process::Command;

use shards_core::{resolve, sample, Address, Error, Memory, Provenance, ResolveOptions};
use tempfile::TempDir;

/// Keep the user's global/system git config (signing, hooks, ...) out of tests.
fn env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ]
}

fn who(author: &str, why: &str) -> Provenance {
    Provenance::new(author, Some("Tess".to_string()), why)
}

fn fresh() -> (TempDir, Memory) {
    let tmp = TempDir::new().unwrap();
    let mem = Memory::init_with_env(tmp.path().join("m"), &who("shard-init", ""), env()).unwrap();
    (tmp, mem)
}

fn addr(s: &str) -> Address {
    Address::parse(s).unwrap()
}

fn set(mem: &Memory, a: &str, v: &str) -> String {
    mem.set(&addr(a), v, &who("shard-a", "seed")).unwrap()
}

/// Plain git, for assertions only (never used to write).
fn git(dir: &Path, args: &[&str]) -> (bool, String) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_string(),
    )
}

// ---------------------------------------------------------------- one home

#[test]
fn set_refuses_a_duplicate_address() {
    let (_t, mem) = fresh();
    set(&mem, "devices/rotoforge-ctl/ip", "10.0.4.40");
    let before = mem.commit_count().unwrap();
    let err = mem
        .set(
            &addr("devices/rotoforge-ctl/ip"),
            "10.0.4.41",
            &who("shard-b", "oops"),
        )
        .unwrap_err();
    assert!(matches!(err, Error::AlreadyExists { .. }), "{err}");
    assert!(err.to_string().contains("supersede"));
    assert_eq!(mem.commit_count().unwrap(), before);
    assert_eq!(
        mem.get(&addr("devices/rotoforge-ctl/ip")).unwrap().value,
        "10.0.4.40"
    );
}

#[test]
fn same_name_elsewhere_is_allowed_only_exact_address_is_refused() {
    let (_t, mem) = fresh();
    set(&mem, "devices/rotoforge-ctl/ip", "10.0.4.40");
    // Same last segment, same section: a different fact. Near-duplicate
    // detection belongs to the drift checker, not to `set`.
    set(&mem, "devices/rotoforge/ip", "10.0.4.77");
    set(&mem, "birds/wren/ip", "10.0.9.9");
    assert_eq!(
        mem.get(&addr("devices/rotoforge/ip")).unwrap().value,
        "10.0.4.77"
    );
    assert!(matches!(
        mem.set(&addr("devices/rotoforge/ip"), "x", &who("s", "w")),
        Err(Error::AlreadyExists { .. })
    ));
}

#[test]
fn set_refuses_a_fact_that_would_be_a_folder() {
    let (_t, mem) = fresh();
    set(&mem, "devices/ctl", "x");
    assert!(matches!(
        mem.set(&addr("devices/ctl/ip"), "y", &who("s", "w")),
        Err(Error::PathConflict { .. })
    ));
}

// ---------------------------------------------------------------- pointers

#[test]
fn resolve_returns_current_value_and_names_home_and_follows_supersede() {
    let (_t, mem) = fresh();
    set(&mem, "devices/rotoforge-ctl/ip", "10.0.4.40");
    set(&mem, "devices/rotoforge-ctl/port", "8080");
    let note = "ctl at {{fact:devices/rotoforge-ctl/ip}}:{{fact:devices/rotoforge-ctl/port}}, \
                again {{fact:devices/rotoforge-ctl/ip}}";

    let plain = resolve(&mem, note, ResolveOptions::default()).unwrap();
    assert_eq!(plain, "ctl at 10.0.4.40:8080, again 10.0.4.40");

    let annotated = resolve(&mem, note, ResolveOptions { annotate: true }).unwrap();
    assert!(
        annotated.contains("10.0.4.40 [devices/rotoforge-ctl/ip]"),
        "{annotated}"
    );
    assert!(
        annotated.contains("8080 [devices/rotoforge-ctl/port]"),
        "{annotated}"
    );

    mem.supersede(
        &addr("devices/rotoforge-ctl/ip"),
        "10.0.4.41",
        &who("shard-b", "moved"),
    )
    .unwrap();
    let after = resolve(&mem, note, ResolveOptions::default()).unwrap();
    assert_eq!(after, "ctl at 10.0.4.41:8080, again 10.0.4.41");
}

#[test]
fn unresolvable_pointer_is_an_error_naming_it() {
    let (_t, mem) = fresh();
    set(&mem, "devices/rotoforge-ctl/ip", "10.0.4.40");
    let err = resolve(
        &mem,
        "{{fact:devices/rotoforge-ctl/ip}} {{fact:devices/ghost/ip}} {{fact:Bad Address}}",
        ResolveOptions::default(),
    )
    .unwrap_err();
    let msg = err.to_string();
    assert!(msg.contains("{{fact:devices/ghost/ip}}"), "{msg}");
    assert!(msg.contains("{{fact:Bad Address}}"), "{msg}");
    assert!(!msg.contains("rotoforge-ctl"), "{msg}");
}

// ---------------------------------------------------------------- supersede

#[test]
fn supersede_keeps_old_value_in_history_and_touches_nothing_else() {
    let (_t, mem) = fresh();
    set(&mem, "devices/rotoforge-ctl/ip", "10.0.4.40");
    set(&mem, "devices/rotoforge-ctl/port", "8080");

    // Leave unrelated changes lying around: one staged, one unstaged.
    std::fs::write(mem.root().join("README.md"), "scribbled\n").unwrap();
    std::fs::write(mem.root().join("stray.txt"), "stray\n").unwrap();
    git(mem.root(), &["add", "stray.txt"]);

    let mut p = Provenance::new("shard-b", Some("Tess".into()), "moved to the new switch");
    p.at = Some("2026-11-03T15:04:05Z".into());
    let a = addr("devices/rotoforge-ctl/ip");
    let commit = mem.supersede(&a, "10.0.4.41", &p).unwrap();

    // Exactly one file changed in that commit.
    assert_eq!(
        mem.changed_paths(&commit).unwrap(),
        vec!["facts/devices/rotoforge-ctl/ip.md".to_string()]
    );
    let (_, names) = git(mem.root(), &["show", "--name-only", "--format=", &commit]);
    assert_eq!(names, "facts/devices/rotoforge-ctl/ip.md");

    // Current value + automatic timestamp/author.
    let now = mem.get(&a).unwrap();
    assert_eq!(now.value, "10.0.4.41");
    assert_eq!(now.front.updated, "2026-11-03T15:04:05Z");
    assert_eq!(now.front.author, "shard-b");

    // Old value still readable through history, newest first.
    let h = mem.history(&a).unwrap();
    assert_eq!(h.len(), 2);
    assert_eq!(h[0].value, "10.0.4.41");
    assert_eq!(h[0].author, "shard-b");
    assert_eq!(h[0].human.as_deref(), Some("Tess"));
    assert_eq!(h[0].why, "moved to the new switch");
    assert_eq!(h[0].updated, "2026-11-03T15:04:05Z");
    assert_eq!(h[1].value, "10.0.4.40");
    assert_eq!(h[1].author, "shard-a");
    assert_eq!(h[1].why, "seed");
    assert!(
        chrono::DateTime::parse_from_rfc3339(&h[1].updated).is_ok(),
        "{}",
        h[1].updated
    );

    // Commit message carries author, human, why; git dates match `updated`.
    let (_, body) = git(mem.root(), &["log", "-1", "--format=%B", &commit]);
    for line in [
        "supersede: devices/rotoforge-ctl/ip",
        "Shards-Op: supersede",
        "Shards-Address: devices/rotoforge-ctl/ip",
        "Shards-Author: shard-b",
        "Shards-Human: Tess",
        "Shards-Why: moved to the new switch",
    ] {
        assert!(body.contains(line), "missing {line:?} in\n{body}");
    }
    let (_, date) = git(mem.root(), &["log", "-1", "--format=%aI %an", &commit]);
    assert_eq!(date, "2026-11-03T15:04:05Z shard-b");

    // The unrelated changes are untouched and still uncommitted.
    let (_, status) = git(mem.root(), &["status", "--porcelain"]);
    assert!(status.contains("M README.md"), "{status}");
    assert!(status.contains("A  stray.txt"), "{status}");
}

#[test]
fn supersede_requires_an_existing_fact_a_why_and_an_author() {
    let (_t, mem) = fresh();
    let a = addr("devices/rotoforge-ctl/ip");
    assert!(matches!(
        mem.supersede(&a, "1", &who("s", "w")),
        Err(Error::UnknownAddress(_))
    ));
    set(&mem, "devices/rotoforge-ctl/ip", "10.0.4.40");
    assert!(matches!(
        mem.supersede(&a, "1", &who("s", "  ")),
        Err(Error::MissingWhy)
    ));
    assert!(matches!(
        mem.supersede(&a, "1", &who("", "w")),
        Err(Error::MissingAuthor)
    ));
    assert!(matches!(
        mem.supersede(&a, "10.0.4.40", &who("s", "w")),
        Err(Error::Unchanged(_))
    ));
}

// ---------------------------------------------------------------- history

#[test]
fn history_only_grows_through_the_tool() {
    let (_t, mem) = fresh();
    let mut count = mem.commit_count().unwrap();
    let mut head = mem.head().unwrap().unwrap();
    let a = addr("devices/rotoforge-ctl/ip");

    let mut check = |mem: &Memory, expect_new: bool| {
        let n = mem.commit_count().unwrap();
        let h = mem.head().unwrap().unwrap();
        assert!(n >= count, "commit count went down: {count} -> {n}");
        assert_eq!(n > count, expect_new);
        // The old head is still an ancestor: nothing was rewritten.
        let (ok, _) = git(mem.root(), &["merge-base", "--is-ancestor", &head, &h]);
        assert!(ok, "{head} is no longer an ancestor of {h}");
        count = n;
        head = h;
    };

    set(&mem, "devices/rotoforge-ctl/ip", "10.0.4.40");
    check(&mem, true);
    let _ = mem.set(&a, "x", &who("s", "w")); // refused
    check(&mem, false);
    for v in ["10.0.4.41", "10.0.4.42", "10.0.4.43"] {
        mem.supersede(&a, v, &who("s", "w")).unwrap();
        check(&mem, true);
    }
    let _ = mem.supersede(&a, "10.0.4.43", &who("s", "w")); // unchanged, refused
    check(&mem, false);
    let _ = mem.supersede(&addr("devices/ghost/ip"), "1", &who("s", "w")); // refused
    check(&mem, false);

    assert_eq!(mem.history(&a).unwrap().len(), 4);
    assert_eq!(count, 5); // init + set + 3 supersedes
}

#[test]
fn writes_refused_off_main() {
    let (_t, mem) = fresh();
    git(mem.root(), &["checkout", "-q", "-b", "side"]);
    let err = mem
        .set(&addr("devices/x/ip"), "1", &who("s", "w"))
        .unwrap_err();
    assert!(
        matches!(err, Error::NotOnMain(ref b) if b == "side"),
        "{err}"
    );
}

// ---------------------------------------------------------------- sample

#[test]
fn sample_generator_is_deterministic_and_fake() {
    let tmp = TempDir::new().unwrap();
    let s1 = sample::generate_with_env(&tmp.path().join("a"), 42, env()).unwrap();
    sample::generate_with_env(&tmp.path().join("b"), 42, env()).unwrap();
    sample::generate_with_env(&tmp.path().join("c"), 43, env()).unwrap();
    let a = Memory::open_with_env(tmp.path().join("a"), env()).unwrap();
    let b = Memory::open_with_env(tmp.path().join("b"), env()).unwrap();
    let c = Memory::open_with_env(tmp.path().join("c"), env()).unwrap();

    assert_eq!(a.head().unwrap(), b.head().unwrap(), "same seed, same repo");
    assert_ne!(
        a.head().unwrap(),
        c.head().unwrap(),
        "different seed, different repo"
    );

    assert_eq!(s1.birds.len(), 3);
    let addrs = a.addresses().unwrap();
    assert!((25..=35).contains(&addrs.len()), "{} facts", addrs.len());
    let sections: std::collections::BTreeSet<_> =
        addrs.iter().map(|x| x.section().to_string()).collect();
    assert!((3..=4).contains(&sections.len()), "{sections:?}");

    for x in &addrs {
        if x.name() == "ip" || x.name() == "gateway" || x.name() == "dns" {
            assert!(
                a.get(x).unwrap().value.starts_with("10."),
                "{x} not in 10/8"
            );
        }
    }
    // Some facts have history.
    assert!(addrs.iter().any(|x| a.history(x).unwrap().len() > 1));

    // Every note resolves.
    for n in &s1.notes {
        let text = std::fs::read_to_string(a.root().join(n)).unwrap();
        assert!(text.contains("{{fact:"), "{n} has no pointers");
        let out = resolve(&a, &text, ResolveOptions::default()).unwrap();
        assert!(!out.contains("{{fact:"), "{n} still has pointers");
    }
}
