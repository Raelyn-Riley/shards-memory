// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Acceptance tests for brief 02: shards, dinit, conflicts, soul approval.

use std::path::Path;
use std::process::Command;

use shards_core::{Address, Context, DinitReport, Error, Memory, Provenance, Take, Verdict};
use tempfile::TempDir;

fn env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ]
}

fn core(why: &str) -> Provenance {
    Provenance::new("core", Some("Ada".to_string()), why)
}

/// In a shard worktree the author defaults to the shard id.
fn shardp(why: &str) -> Provenance {
    Provenance::new("", None, why)
}

fn addr(s: &str) -> Address {
    Address::parse(s).unwrap()
}

fn fresh(config: &str) -> (TempDir, Memory) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("m");
    Memory::init_with_env(&root, &core(""), env()).unwrap();
    std::fs::write(root.join("shards.toml"), config).unwrap();
    let mem = Memory::open_with_env(&root, env()).unwrap();
    mem.set(&addr("devices/ctl/ip"), "10.0.0.1", &core("seed"))
        .unwrap();
    mem.set(&addr("devices/ctl/port"), "8080", &core("seed"))
        .unwrap();
    mem.set(&addr("network/dns"), "10.0.0.53", &core("seed"))
        .unwrap();
    (tmp, mem)
}

fn open_shard(mem: &Memory, id: &str, human: &str) -> Memory {
    let meta = mem.shard_open(id, Some(human)).unwrap();
    let s = Memory::open_with_env(&meta.worktree, env()).unwrap();
    assert_eq!(s.context(), &Context::Shard(id.to_string()));
    s
}

/// The core accepts every open change in the dinit's proposal (since brief
/// 12, a dinit proposes facts and people instead of merging them).
fn accept(mem: &Memory, r: &DinitReport) {
    let id = r.proposal.as_deref().expect("the dinit made a proposal");
    mem.proposal_decide(id, None, &Verdict::Accept(None), &core("reviewed"))
        .unwrap();
}

fn val(mem: &Memory, a: &str) -> String {
    mem.get(&addr(a)).unwrap().value
}

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

#[test]
fn shard_writes_go_to_its_branch_not_main() {
    let (_t, mem) = fresh("");
    let s1 = open_shard(&mem, "s1", "Ada");
    assert!(s1.root().starts_with(mem.root().join(".shards/worktrees")));
    s1.supersede(&addr("devices/ctl/ip"), "10.0.0.2", &shardp("moved"))
        .unwrap();
    assert_eq!(val(&s1, "devices/ctl/ip"), "10.0.0.2");
    assert_eq!(val(&mem, "devices/ctl/ip"), "10.0.0.1");
    let h = s1.history(&addr("devices/ctl/ip")).unwrap();
    assert_eq!(h[0].author, "s1");
    assert_eq!(h[0].human.as_deref(), Some("Ada"));
    // Someone else can't write in s1's worktree.
    let err = s1
        .set(&addr("devices/x/y"), "1", &Provenance::new("s2", None, "w"))
        .unwrap_err();
    assert!(matches!(err, Error::NotYourShard { .. }), "{err}");
    // .shards/ is ignored on main.
    let (_, status) = git(mem.root(), &["status", "--porcelain"]);
    assert!(!status.contains(".shards"), "{status}");
    // Listed with its base.
    let list = mem.shard_list().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].meta.human.as_deref(), Some("Ada"));
    assert_eq!(Some(list[0].meta.base.clone()), mem.head().unwrap());
}

#[test]
fn two_shards_different_facts_both_dinit_cleanly() {
    let (_t, mem) = fresh("");
    let s1 = open_shard(&mem, "s1", "Ada");
    let s2 = open_shard(&mem, "s2", "Basil");
    s1.supersede(&addr("devices/ctl/ip"), "10.0.0.2", &shardp("s1 moved it"))
        .unwrap();
    s1.supersede(
        &addr("devices/ctl/ip"),
        "10.0.0.4",
        &shardp("s1 moved it again"),
    )
    .unwrap();
    s2.supersede(&addr("network/dns"), "10.0.0.54", &shardp("s2 new dns"))
        .unwrap();
    s2.set(&addr("network/ntp"), "10.0.0.123", &shardp("s2 added ntp"))
        .unwrap();
    let wt1 = s1.root().to_path_buf();

    let r1 = mem.dinit("s1", false).unwrap();
    assert!(r1.conflicts.is_empty() && r1.closed, "{r1:?}");
    assert_eq!(r1.proposed, vec!["facts/devices/ctl/ip.md"]);
    let r2 = mem.dinit("s2", false).unwrap();
    assert!(r2.conflicts.is_empty() && r2.closed, "{r2:?}");
    accept(&mem, &r1);
    accept(&mem, &r2);

    assert_eq!(val(&mem, "devices/ctl/ip"), "10.0.0.4");
    assert_eq!(val(&mem, "network/dns"), "10.0.0.54");
    assert_eq!(val(&mem, "network/ntp"), "10.0.0.123");
    assert_eq!(val(&mem, "devices/ctl/port"), "8080");

    // The accepted version keeps the shard's author and why; the shard's
    // intermediate value was never proposed, so it isn't main's history.
    let h = mem.history(&addr("devices/ctl/ip")).unwrap();
    let got: Vec<(&str, &str, &str)> = h
        .iter()
        .map(|e| (e.value.as_str(), e.author.as_str(), e.why.as_str()))
        .collect();
    assert_eq!(
        got,
        vec![
            ("10.0.0.4", "s1", "s1 moved it again"),
            ("10.0.0.1", "core", "seed"),
        ]
    );
    // Main is linear-plus-merges; old main is an ancestor of new main.
    let (ok, _) = git(
        mem.root(),
        &["merge-base", "--is-ancestor", "shard/s1", "main"],
    );
    assert!(ok);
    // Clean dinit: worktree removed, branch kept (default), shard closed.
    assert!(!wt1.exists());
    assert!(git(mem.root(), &["rev-parse", "--verify", "shard/s1"]).0);
    assert!(mem.shard_list().unwrap().is_empty());
}

#[test]
fn keep_branches_false_deletes_the_branch_after_a_clean_dinit() {
    let (_t, mem) = fresh("[shards]\nkeep_branches = false\n");
    let s1 = open_shard(&mem, "s1", "Ada");
    s1.supersede(&addr("devices/ctl/ip"), "10.0.0.2", &shardp("w"))
        .unwrap();
    let r = mem.dinit("s1", false).unwrap();
    assert!(r.closed && r.branch_deleted, "{r:?}");
    assert!(!git(mem.root(), &["rev-parse", "--verify", "shard/s1"]).0);
    // The proposal holds what it needs: accepting works after the branch went.
    accept(&mem, &r);
    assert_eq!(
        mem.history(&addr("devices/ctl/ip")).unwrap()[0].author,
        "s1"
    );
}

#[test]
fn same_fact_on_two_shards_conflicts_and_is_settled_by_a_human() {
    let (_t, mem) = fresh("");
    let s1 = open_shard(&mem, "s1", "Ada");
    let s2 = open_shard(&mem, "s2", "Basil");
    s1.supersede(&addr("devices/ctl/ip"), "10.0.0.2", &shardp("s1 says .2"))
        .unwrap();
    s2.supersede(&addr("devices/ctl/ip"), "10.0.0.3", &shardp("s2 says .3"))
        .unwrap();
    s2.supersede(&addr("devices/ctl/port"), "9090", &shardp("s2 port"))
        .unwrap();

    let r1 = mem.dinit("s1", false).unwrap();
    assert!(r1.closed);
    accept(&mem, &r1);
    let main_before = mem.head().unwrap().unwrap();

    // --all-or-nothing: nothing at all is written.
    let r = mem.dinit("s2", true).unwrap();
    assert!(r.aborted && r.conflicts.len() == 1, "{r:?}");
    assert_eq!(mem.head().unwrap().unwrap(), main_before);
    assert_eq!(val(&mem, "devices/ctl/port"), "8080");

    let r = mem.dinit("s2", false).unwrap();
    assert!(!r.closed);
    assert!(r.merged.is_empty());
    assert_eq!(r.proposed, vec!["facts/devices/ctl/port.md"]);
    assert_eq!(r.conflicts.len(), 1);
    let c = &r.conflicts[0];
    assert_eq!(c.target, "devices/ctl/ip");
    assert_eq!(
        (c.main.value.as_str(), c.main.author.as_str()),
        ("10.0.0.2", "s1")
    );
    assert_eq!(
        (c.shard.value.as_str(), c.shard.author.as_str()),
        ("10.0.0.3", "s2")
    );
    assert_eq!(c.main.human.as_deref(), Some("Ada"));
    assert_eq!(c.shard.human.as_deref(), Some("Basil"));
    assert!(!c.main.updated.is_empty() && !c.shard.updated.is_empty());

    // Main unchanged for that fact; the other fact waits in the proposal.
    assert_eq!(val(&mem, "devices/ctl/ip"), "10.0.0.2");
    assert_eq!(val(&mem, "devices/ctl/port"), "8080");
    // The merge commit changed exactly the proposal + the record.
    let commit = r.commit.clone().unwrap();
    let mut changed = mem.changed_paths(&commit).unwrap();
    changed.sort();
    assert_eq!(
        changed,
        vec!["conflicts/s2/devices/ctl/ip.md", "proposals/p2.json"]
    );
    accept(&mem, &r);
    assert_eq!(val(&mem, "devices/ctl/port"), "9090");
    let record =
        std::fs::read_to_string(mem.root().join("conflicts/s2/devices/ctl/ip.md")).unwrap();
    for needle in ["10.0.0.2", "10.0.0.3", "s1 says .2", "s2 says .3", "Basil"] {
        assert!(record.contains(needle), "record lacks {needle}:\n{record}");
    }

    // Dinit again with nothing new: no new conflict, no new commit.
    let again = mem.dinit("s2", false).unwrap();
    assert!(
        again.commit.is_none() && again.conflicts.is_empty(),
        "{again:?}"
    );

    // A human settles it.
    let commit = mem
        .resolve_conflict(
            "devices/ctl/ip",
            &Take::Shard,
            None,
            &Provenance::new("core", Some("Cleo".into()), "s2's switch is the live one"),
        )
        .unwrap()
        .commit;
    assert_eq!(val(&mem, "devices/ctl/ip"), "10.0.0.3");
    assert!(!mem.root().join("conflicts/s2").exists());
    let now = mem.get(&addr("devices/ctl/ip")).unwrap();
    assert_eq!(now.front.why, "s2's switch is the live one");
    assert_eq!(now.front.human.as_deref(), Some("Cleo"));
    let (_, body) = git(mem.root(), &["log", "-1", "--format=%B", &commit]);
    assert!(
        body.contains("Shards-Take: shard")
            && body.contains("Shards-Why: s2's switch is the live one"),
        "{body}"
    );
    // History keeps all three contenders' values.
    let values: Vec<String> = mem
        .history(&addr("devices/ctl/ip"))
        .unwrap()
        .into_iter()
        .map(|e| e.value)
        .collect();
    assert_eq!(values, vec!["10.0.0.3", "10.0.0.2", "10.0.0.1"]);
    assert!(matches!(
        mem.resolve_conflict("devices/ctl/ip", &Take::Main, None, &core("again")),
        Err(Error::NoConflict(_))
    ));

    // Now the shard is clean and closes.
    assert!(mem.dinit("s2", false).unwrap().closed);
}

#[test]
fn resolve_conflict_take_main_and_take_value() {
    let (_t, mem) = fresh("");
    let a = open_shard(&mem, "a", "Ada");
    let b = open_shard(&mem, "b", "Basil");
    let c = open_shard(&mem, "c", "Cleo");
    a.supersede(&addr("devices/ctl/ip"), "10.0.0.2", &shardp("a"))
        .unwrap();
    b.supersede(&addr("devices/ctl/ip"), "10.0.0.3", &shardp("b"))
        .unwrap();
    c.supersede(&addr("devices/ctl/ip"), "10.0.0.4", &shardp("c"))
        .unwrap();
    // Nothing conflicts at dinit (main hasn't moved); the accepts do.
    let rs: Vec<DinitReport> = ["a", "b", "c"]
        .iter()
        .map(|id| mem.dinit(id, false).unwrap())
        .collect();
    for r in &rs {
        assert!(r.conflicts.is_empty());
        accept(&mem, r);
    }
    // Two shards conflict on the same fact: must say which.
    assert!(matches!(
        mem.resolve_conflict("devices/ctl/ip", &Take::Main, None, &core("x")),
        Err(Error::AmbiguousConflict { .. })
    ));
    mem.resolve_conflict("devices/ctl/ip", &Take::Main, Some("b"), &core("keep a's"))
        .unwrap();
    assert_eq!(val(&mem, "devices/ctl/ip"), "10.0.0.2");
    mem.resolve_conflict(
        "devices/ctl/ip",
        &Take::Value("10.0.0.9".into()),
        Some("c"),
        &core("neither; it's .9 now"),
    )
    .unwrap();
    assert_eq!(val(&mem, "devices/ctl/ip"), "10.0.0.9");
}

#[test]
fn people_profiles_merge_like_facts() {
    let (_t, mem) = fresh("");
    let s1 = open_shard(&mem, "s1", "Ada");
    let s2 = open_shard(&mem, "s2", "Basil");
    s1.put_file("people/ada.md", "likes short answers\n", &shardp("learned"))
        .unwrap();
    s2.put_file("people/ada.md", "likes long answers\n", &shardp("learned"))
        .unwrap();
    s2.put_file("people/basil.md", "works nights\n", &shardp("learned"))
        .unwrap();
    let r1 = mem.dinit("s1", false).unwrap();
    assert!(r1.closed);
    accept(&mem, &r1);
    let r = mem.dinit("s2", false).unwrap();
    assert_eq!(r.proposed, vec!["people/basil.md"]);
    assert_eq!(r.conflicts.len(), 1);
    assert_eq!(r.conflicts[0].record, "conflicts/s2/@/people/ada.md");
    mem.resolve_conflict("people/ada.md", &Take::Shard, None, &core("asked her"))
        .unwrap();
    let text = std::fs::read_to_string(mem.root().join("people/ada.md")).unwrap();
    assert_eq!(text, "likes long answers\n");
}

#[test]
fn soul_change_is_a_proposal_until_an_admin_approves() {
    let (_t, mem) = fresh("[soul]\nadmins = [\"Ada\"]\n");
    let s1 = open_shard(&mem, "s1", "Basil");
    s1.put_file("soul/voice.md", "warmer\n", &shardp("Basil asked"))
        .unwrap();
    s1.supersede(&addr("devices/ctl/port"), "9000", &shardp("port"))
        .unwrap();

    let r = mem.dinit("s1", false).unwrap();
    assert_eq!(r.proposals, vec!["soul/voice.md"]);
    assert_eq!(r.proposed, vec!["facts/devices/ctl/port.md"]);
    assert!(!r.closed);
    assert!(!mem.root().join("soul/voice.md").exists());
    assert_eq!(
        mem.shard_list().unwrap()[0].meta.soul_pending,
        vec!["soul/voice.md"]
    );

    // Not an admin: refused, nothing written.
    let head = mem.head().unwrap();
    let err = mem
        .soul_approve("s1", &Provenance::new("", Some("Basil".into()), ""))
        .unwrap_err();
    assert!(matches!(err, Error::NotAdmin { .. }), "{err}");
    assert_eq!(mem.head().unwrap(), head);
    // No human at all: refused.
    assert!(mem
        .soul_approve("s1", &Provenance::new("x", None, ""))
        .is_err());
    // Writing the soul directly on main as a non-admin: refused.
    assert!(matches!(
        mem.put_file(
            "soul/voice.md",
            "colder\n",
            &Provenance::new("x", Some("Basil".into()), "w")
        ),
        Err(Error::NotAdmin { .. })
    ));

    // Admin approves: merged.
    let commit = mem
        .soul_approve("s1", &Provenance::new("", Some("Ada".into()), ""))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(mem.root().join("soul/voice.md")).unwrap(),
        "warmer\n"
    );
    let (_, body) = git(mem.root(), &["log", "-1", "--format=%B", &commit]);
    assert!(body.contains("Shards-Approved-By: Ada"), "{body}");
    assert!(matches!(
        mem.soul_approve("s1", &Provenance::new("", Some("Ada".into()), "")),
        Err(Error::NothingToApprove(_))
    ));
    // Nothing pending any more: the next dinit is clean and closes it.
    assert!(mem.dinit("s1", false).unwrap().closed);
}

#[test]
fn status_line_is_shown_and_never_touches_git() {
    let (_t, mem) = fresh("");
    let s1 = open_shard(&mem, "s1", "Ada");
    let before = mem.head().unwrap();
    mem.shard_set_status("s1", "working on the controller IP")
        .unwrap();
    let list = mem.shard_list().unwrap();
    assert_eq!(
        list[0].status.as_ref().unwrap().gist,
        "working on the controller IP"
    );
    assert_eq!(mem.head().unwrap(), before);
    assert_eq!(s1.commit_count().unwrap(), mem.commit_count().unwrap());
    assert!(mem.shard_set_status("nope", "x").is_err());
}

#[test]
fn skipped_paths_are_reported_not_merged() {
    let (_t, mem) = fresh("");
    let s1 = open_shard(&mem, "s1", "Ada");
    // A shard can't change config through dinit even if it commits it.
    std::fs::write(
        s1.root().join("shards.toml"),
        "[lock]\nscope = \"section\"\n",
    )
    .unwrap();
    assert!(
        git(
            s1.root(),
            &[
                "-c",
                "user.name=s1",
                "-c",
                "user.email=s1@x",
                "commit",
                "-qam",
                "sneaky"
            ]
        )
        .0
    );
    let r = mem.dinit("s1", false).unwrap();
    assert_eq!(r.skipped, vec!["shards.toml"]);
    assert!(r.merged.is_empty());
}
