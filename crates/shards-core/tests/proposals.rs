// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Brief 12: dinit as a proposal. Notes merge at dinit; a shard's facts and
//! people changes wait in one proposal, which the core reviews per change.

use std::path::Path;
use std::process::Command;

use chrono::Utc;
use shards_core::watch::EventKind;
use shards_core::{
    Address, ChangeOp, ChangeStatus, Error, Memory, Provenance, Take, Verdict, WakeOutcome,
};
use tempfile::TempDir;

fn env() -> Vec<(String, String)> {
    vec![
        ("GIT_CONFIG_GLOBAL".into(), "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
    ]
}

fn core(why: &str) -> Provenance {
    Provenance::new("core", None, why)
}

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
    std::fs::write(
        root.join("shards.toml"),
        format!("[activity]\nsource = \"none\"\n{config}"),
    )
    .unwrap();
    let mem = Memory::open_with_env(&root, env()).unwrap();
    mem.set(&addr("kiln/east/temp"), "900", &core("seed"))
        .unwrap();
    mem.set(&addr("kiln/east/vent"), "closed", &core("seed"))
        .unwrap();
    mem.set(&addr("kiln/west/temp"), "850", &core("seed"))
        .unwrap();
    (tmp, mem)
}

fn open_shard(mem: &Memory, id: &str, human: &str) -> Memory {
    let meta = mem.shard_open(id, Some(human)).unwrap();
    Memory::open_with_env(&meta.worktree, env()).unwrap()
}

fn val(mem: &Memory, a: &str) -> Option<String> {
    mem.get(&addr(a)).ok().map(|f| f.value)
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn decide(mem: &Memory, id: &str, n: usize, v: Verdict, why: &str) -> ChangeStatus {
    let r = mem.proposal_decide(id, Some(n), &v, &core(why)).unwrap();
    assert_eq!(r.changes.len(), 1);
    r.changes[0].2
}

#[test]
fn notes_land_at_dinit_while_facts_and_people_wait() {
    let (_t, mem) = fresh("");
    let s1 = open_shard(&mem, "s1", "Basil");
    s1.set(
        &addr("kiln/east/shelf"),
        "cracked",
        &shardp("saw the crack"),
    )
    .unwrap();
    s1.supersede(
        &addr("kiln/east/temp"),
        "950",
        &shardp("read off the controller"),
    )
    .unwrap();
    s1.put_file("people/basil.md", "works nights\n", &shardp("he said so"))
        .unwrap();
    s1.put_file("notes/kiln.md", "Vent sticks.\n", &shardp("tip"))
        .unwrap();
    let main0 = mem.head().unwrap().unwrap();

    let r = mem.dinit("s1", false).unwrap();
    assert_eq!(r.merged, vec!["notes/kiln.md"]);
    assert_eq!(r.proposal.as_deref(), Some("p1"));
    assert_eq!(
        r.proposed,
        vec![
            "facts/kiln/east/shelf.md",
            "facts/kiln/east/temp.md",
            "people/basil.md"
        ]
    );
    // A proposal doesn't hold the shard open: the record has what it needs.
    assert!(r.closed, "{r:?}");

    // The note is on main; the facts and the profile are not.
    assert_eq!(
        std::fs::read_to_string(mem.root().join("notes/kiln.md")).unwrap(),
        "Vent sticks.\n"
    );
    assert_eq!(val(&mem, "kiln/east/shelf"), None);
    assert_eq!(val(&mem, "kiln/east/temp").as_deref(), Some("900"));
    assert!(!mem.root().join("people/basil.md").exists());
    // History on main doesn't show a value main never had.
    let h = mem.history(&addr("kiln/east/temp")).unwrap();
    assert_eq!(h.len(), 1);
    // The merge commit is one commit: the note and the proposal record.
    let mut changed = mem.changed_paths(&main0_child(&mem, &main0)).unwrap();
    changed.sort();
    assert_eq!(changed, vec!["notes/kiln.md", "proposals/p1.json"]);

    let p = mem.proposal("p1").unwrap();
    assert_eq!(
        (p.shard.as_str(), p.human.as_deref()),
        ("s1", Some("Basil"))
    );
    let got: Vec<_> = p
        .changes
        .iter()
        .map(|c| {
            (
                c.n,
                c.op,
                c.target(),
                c.value.as_deref(),
                c.main_value.as_deref(),
                c.why.as_str(),
                c.status,
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (
                1,
                ChangeOp::Set,
                "kiln/east/shelf",
                Some("cracked"),
                None,
                "saw the crack",
                ChangeStatus::Pending
            ),
            (
                2,
                ChangeOp::Supersede,
                "kiln/east/temp",
                Some("950"),
                Some("900"),
                "read off the controller",
                ChangeStatus::Pending
            ),
            (
                3,
                ChangeOp::Put,
                "people/basil.md",
                Some("works nights\n"),
                None,
                "he said so",
                ChangeStatus::Pending
            ),
        ]
    );
    assert!(p.changes.iter().all(|c| c.author == "s1"));

    // The board shows it pending, readable without a lock.
    let b = mem.board(Utc::now()).unwrap();
    assert_eq!(b.proposals.len(), 1);
    assert_eq!((b.proposals[0].pending, b.proposals[0].held), (3, 0));
    let json = serde_json::to_value(&b).unwrap();
    assert_eq!(json["proposals"][0]["id"], "p1");
    assert_eq!(
        json["proposals"][0]["changes"][1]["target"],
        "kiln/east/temp"
    );
    // So does the core's context.
    let ctx = serde_json::to_value(mem.core_context(Utc::now()).unwrap()).unwrap();
    assert_eq!(ctx["board"]["proposals"][0]["shard"], "s1");
}

/// The commit main moved to by the dinit.
fn main0_child(mem: &Memory, main0: &str) -> String {
    let head = mem.head().unwrap().unwrap();
    assert_eq!(git(mem.root(), &["rev-parse", &format!("{head}^1")]), main0);
    head
}

#[test]
fn a_fact_or_people_change_without_a_reason_is_refused() {
    let (_t, mem) = fresh("");
    let s1 = open_shard(&mem, "s1", "Basil");
    let head = s1.head().unwrap();
    for why in ["", "   "] {
        let errs = [
            s1.set(&addr("kiln/east/shelf"), "cracked", &shardp(why))
                .unwrap_err(),
            s1.supersede(&addr("kiln/east/temp"), "950", &shardp(why))
                .unwrap_err(),
            s1.put_file("people/basil.md", "x\n", &shardp(why))
                .unwrap_err(),
        ];
        for e in errs {
            assert!(matches!(e, Error::MissingWhy), "{e}");
            let msg = e.to_string();
            assert!(msg.contains("--why") && msg.contains("saying why"), "{msg}");
        }
    }
    assert_eq!(s1.head().unwrap(), head, "nothing committed");
    // A decision needs a reason too.
    s1.set(&addr("kiln/east/shelf"), "cracked", &shardp("seen"))
        .unwrap();
    mem.dinit("s1", false).unwrap();
    assert!(matches!(
        mem.proposal_decide("p1", None, &Verdict::Accept(None), &core(" ")),
        Err(Error::MissingWhy)
    ));
}

#[test]
fn accept_reject_hold_and_fix_each_work_per_change() {
    let (_t, mem) = fresh("");
    let s1 = open_shard(&mem, "s1", "Basil");
    s1.set(
        &addr("kiln/east/shelf"),
        "cracked",
        &shardp("saw the crack"),
    )
    .unwrap();
    s1.supersede(
        &addr("kiln/east/temp"),
        "950",
        &shardp("read off the controller"),
    )
    .unwrap();
    s1.supersede(&addr("kiln/east/vent"), "open", &shardp("left it open"))
        .unwrap();
    s1.supersede(&addr("kiln/west/temp"), "870", &shardp("thermocouple"))
        .unwrap();
    let r = mem.dinit("s1", false).unwrap();
    assert_eq!(r.proposed.len(), 4);
    // 1 shelf, 2 temp, 3 vent, 4 west temp (sorted by path).
    let p = mem.proposal("p1").unwrap();
    let n = |t: &str| p.changes.iter().find(|c| c.target() == t).unwrap().n;
    let (shelf, temp, vent, west) = (
        n("kiln/east/shelf"),
        n("kiln/east/temp"),
        n("kiln/east/vent"),
        n("kiln/west/temp"),
    );

    // Accept one change: only it reaches main.
    assert_eq!(
        decide(
            &mem,
            "p1",
            shelf,
            Verdict::Accept(None),
            "matches the photo"
        ),
        ChangeStatus::Accepted
    );
    assert_eq!(val(&mem, "kiln/east/shelf").as_deref(), Some("cracked"));
    assert_eq!(val(&mem, "kiln/east/temp").as_deref(), Some("900"));
    // Stamped with the shard, its human, its reason, and the core's reason.
    let f = mem.get(&addr("kiln/east/shelf")).unwrap();
    assert_eq!(f.front.author, "s1");
    assert_eq!(f.front.human.as_deref(), Some("Basil"));
    assert_eq!(f.front.why, "saw the crack");
    assert_eq!(f.front.reviewed_by.as_deref(), Some("core"));
    assert_eq!(f.front.review_why.as_deref(), Some("matches the photo"));
    assert_eq!(f.front.proposal, Some(format!("p1#{shelf}")));
    let h = mem.history(&addr("kiln/east/shelf")).unwrap();
    assert_eq!(h[0].why, "saw the crack");
    assert_eq!(h[0].review.as_ref().unwrap().why, "matches the photo");
    let body = git(mem.root(), &["log", "-1", "--format=%B"]);
    for needle in [
        "proposal-accept: p1",
        "Shards-Proposal: p1",
        "Shards-Shard: s1",
        "Shards-Shard-Human: Basil",
        "Shards-Why: matches the photo",
        "Shards-Author: core",
    ] {
        assert!(body.contains(needle), "{needle} not in\n{body}");
    }
    assert!(body.contains(&format!("Shards-Shard-Why: {shelf}: saw the crack")));

    // Reject: main stays.
    assert_eq!(
        decide(&mem, "p1", vent, Verdict::Reject, "the vent stays shut"),
        ChangeStatus::Rejected
    );
    assert_eq!(val(&mem, "kiln/east/vent").as_deref(), Some("closed"));

    // Hold: main stays, and it waits for a human.
    assert_eq!(
        decide(
            &mem,
            "p1",
            temp,
            Verdict::Hold("Which probe did you read?".into()),
            "ask Ada which probe"
        ),
        ChangeStatus::Held
    );
    assert_eq!(val(&mem, "kiln/east/temp").as_deref(), Some("900"));
    let e = mem
        .proposal_decide("p1", Some(temp), &Verdict::Accept(None), &core("x"))
        .unwrap_err();
    assert!(e.to_string().contains("held for a human"), "{e}");
    assert!(mem
        .proposal_decide(
            "p1",
            Some(temp),
            &Verdict::Hold("again?".into()),
            &core("again")
        )
        .is_err());

    // Fix: accept an edited value; the record keeps both.
    assert_eq!(
        decide(
            &mem,
            "p1",
            west,
            Verdict::Accept(Some("865".into())),
            "the probe reads 5 high"
        ),
        ChangeStatus::Accepted
    );
    assert_eq!(val(&mem, "kiln/west/temp").as_deref(), Some("865"));
    let p = mem.proposal("p1").unwrap();
    let c = &p.changes[west - 1];
    assert_eq!(
        (c.value.as_deref(), c.accepted_value.as_deref()),
        (Some("870"), Some("865"))
    );

    // A decision is final.
    let e = mem
        .proposal_decide(
            "p1",
            Some(vent),
            &Verdict::Accept(None),
            &core("changed my mind"),
        )
        .unwrap_err();
    assert!(e.to_string().contains("already rejected"), "{e}");
    // A fix names one change.
    assert!(mem
        .proposal_decide("p1", None, &Verdict::Accept(Some("1".into())), &core("x"))
        .is_err());

    // The board: one held, nothing pending.
    let b = mem.board(Utc::now()).unwrap();
    assert_eq!((b.proposals[0].pending, b.proposals[0].held), (0, 1));

    // A human decides the held change.
    let r = mem
        .proposal_decide(
            "p1",
            None,
            &Verdict::Accept(None),
            &Provenance::new("core", Some("Ada".into()), "Ada checked the probe"),
        )
        .unwrap();
    assert_eq!(r.changes.len(), 1, "only the held change was still open");
    assert_eq!(val(&mem, "kiln/east/temp").as_deref(), Some("950"));
    assert_eq!(
        mem.get(&addr("kiln/east/temp"))
            .unwrap()
            .front
            .reviewed_by
            .as_deref(),
        Some("core / Ada")
    );
    assert!(mem.board(Utc::now()).unwrap().proposals.is_empty());
    assert!(mem
        .proposal_decide("p1", None, &Verdict::Reject, &core("x"))
        .is_err());

    // Every decision is logged with its reason, in order, and every
    // earlier version of the record is still in git.
    let p = mem.proposal("p1").unwrap();
    let log: Vec<(usize, &str, &str)> = p
        .log
        .iter()
        .map(|d| (d.change, d.action.as_str(), d.why.as_str()))
        .collect();
    assert_eq!(
        log,
        vec![
            (shelf, "accept", "matches the photo"),
            (vent, "reject", "the vent stays shut"),
            (temp, "hold", "ask Ada which probe"),
            (west, "fix", "the probe reads 5 high"),
            (temp, "accept", "Ada checked the probe"),
        ]
    );
    assert_eq!(p.log[3].value.as_deref(), Some("865"));
    assert_eq!(p.log[4].human.as_deref(), Some("Ada"));
    let versions = git(
        mem.root(),
        &["log", "--format=%H", "--", "proposals/p1.json"],
    );
    assert_eq!(versions.lines().count(), 6, "the dinit and five decisions");
    // What a later step (telling the person) needs is in the record.
    let rej = &p.changes[vent - 1];
    assert_eq!(
        (p.shard.as_str(), p.human.as_deref()),
        ("s1", Some("Basil"))
    );
    assert_eq!(
        (rej.status, rej.why.as_str()),
        (ChangeStatus::Rejected, "left it open")
    );
    assert_eq!(p.log[1].why, "the vent stays shut");
}

#[test]
fn an_accept_after_main_moved_records_a_conflict() {
    let (_t, mem) = fresh("");
    let s1 = open_shard(&mem, "s1", "Basil");
    let s2 = open_shard(&mem, "s2", "Cleo");
    s1.supersede(&addr("kiln/east/temp"), "950", &shardp("Basil's probe"))
        .unwrap();
    s2.supersede(&addr("kiln/east/temp"), "940", &shardp("Cleo's probe"))
        .unwrap();
    s2.set(&addr("kiln/east/shelf"), "cracked", &shardp("seen"))
        .unwrap();
    // Neither dinit conflicts: main hasn't moved.
    assert!(mem.dinit("s1", false).unwrap().conflicts.is_empty());
    assert!(mem.dinit("s2", false).unwrap().conflicts.is_empty());

    mem.proposal_decide(
        "p1",
        None,
        &Verdict::Accept(None),
        &core("Basil's probe is new"),
    )
    .unwrap();
    assert_eq!(val(&mem, "kiln/east/temp").as_deref(), Some("950"));

    // p2's temp: main changed it since p2 was made.
    let r = mem
        .proposal_decide("p2", None, &Verdict::Accept(None), &core("looks fine"))
        .unwrap();
    let st: Vec<_> = r
        .changes
        .iter()
        .map(|c| (c.1.as_str(), c.2, c.3.as_deref()))
        .collect();
    assert_eq!(
        st,
        vec![
            ("kiln/east/shelf", ChangeStatus::Accepted, None),
            (
                "kiln/east/temp",
                ChangeStatus::Conflict,
                Some("conflicts/s2/kiln/east/temp.md")
            ),
        ]
    );
    assert_eq!(val(&mem, "kiln/east/temp").as_deref(), Some("950"));
    assert_eq!(val(&mem, "kiln/east/shelf").as_deref(), Some("cracked"));
    let record =
        std::fs::read_to_string(mem.root().join("conflicts/s2/kiln/east/temp.md")).unwrap();
    for needle in ["950", "940", "Basil's probe", "Cleo's probe", "Cleo"] {
        assert!(record.contains(needle), "{needle} not in\n{record}");
    }
    // It is an open conflict like any other, and settles the usual way.
    let b = mem.board(Utc::now()).unwrap();
    assert_eq!(b.conflicts.len(), 1);
    mem.resolve_conflict(
        "kiln/east/temp",
        &Take::Shard,
        None,
        &Provenance::new("core", Some("Ada".into()), "Cleo's probe is calibrated"),
    )
    .unwrap();
    assert_eq!(val(&mem, "kiln/east/temp").as_deref(), Some("940"));
    let p = mem.proposal("p2").unwrap();
    assert_eq!(p.log.iter().filter(|d| d.action == "conflict").count(), 1);
}

#[test]
fn a_shard_cannot_decide_proposals() {
    let (_t, mem) = fresh("");
    let s1 = open_shard(&mem, "s1", "Basil");
    let s2 = open_shard(&mem, "s2", "Cleo");
    s1.set(&addr("kiln/east/shelf"), "cracked", &shardp("seen"))
        .unwrap();
    mem.dinit("s1", false).unwrap();
    // From another shard's worktree.
    let e = s2
        .proposal_decide("p1", None, &Verdict::Accept(None), &shardp("fine"))
        .unwrap_err();
    assert!(e.to_string().contains("can't decide proposals"), "{e}");
    // As the proposing shard, on main.
    let e = mem
        .proposal_decide(
            "p1",
            None,
            &Verdict::Accept(None),
            &Provenance::new("s1", None, "mine"),
        )
        .unwrap_err();
    assert!(e.to_string().contains("its own proposal"), "{e}");
    assert_eq!(val(&mem, "kiln/east/shelf"), None);
    assert!(matches!(
        mem.proposal_decide("p9", None, &Verdict::Reject, &core("x")),
        Err(Error::UnknownProposal(_))
    ));
}

#[test]
fn a_new_proposal_fires_an_event_that_wakes_the_core() {
    let tmp = TempDir::new().unwrap();
    let got = tmp.path().join("woken.json");
    let (_t, mem) = fresh(&format!(
        "[wake]\nmode = \"command\"\ncommand = \"cat > '{}'\"\n",
        got.display()
    ));
    let s1 = open_shard(&mem, "s1", "Basil");
    s1.set(&addr("kiln/east/shelf"), "cracked", &shardp("seen"))
        .unwrap();
    s1.put_file("notes/kiln.md", "x\n", &shardp("tip")).unwrap();
    // Nothing to review before the dinit.
    let r = mem.watch_pass(Utc::now()).unwrap();
    assert!(r.fired.iter().all(|e| e.kind != EventKind::Proposal));
    mem.dinit("s1", false).unwrap();

    let r = mem.watch_pass(Utc::now()).unwrap();
    assert_eq!(r.fired.len(), 1, "{:?}", r.fired);
    let e = &r.fired[0];
    assert_eq!(e.kind, EventKind::Proposal);
    assert_eq!(e.key, "proposal:p1");
    assert_eq!(e.shards, vec!["s1"]);
    assert_eq!(e.humans, vec!["Basil"]);
    assert_eq!(e.addresses, vec!["kiln/east/shelf"]);
    assert!(e.summary.contains("mem proposal show p1"), "{}", e.summary);
    assert!(matches!(r.wake, WakeOutcome::Delivered { events: 1, .. }));
    let woken = std::fs::read_to_string(&got).unwrap();
    assert!(woken.contains("\"kind\": \"proposal\""), "{woken}");

    // Fires once while it holds.
    assert!(mem.watch_pass(Utc::now()).unwrap().fired.is_empty());
    // Held for a human: nothing for the core, so the condition clears.
    mem.proposal_decide(
        "p1",
        None,
        &Verdict::Hold("Is that right, Basil?".into()),
        &core("ask Basil"),
    )
    .unwrap();
    assert!(mem.watch_pass(Utc::now()).unwrap().fired.is_empty());
    assert!(!mem
        .watch_state()
        .unwrap()
        .active
        .contains_key("proposal:p1"));
}

#[test]
fn the_proposal_trigger_can_be_switched_off() {
    let (_t, mem) = fresh("[triggers]\nproposal = false\n");
    let s1 = open_shard(&mem, "s1", "Basil");
    s1.set(&addr("kiln/east/shelf"), "cracked", &shardp("seen"))
        .unwrap();
    mem.dinit("s1", false).unwrap();
    assert!(mem.watch_pass(Utc::now()).unwrap().fired.is_empty());
    // Still on the board.
    assert_eq!(mem.board(Utc::now()).unwrap().proposals.len(), 1);
}
