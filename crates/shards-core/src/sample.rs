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
use crate::memory::{Memory, Provenance};

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

    for (addr, value) in &facts {
        let a = Address::parse(addr)?;
        let p = prov(&mut rng, "seeded by gen-sample", stamp());
        mem.set(&a, value, &p)?;
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

/// Markers planted in [`fake_transcript`]: prompt text, answer text, tool
/// results, file contents and secrets. None of them may ever appear in any
/// output or state file.
pub const PLANTED: &[&str] = &[
    "PLANTED",
    "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiJ3cmVuIn0.c2lnbmF0dXJlLWZha2U",
    "c0ffee00c0ffee00c0ffee00c0ffee00c0ffee00",
];

/// Write a hand-made, entirely FAKE Claude Code transcript for a shard
/// worktree, where Claude Code would put it under `projects`:
/// `<projects>/<slug of worktree>/<session>.jsonl`. One record per minute
/// from `start`. It contains a prompt, thinking, an answer, tool results and
/// secrets (see [`PLANTED`]) next to ordinary tool calls (an `Edit` of
/// `facts/devices/kiln-east/ip.md`, a few `Bash` commands, a `Grep`).
pub fn fake_transcript(
    projects: &Path,
    worktree: &Path,
    session: &str,
    start: chrono::DateTime<chrono::Utc>,
) -> Result<std::path::PathBuf> {
    use serde_json::json;
    let dir = projects.join(crate::activity::claude_slug(worktree));
    std::fs::create_dir_all(&dir)?;
    let wt = worktree.to_string_lossy();
    let tool = |name: &str, input: serde_json::Value| {
        json!({"type": "assistant", "cwd": wt, "sessionId": session,
               "message": {"role": "assistant", "content": [
                   {"type": "tool_use", "id": "toolu_fake", "name": name, "input": input}]}})
    };
    let records = vec![
        json!({"type": "user", "cwd": wt, "sessionId": session, "message": {"role": "user",
               "content": "PLANTED-PROMPT move the kiln controller; the wifi password is PLANTED-wifi"}}),
        json!({"type": "assistant", "cwd": wt, "sessionId": session, "message": {"role": "assistant",
               "content": [{"type": "thinking", "thinking": "PLANTED-THINKING"},
                           {"type": "text", "text": "PLANTED-ANSWER on it"}]}}),
        tool(
            "Bash",
            json!({"command": "curl -H \"Authorization: Bearer PLANTEDtok3n\" https://kiln.example.invalid/status",
                            "description": "PLANTED-DESCRIPTION"}),
        ),
        json!({"type": "user", "cwd": wt, "sessionId": session, "message": {"role": "user",
               "content": [{"type": "tool_result", "tool_use_id": "toolu_fake",
                            "content": "PLANTED-RESULT ip 10.9.9.9"}]}}),
        tool(
            "Edit",
            json!({"file_path": format!("{wt}/facts/devices/kiln-east/ip.md"),
                            "old_string": "PLANTED-OLD", "new_string": "PLANTED-NEW"}),
        ),
        tool(
            "Write",
            json!({"file_path": format!("{wt}/notes/kiln-plan.md"),
                             "content": "PLANTED-FILE-CONTENT"}),
        ),
        tool(
            "Bash",
            json!({"command": "export API_KEY=PLANTED-apikey-55; make check"}),
        ),
        tool(
            "Bash",
            json!({"command": "git clone https://wren:PLANTED-pw@git.example.invalid/kiln.git"}),
        ),
        tool(
            "Bash",
            json!({"command": format!("echo {} | jwt-decode", PLANTED[1])}),
        ),
        tool(
            "Bash",
            json!({"command": format!("sign --key {} build.tar", PLANTED[2])}),
        ),
        tool(
            "Bash",
            json!({"command": "mysql --password PLANTED-db-pw kiln"}),
        ),
        tool(
            "Grep",
            json!({"pattern": "PLANTED-PATTERN", "path": format!("{wt}/facts")}),
        ),
    ];
    let mut text = String::new();
    for (i, mut r) in records.into_iter().enumerate() {
        let at = start + chrono::Duration::minutes(i as i64);
        r["timestamp"] = json!(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
        text.push_str(&r.to_string());
        text.push('\n');
    }
    let path = dir.join(format!("{session}.jsonl"));
    std::fs::write(&path, text)?;
    Ok(path)
}

/// A FAKE agent session (the core, or a shard) for tests. It never calls a
/// model: it reads lines from its terminal and writes a Claude Code-shaped
/// transcript, one prompt and one reply (with made-up token usage and a
/// planted reply text) per line. Like Claude Code, it writes no transcript
/// until the first prompt, and `--resume` of an id with no transcript fails.
/// Usage:
///
/// `sh fake-core.sh <projects> <ctl> [--resume <id> | --session-id <id>] [-- <first message>]`
///
/// Like Claude Code (checked on 2.1.319), it puts its terminal in raw mode
/// (no echo, no line editing, Enter is a bare carriage return) and the
/// alternate screen, echoes what is typed itself, and restores the terminal
/// only when it exits cleanly (`/exit`, or SIGTERM). Killed with SIGKILL, it
/// leaves the pane's shell unable to take a typed line.
///
/// A prompt `work <n>` starts a busy turn of `n` seconds with one tool call
/// a second. A line typed during it is queued the way Claude Code queues
/// it: a `queue-operation` enqueue record at once, then, at the next tool
/// call, a `queued_command` attachment *inside the same turn* (not a new
/// prompt). A line still queued when the turn ends becomes the next prompt.
///
/// Each start appends `args: ...` to `<ctl>/starts.log`. Files in `<ctl>`
/// change its behaviour: `fail-all` (exit at once), `fail-fresh` (exit at
/// once unless resuming), `ignore-exit` (`/exit` does nothing),
/// `slow-exit` (`/exit` takes 2 s), `cold` (replies report a cache write
/// instead of a cache read), `trust-prompt` (show Claude Code's "trust this
/// folder" question and wait).
pub const FAKE_CORE: &str = r#"#!/bin/sh
projects=$1; ctl=$2; shift 2
echo "args: $*" >> "$ctl/starts.log"
id=""; resume=""
if [ "$1" = "--resume" ]; then id=$2; resume=1; shift 2; fi
if [ "$1" = "--session-id" ]; then id=$2; shift 2; fi
[ "$1" = "--" ] && shift
[ -e "$ctl/fail-all" ] && exit 3
[ -z "$resume" ] && [ -e "$ctl/fail-fresh" ] && exit 4
[ -z "$id" ] && id=$(cat /proc/sys/kernel/random/uuid)
slug=$(printf %s "$PWD" | sed 's/[^A-Za-z0-9]/-/g')
tx="$projects/$slug/$id.jsonl"
# Like Claude Code: nothing to resume without a transcript.
[ -n "$resume" ] && [ ! -f "$tx" ] && { echo "No conversation found with session ID: $id"; exit 1; }
if [ -e "$ctl/trust-prompt" ]; then
  printf ' Accessing workspace:\n %s\n Is this a project you trust?\n \342\235\257 No, exit\n   Yes, I trust this folder\n' "$PWD"
  read -r answer; exit 0
fi
mkdir -p "$projects/$slug"
# Raw terminal and the alternate screen, undone only on a clean exit.
# Reads block (min 1) as Claude Code's do, except in a busy turn, which needs
# a clock: then a read gives up after 0.1 s (min 0 time 1).
stty -echo -icanon -icrnl min 1 time 0 2>/dev/null
printf '\033[?1049h'
trap 'printf "\033[?1049l"; stty sane 2>/dev/null' EXIT
trap 'exit 143' TERM
n=0; busy_until=0; next_tool=0; tools=0; queue=""
CR=$(printf '\r'); NL='
'
ts() { date -u +%Y-%m-%dT%H:%M:%S.%3NZ; }
esc() { printf %s "$1" | tr '\n\t' '  ' | sed 's/\\/\\\\/g; s/"/\\"/g'; }
usage() {
  read=5000; write=200
  [ -e "$ctl/cold" ] && { read=0; write=5200; }
  printf '"usage":{"input_tokens":3,"output_tokens":%s,"cache_read_input_tokens":%s,"cache_creation_input_tokens":%s}' "$1" "$read" "$write"
}
reply() {
  out=40
  case "$1" in \[[0-9][0-9]:[0-9][0-9]\]) out=1;; esac
  printf '{"type":"assistant","timestamp":"%s","sessionId":"%s","message":{"id":"msg-%s-%s","role":"assistant","stop_reason":"end_turn",%s,"content":[{"type":"text","text":"PLANTED-CORE-REPLY"}]}}\n' "$(ts)" "$id" "$$" "$n" "$(usage $out)" >> "$tx"
  printf 'fake-core: replied (%s)\r\n' "$n"
}
turn() {
  n=$((n+1))
  printf '{"type":"user","timestamp":"%s","sessionId":"%s","message":{"role":"user","content":"%s"}}\n' "$(ts)" "$id" "$(esc "$1")" >> "$tx"
  case "$1" in
    "work "*)
      now=$(date +%s); busy_until=$((now + ${1#work })); next_tool=$((now + 1))
      stty min 0 time 1 2>/dev/null
      printf 'fake-core: working (%s)\r\n' "$n";;
    *) reply "$1";;
  esac
}
# A tool call in a busy turn; queued lines join the turn here.
tool() {
  tools=$((tools+1))
  printf '{"type":"assistant","timestamp":"%s","sessionId":"%s","message":{"id":"msg-%s-%s-t%s","role":"assistant","stop_reason":"tool_use",%s,"content":[{"type":"tool_use","name":"Bash","input":{"command":"sleep 1"}}]}}\n' "$(ts)" "$id" "$$" "$n" "$tools" "$(usage 20)" >> "$tx"
  printf '{"type":"user","timestamp":"%s","sessionId":"%s","message":{"role":"user","content":[{"type":"tool_result","content":"PLANTED-TOOL-RESULT"}]}}\n' "$(ts)" "$id" >> "$tx"
  printf %s "$queue" | while IFS= read -r q; do
    printf '{"type":"queue-operation","operation":"remove","timestamp":"%s","sessionId":"%s","content":"%s"}\n' "$(ts)" "$id" "$(esc "$q")" >> "$tx"
    printf '{"type":"attachment","timestamp":"%s","sessionId":"%s","attachment":{"type":"queued_command","prompt":"%s"}}\n' "$(ts)" "$id" "$(esc "$q")" >> "$tx"
  done
  queue=""
}
tick() {
  [ "$busy_until" -eq 0 ] && return
  now=$(date +%s)
  [ "$now" -ge "$next_tool" ] && { tool; next_tool=$((now + 1)); }
  if [ "$now" -ge "$busy_until" ]; then
    busy_until=0; stty min 1 time 0 2>/dev/null; reply work
    left=$queue; queue=""
    while IFS= read -r q; do [ -n "$q" ] && turn "$q"; done <<EOF
$left
EOF
  fi
}
line() {
  case "$1" in
    /exit)
      [ -e "$ctl/ignore-exit" ] && return
      [ -e "$ctl/slow-exit" ] && sleep 2
      exit 0;;
    "") ;;
    *)
      if [ "$busy_until" -gt 0 ]; then
        printf '{"type":"queue-operation","operation":"enqueue","timestamp":"%s","sessionId":"%s","content":"%s"}\n' "$(ts)" "$id" "$(esc "$1")" >> "$tx"
        queue="$queue$1$NL"
        printf 'fake-core: queued\r\n'
      else
        turn "$1"
      fi;;
  esac
}
[ $# -gt 0 ] && turn "$1"
printf 'fake-core: session %s ready\r\n\342\235\257 ' "$id"
buf=""
while :; do
  c=$(dd bs=1 count=1 2>/dev/null; echo .); c=${c%.}
  if [ -z "$c" ]; then tick; continue; fi
  if [ "$c" = "$CR" ] || [ "$c" = "$NL" ]; then
    printf '\r\n'; l=$buf; buf=""; line "$l"; continue
  fi
  buf="$buf$c"; printf %s "$c"
done
"#;

/// Write [`FAKE_CORE`] to `dir/fake-core.sh` and return its path.
pub fn write_fake_core(dir: &Path) -> Result<std::path::PathBuf> {
    std::fs::create_dir_all(dir)?;
    let p = dir.join("fake-core.sh");
    std::fs::write(&p, FAKE_CORE)?;
    Ok(p)
}
