// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Brief 14: `mem index` lists addresses and descriptions, never values,
//! from main and from a shard (its pending facts marked). All data is fake.

use shards_core::{Address, ChangeStatus, Error, Memory, Provenance, Verdict};
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

fn fresh() -> (TempDir, Memory) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("m");
    Memory::init_with_env(&root, &core(""), env()).unwrap();
    std::fs::write(root.join("shards.toml"), "[activity]\nsource = \"none\"\n").unwrap();
    let mem = Memory::open_with_env(&root, env()).unwrap();
    mem.set_described(
        &addr("devices/mill3/ip"),
        "10.77.0.13",
        Some("IP address of mill 3's controller"),
        &core("seed"),
    )
    .unwrap();
    mem.set_described(
        &addr("devices/mill3/port"),
        "5021",
        Some("TCP port of mill 3's controller API"),
        &core("seed"),
    )
    .unwrap();
    mem.set(&addr("shop/hours"), "7-15", &core("seed")).unwrap();
    (tmp, mem)
}

#[test]
fn the_index_lists_addresses_and_descriptions_and_never_values() {
    let (_t, mem) = fresh();
    let idx = mem.index(None).unwrap();
    let addrs: Vec<&str> = idx.iter().map(|e| e.address.as_str()).collect();
    assert_eq!(
        addrs,
        ["devices/mill3/ip", "devices/mill3/port", "shop/hours"]
    );
    assert_eq!(
        idx[0].description.as_deref(),
        Some("IP address of mill 3's controller")
    );
    assert_eq!(idx[2].description, None);
    assert!(idx.iter().all(|e| e.pending.is_none()));
    let text = serde_json::to_string(&idx).unwrap();
    for v in ["10.77.0.13", "5021", "7-15"] {
        assert!(!text.contains(v), "a value leaked into the index: {v}");
    }
    // A section.
    let idx = mem.index(Some("devices")).unwrap();
    assert_eq!(idx.len(), 2);
    assert!(mem.index(Some("dev")).unwrap().is_empty());

    // A description can't carry the value.
    let e = mem
        .set_described(
            &addr("shop/phone"),
            "555-0142",
            Some("call 555-0142"),
            &core("x"),
        )
        .unwrap_err();
    assert!(matches!(e, Error::Description(_)), "{e}");
    let e = mem
        .describe(&addr("devices/mill3/port"), "port 5021", &core("x"))
        .unwrap_err();
    assert!(matches!(e, Error::Description(_)), "{e}");

    // Supersede keeps the description; describe changes only it.
    mem.supersede(&addr("devices/mill3/ip"), "10.77.0.14", &core("moved"))
        .unwrap();
    assert_eq!(
        mem.get(&addr("devices/mill3/ip"))
            .unwrap()
            .front
            .description
            .as_deref(),
        Some("IP address of mill 3's controller")
    );
    mem.describe(
        &addr("shop/hours"),
        "When the shop floor is open",
        &core("tidy"),
    )
    .unwrap();
    let f = mem.get(&addr("shop/hours")).unwrap();
    assert_eq!(f.value, "7-15");
    assert_eq!(f.front.why, "seed");
    // ... and isn't a new version in the history.
    assert_eq!(mem.history(&addr("shop/hours")).unwrap().len(), 1);
}

#[test]
fn from_a_shard_the_index_marks_its_pending_facts_and_the_core_keeps_the_description() {
    let (_t, mem) = fresh();
    let meta = mem.shard_open("s1", Some("Nell")).unwrap();
    let s1 = Memory::open_with_env(&meta.worktree, env()).unwrap();
    s1.set_described(
        &addr("devices/lathe2/ip"),
        "10.77.0.22",
        Some("IP address of lathe 2's controller"),
        &shardp("Nell read it off the panel"),
    )
    .unwrap();
    s1.supersede(&addr("shop/hours"), "6-14", &shardp("Nell: winter hours"))
        .unwrap();
    // A shard can't edit descriptions; the core does.
    let e = s1
        .describe(&addr("shop/hours"), "Opening hours", &shardp("x"))
        .unwrap_err();
    assert!(matches!(e, Error::Description(_)), "{e}");
    // Main moves after the shard opened: the shard still sees it.
    mem.set_described(
        &addr("devices/saw1/ip"),
        "10.77.0.31",
        Some("IP address of the saw's controller"),
        &core("seed"),
    )
    .unwrap();

    let idx = s1.index(None).unwrap();
    let get = |a: &str| idx.iter().find(|e| e.address == a).unwrap().clone();
    assert_eq!(get("devices/lathe2/ip").pending, Some("new"));
    assert_eq!(
        get("devices/lathe2/ip").description.as_deref(),
        Some("IP address of lathe 2's controller")
    );
    assert_eq!(get("shop/hours").pending, Some("changed"));
    assert_eq!(get("devices/saw1/ip").pending, None);
    assert_eq!(get("devices/mill3/ip").pending, None);
    let text = serde_json::to_string(&idx).unwrap();
    assert!(!text.contains("10.77.0.22") && !text.contains("6-14"));
    // Main's index doesn't show the shard's facts.
    assert!(mem
        .index(None)
        .unwrap()
        .iter()
        .all(|e| e.address != "devices/lathe2/ip"));

    // Dinit, accept: the description reaches main with the fact.
    mem.dinit("s1", false).unwrap();
    let p = mem.proposal("p1").unwrap();
    let c = p
        .changes
        .iter()
        .find(|c| c.address.as_deref() == Some("devices/lathe2/ip"))
        .unwrap();
    assert_eq!(
        c.description.as_deref(),
        Some("IP address of lathe 2's controller")
    );
    mem.proposal_decide("p1", None, &Verdict::Accept(None), &core("panel photo"))
        .unwrap();
    assert!(mem
        .proposal("p1")
        .unwrap()
        .changes
        .iter()
        .all(|c| c.status == ChangeStatus::Accepted));
    let f = mem.get(&addr("devices/lathe2/ip")).unwrap();
    assert_eq!(
        f.front.description.as_deref(),
        Some("IP address of lathe 2's controller")
    );
    assert_eq!(f.value, "10.77.0.22");
}

#[test]
fn everything_the_index_lists_from_a_shard_can_be_read_there() {
    let (_t, mem) = fresh();
    let meta = mem.shard_open("s1", Some("Nell")).unwrap();
    let s1 = Memory::open_with_env(&meta.worktree, env()).unwrap();
    // A fact reaches main after the shard opened.
    mem.set_described(
        &addr("equipment/saws/band-2/blade"),
        "14 TPI bimetal",
        Some("Blade fitted on band saw 2"),
        &core("fitted it"),
    )
    .unwrap();
    assert!(s1
        .index(None)
        .unwrap()
        .iter()
        .any(|e| e.address == "equipment/saws/band-2/blade"));
    // The shard's branch never had it; `get` is the branch, `get_listed`
    // reads it from main and says so.
    assert!(s1.get(&addr("equipment/saws/band-2/blade")).is_err());
    let (f, from_main) = s1
        .get_listed(&addr("equipment/saws/band-2/blade"))
        .unwrap();
    assert_eq!((f.value.as_str(), from_main), ("14 TPI bimetal", true));
    // What the shard has is read as the shard has it.
    s1.supersede(&addr("shop/hours"), "6-14", &shardp("winter"))
        .unwrap();
    let (f, from_main) = s1.get_listed(&addr("shop/hours")).unwrap();
    assert_eq!((f.value.as_str(), from_main), ("6-14", false));
    // Unknown stays unknown.
    assert!(s1.get_listed(&addr("nothing/here")).is_err());
}
