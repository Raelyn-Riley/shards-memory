// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! Sample data generator v0: a small, entirely FAKE flock memory for tests.
//!
//! Three birds, ~30 facts across four sections, a few supersedes so there is
//! history, and markdown notes that use pointers. Output is deterministic by
//! seed: the same seed gives the same commits, byte for byte (timestamps are
//! synthetic, not wall-clock). All values are invented; IPs are in 10.0.0.0/8.

use std::path::Path;

use crate::address::Address;
use crate::error::Result;
use crate::memory::{Memory, Provenance, SetOptions};

/// splitmix64: tiny, stable, dependency-free PRNG (output never changes
/// between versions, unlike some crates' generators).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
        xs[self.below(xs.len() as u64) as usize]
    }
    fn ip(&mut self) -> String {
        format!(
            "10.{}.{}.{}",
            self.below(255),
            self.below(255),
            1 + self.below(253)
        )
    }
}

const BIRD_NAMES: &[&str] = &[
    "wren", "kestrel", "plover", "finch", "heron", "tern", "linnet", "siskin",
];
const DEVICE_NAMES: &[&str] = &[
    "ctl-alpha",
    "press-02",
    "kiln-east",
    "lathe-04",
    "spindle-9",
    "scope-b",
];
const ROLES: &[&str] = &["planner", "builder", "archivist", "scout", "reviewer"];
const TIERS: &[&str] = &["small", "medium", "large"];
const HUMANS: &[&str] = &["Ada", "Basil", "Cleo", "Dov"];

/// What the generator produced, for tests and for printing.
#[derive(Debug, Clone)]
pub struct SampleSummary {
    pub birds: Vec<String>,
    pub facts: usize,
    pub supersedes: usize,
    pub notes: Vec<String>,
}

/// Build a fake flock memory at `dir` (must not exist or be empty).
pub fn generate(dir: &Path, seed: u64) -> Result<SampleSummary> {
    generate_with_env(dir, seed, Vec::new())
}

pub fn generate_with_env(
    dir: &Path,
    seed: u64,
    env: Vec<(String, String)>,
) -> Result<SampleSummary> {
    let mut rng = Rng(seed);
    let mut tick = 0u32;
    let mut stamp = move || {
        tick += 1;
        // 2026-01-05T09:00:00Z + 7 minutes per change.
        let m = 9 * 60 + 7 * tick;
        format!("2026-01-05T{:02}:{:02}:00Z", (m / 60) % 24, m % 60)
    };

    let shards: Vec<String> = (0..3)
        .map(|_| format!("shard-{:04x}", rng.below(0x10000)))
        .collect();
    let prov = |rng: &mut Rng, why: &str, at: String| {
        let mut p = Provenance::new(
            shards[rng.below(shards.len() as u64) as usize].clone(),
            Some(rng.pick(HUMANS).to_string()),
            why,
        );
        p.at = Some(at);
        p
    };

    let mut init = Provenance::new("gen-sample", None, "");
    init.at = Some("2026-01-05T09:00:00Z".to_string());
    let mem = Memory::init_with_env(dir, &init, env)?;

    // Pick 3 distinct birds and 3 distinct devices.
    let mut birds: Vec<&str> = Vec::new();
    while birds.len() < 3 {
        let b = rng.pick(BIRD_NAMES);
        if !birds.contains(&b) {
            birds.push(b);
        }
    }
    let mut devices: Vec<&str> = Vec::new();
    while devices.len() < 3 {
        let d = rng.pick(DEVICE_NAMES);
        if !devices.contains(&d) {
            devices.push(d);
        }
    }

    let mut facts: Vec<(String, String)> = Vec::new();
    for b in &birds {
        facts.push((format!("birds/{b}/host"), format!("{b}-vm.flock.test")));
        facts.push((format!("birds/{b}/ip"), rng.ip()));
        facts.push((format!("birds/{b}/role"), rng.pick(ROLES).to_string()));
        facts.push((format!("birds/{b}/model-tier"), rng.pick(TIERS).to_string()));
    }
    for d in &devices {
        facts.push((format!("devices/{d}/ip"), rng.ip()));
        facts.push((
            format!("devices/{d}/port"),
            (2000 + rng.below(7000)).to_string(),
        ));
        facts.push((
            format!("devices/{d}/firmware"),
            format!("{}.{}.{}", rng.below(4), rng.below(20), rng.below(10)),
        ));
    }
    let net_third = rng.below(255);
    facts.push(("network/subnet".into(), format!("10.{net_third}.0.0/16")));
    facts.push(("network/gateway".into(), format!("10.{net_third}.0.1")));
    facts.push(("network/dns".into(), format!("10.{net_third}.0.53")));
    facts.push(("network/vlan".into(), (100 + rng.below(800)).to_string()));
    facts.push((
        "constants/heartbeat-seconds".into(),
        (5 + rng.below(25)).to_string(),
    ));
    facts.push((
        "constants/lease-seconds".into(),
        (60 + rng.below(240)).to_string(),
    ));
    facts.push((
        "constants/max-retries".into(),
        (2 + rng.below(6)).to_string(),
    ));
    facts.push((
        "constants/flux-ratio".into(),
        format!("{}.{:03}", 1 + rng.below(3), rng.below(1000)),
    ));
    facts.push((
        "constants/queue-depth".into(),
        (8 << rng.below(4)).to_string(),
    ));

    // Same-named facts under different birds/devices are legitimately
    // different facts, so the generator always passes force_new_home.
    let opts = SetOptions {
        force_new_home: true,
    };
    for (addr, value) in &facts {
        let a = Address::parse(addr)?;
        let p = prov(&mut rng, "seeded by gen-sample", stamp());
        mem.set(&a, value, &p, &opts)?;
    }

    // A few supersedes, so history has something in it.
    let changes = [
        (
            format!("devices/{}/ip", devices[0]),
            rng.ip(),
            "moved to the new switch",
        ),
        (
            format!("devices/{}/firmware", devices[1]),
            "9.9.9".to_string(),
            "flashed after vendor fix",
        ),
        (
            "constants/max-retries".to_string(),
            "9".to_string(),
            "flaky link; retry harder",
        ),
    ];
    for (addr, value, why) in &changes {
        let a = Address::parse(addr)?;
        let p = prov(&mut rng, why, stamp());
        mem.supersede(&a, value, &p)?;
    }

    // Notes that use pointers.
    let notes = vec![
        (
            "notes/runbook.md".to_string(),
            format!(
                "# Runbook\n\nThe controller `{d0}` answers on {{{{fact:devices/{d0}/ip}}}} \
                 port {{{{fact:devices/{d0}/port}}}}.\nRetry at most \
                 {{{{fact:constants/max-retries}}}} times, heartbeat every \
                 {{{{fact:constants/heartbeat-seconds}}}}s.\n",
                d0 = devices[0]
            ),
        ),
        (
            "notes/flock.md".to_string(),
            format!(
                "# The flock\n\n{}",
                birds
                    .iter()
                    .map(|b| format!(
                        "- {b}: {{{{fact:birds/{b}/role}}}} on {{{{fact:birds/{b}/host}}}} \
                         ({{{{fact:birds/{b}/ip}}}})\n"
                    ))
                    .collect::<String>()
            ),
        ),
        (
            "notes/network.md".to_string(),
            "# Network\n\nSubnet {{fact:network/subnet}}, gateway {{fact:network/gateway}}, \
             DNS {{fact:network/dns}}, VLAN {{fact:network/vlan}}.\n"
                .to_string(),
        ),
    ];
    for (path, text) in &notes {
        let p = prov(&mut rng, "seeded by gen-sample", stamp());
        mem.put_note(path, text, &p)?;
    }

    Ok(SampleSummary {
        birds: birds.iter().map(|s| s.to_string()).collect(),
        facts: facts.len(),
        supersedes: changes.len(),
        notes: notes.into_iter().map(|(p, _)| p).collect(),
    })
}
