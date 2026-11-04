// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! `mem`: the one door into a shards memory. A thin layer over `shards-core`.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use shards_core::board::{human_age, Board};
use shards_core::{
    resolve, sample, Address, DinitReport, Memory, PassReport, Provenance, ResolveOptions, Side,
    Take, WakeOutcome,
};

#[derive(Parser)]
#[command(name = "mem", version, about = "Read and write a shards memory")]
struct Cli {
    /// The memory repository to use.
    #[arg(
        long,
        short = 'C',
        global = true,
        env = "SHARDS_MEMORY",
        default_value = "."
    )]
    memory: PathBuf,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct Who {
    /// Shard / session id recorded as the author.
    #[arg(long, env = "SHARDS_AUTHOR")]
    author: Option<String>,
    /// The human at the keyboard.
    #[arg(long, env = "SHARDS_HUMAN")]
    human: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create a new, empty memory repository.
    Init {
        dir: PathBuf,
        #[command(flatten)]
        who: Who,
    },
    /// Print a fact's current value and its home path.
    Get { address: String },
    /// Create a NEW fact (refuses an existing address; use supersede to change one).
    Set {
        address: String,
        /// The value; `-` reads it from stdin.
        value: String,
        #[arg(long)]
        why: String,
        #[command(flatten)]
        who: Who,
    },
    /// Replace the value of an existing fact (old value stays in history).
    Supersede {
        address: String,
        /// The value; `-` reads it from stdin.
        value: String,
        #[arg(long)]
        why: String,
        #[command(flatten)]
        who: Who,
    },
    /// Every version of a fact, newest first.
    History { address: String },
    /// Replace every {{fact:<address>}} pointer with the current value.
    Resolve {
        /// File to read; `-` or omitted reads stdin.
        file: Option<PathBuf>,
        /// Add the fact's address after each value.
        #[arg(long)]
        annotate: bool,
    },
    /// Build a small FAKE flock memory for tests.
    GenSample {
        dir: PathBuf,
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Write a notes/, people/ or soul/ markdown file (a soul change on main
    /// needs an admin --human).
    Put {
        path: String,
        /// File to read the contents from; `-` or omitted reads stdin.
        file: Option<PathBuf>,
        #[arg(long)]
        why: String,
        #[command(flatten)]
        who: Who,
    },
    /// Open, list and describe shards.
    #[command(subcommand)]
    Shard(ShardCmd),
    /// Merge shard/<id> into main under the lock.
    Dinit {
        #[arg(long)]
        id: String,
        /// Merge nothing if anything conflicts.
        #[arg(long)]
        all_or_nothing: bool,
    },
    /// Settle a conflict recorded by dinit.
    ResolveConflict {
        /// A fact address, or a repo path ending in .md (people/, notes/).
        target: String,
        /// `shard`, `main`, or a literal new value.
        #[arg(long)]
        take: String,
        #[arg(long)]
        why: String,
        /// Which shard's conflict, if several shards conflict on it.
        #[arg(long)]
        shard: Option<String>,
        #[command(flatten)]
        who: Who,
    },
    /// Soul proposals.
    #[command(subcommand)]
    Soul(SoulCmd),
    /// What every open shard is doing, read from git and its transcript.
    Board {
        /// Machine-readable output (format documented in the README).
        #[arg(long)]
        json: bool,
    },
    /// Evaluate [triggers] every [awareness] poll_secs; wake the core when one fires.
    Watch {
        /// Run one pass and exit (for a timer or cron).
        #[arg(long)]
        once: bool,
    },
    /// The reasoning core's view.
    #[command(subcommand)]
    Core(CoreCmd),
    /// Test helpers.
    #[command(subcommand, hide = true)]
    Debug(DebugCmd),
}

#[derive(Subcommand)]
enum ShardCmd {
    /// Create shard/<id> from main in its own worktree; prints the path.
    Open {
        #[arg(long)]
        id: String,
        #[arg(long, env = "SHARDS_HUMAN")]
        human: Option<String>,
    },
    /// Open shards: human, base, last commit, status line.
    List,
    /// Set a shard's one-line status ("working on X").
    Status {
        #[arg(long)]
        id: String,
        gist: String,
    },
}

#[derive(Subcommand)]
enum SoulCmd {
    /// Approve a shard's soul proposal (admins only, from shards.toml).
    Approve {
        shard: String,
        #[arg(long, env = "SHARDS_HUMAN")]
        human: Option<String>,
        #[arg(long, env = "SHARDS_AUTHOR")]
        author: Option<String>,
        #[arg(long, default_value = "")]
        why: String,
    },
}

#[derive(Subcommand)]
enum CoreCmd {
    /// Print `mem board --json` plus the pending events, as one JSON document.
    Context,
}

#[derive(Subcommand)]
enum DebugCmd {
    /// Take the main lock and hold it (for crash-recovery tests).
    HoldLock {
        #[arg(long, default_value_t = 60.0)]
        secs: f64,
        #[arg(long, default_value = "debug-holder")]
        holder: String,
    },
}

fn short(c: &str) -> &str {
    &c[..c.len().min(12)]
}

fn side_line(s: &Side) -> String {
    format!(
        "{}  (by {}{}, {}, why: {})",
        s.value.replace('\n', " ⏎ "),
        s.author,
        s.human
            .as_deref()
            .map(|h| format!(" / {h}"))
            .unwrap_or_default(),
        s.updated,
        s.why
    )
}

fn print_dinit(out: &mut impl Write, r: &DinitReport) -> io::Result<()> {
    if r.aborted {
        writeln!(
            out,
            "dinit {} ABORTED (--all-or-nothing): {} conflict(s); main untouched",
            r.shard,
            r.conflicts.len()
        )?;
    } else {
        match &r.commit {
            Some(c) => writeln!(out, "dinit {} -> main at {}", r.shard, short(c))?,
            None => writeln!(out, "dinit {}: nothing new to merge", r.shard)?,
        }
    }
    for p in &r.merged {
        writeln!(out, "  merged    {p}")?;
    }
    for p in &r.unchanged {
        writeln!(out, "  same      {p}")?;
    }
    for c in &r.conflicts {
        writeln!(out, "  CONFLICT  {}  (record: {})", c.target, c.record)?;
        writeln!(out, "      main:  {}", side_line(&c.main))?;
        writeln!(out, "      shard: {}", side_line(&c.shard))?;
    }
    for p in &r.proposals {
        writeln!(
            out,
            "  PROPOSAL  {p}  (needs a human admin: mem soul approve {} --human <admin>)",
            r.shard
        )?;
    }
    for p in &r.skipped {
        writeln!(out, "  skipped   {p}  (dinit never merges this)")?;
    }
    if r.closed {
        writeln!(
            out,
            "shard {} closed: worktree removed, branch {}",
            r.shard,
            if r.branch_deleted { "deleted" } else { "kept" }
        )?;
    } else if let Some(why) = &r.kept_open {
        writeln!(out, "shard {} left open: {why}", r.shard)?;
    } else if !r.aborted {
        writeln!(
            out,
            "shard {} stays open until its conflicts/proposals are settled",
            r.shard
        )?;
    }
    Ok(())
}

fn print_board(out: &mut impl Write, b: &Board) -> io::Result<()> {
    if b.shards.is_empty() {
        writeln!(out, "no open shards")?;
    }
    for s in &b.shards {
        let others = s.in_flight.len() - s.facts_in_flight;
        writeln!(
            out,
            "{}  {}  {}  last activity {} ago ({})  in flight: {} fact(s){}",
            s.id,
            s.human.as_deref().unwrap_or("-"),
            s.state.as_str(),
            human_age(s.age_secs),
            s.last_activity_from,
            s.facts_in_flight,
            if others > 0 {
                format!(", {others} other")
            } else {
                String::new()
            }
        )?;
        if let Some(g) = &s.gist {
            writeln!(out, "    gist:   {g}")?;
        }
        if !s.in_flight.is_empty() {
            let items: Vec<String> = s
                .in_flight
                .iter()
                .map(|f| {
                    format!(
                        "{}{}",
                        f.address.as_deref().unwrap_or(&f.path),
                        if f.uncommitted { "*" } else { "" }
                    )
                })
                .collect();
            writeln!(out, "    flight: {}  (* uncommitted)", items.join(", "))?;
        }
        for a in &s.recent_actions {
            let age = chrono_age(&b.generated, &a.at);
            if a.summary.is_empty() {
                writeln!(out, "    {age:>4} ago  {}", a.tool)?;
            } else {
                writeln!(out, "    {age:>4} ago  {}  {}", a.tool, a.summary)?;
            }
        }
    }
    if !b.overlaps.is_empty() {
        writeln!(out, "\noverlaps:")?;
        for o in &b.overlaps {
            writeln!(
                out,
                "  {:<12}  {}  shards {}{}",
                o.kind.as_str(),
                o.target,
                o.shards.join(", "),
                if o.humans.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", o.humans.join(", "))
                }
            )?;
        }
    }
    if !b.conflicts.is_empty() {
        writeln!(out, "\nopen conflicts:")?;
        for c in &b.conflicts {
            writeln!(out, "  {}  from {}  ({})", c.target, c.shard, c.record)?;
        }
    }
    if !b.soul_proposals.is_empty() {
        writeln!(out, "\nsoul proposals:")?;
        for p in &b.soul_proposals {
            writeln!(
                out,
                "  {} ({})  {}",
                p.shard,
                p.human.as_deref().unwrap_or("-"),
                p.paths.join(", ")
            )?;
        }
    }
    Ok(())
}

/// Age of RFC 3339 `at` relative to `now`, e.g. "4m".
fn chrono_age(now: &str, at: &str) -> String {
    use chrono::DateTime;
    match (
        DateTime::parse_from_rfc3339(now),
        DateTime::parse_from_rfc3339(at),
    ) {
        (Ok(n), Ok(a)) => human_age((n - a).num_seconds().max(0)),
        _ => "?".to_string(),
    }
}

fn print_pass(out: &mut impl Write, r: &PassReport) -> io::Result<()> {
    for e in &r.fired {
        writeln!(out, "event {} {}: {}", e.id, e.kind.as_str(), e.summary)?;
    }
    match &r.wake {
        WakeOutcome::Idle => {}
        WakeOutcome::NoCommand => {
            if !r.fired.is_empty() {
                writeln!(
                    out,
                    "{} event(s) pending; no [wake] command set, nothing run",
                    r.pending
                )?;
            }
        }
        WakeOutcome::Cooldown { until } => {
            if !r.fired.is_empty() {
                writeln!(
                    out,
                    "{} event(s) pending; cooldown until {until}",
                    r.pending
                )?;
            }
        }
        WakeOutcome::Busy => writeln!(out, "another wake is running; events stay pending")?,
        WakeOutcome::Delivered { events, secs } => {
            writeln!(out, "woke the core: {events} event(s) in {secs:.1}s")?
        }
        WakeOutcome::Failed { events, reason } => writeln!(
            out,
            "wake failed ({reason}); {events} event(s) stay pending, see .shards/wake.log"
        )?,
    }
    Ok(())
}

fn read_value(v: String) -> io::Result<String> {
    if v == "-" {
        let mut s = String::new();
        io::stdin().read_to_string(&mut s)?;
        Ok(s.strip_suffix('\n').map(String::from).unwrap_or(s))
    } else {
        Ok(v)
    }
}

fn prov(who: Who, why: String) -> Provenance {
    Provenance::new(who.author.unwrap_or_default(), who.human, why)
}

fn read_input(file: Option<PathBuf>) -> io::Result<String> {
    match file {
        Some(p) if p.as_os_str() != "-" => std::fs::read_to_string(p),
        _ => {
            let mut s = String::new();
            io::stdin().read_to_string(&mut s)?;
            Ok(s)
        }
    }
}

fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = io::stdout().lock();
    match cli.cmd {
        Cmd::Init { dir, who } => {
            Memory::init(&dir, &prov(who, String::new()))?;
            writeln!(out, "initialized memory at {}", dir.display())?;
        }
        Cmd::Get { address } => {
            let mem = Memory::open(&cli.memory)?;
            let f = mem.get(&Address::parse(&address)?)?;
            writeln!(out, "{}", f.value)?;
            writeln!(out, "home: {}", f.home)?;
        }
        Cmd::Set {
            address,
            value,
            why,
            who,
        } => {
            let mem = Memory::open(&cli.memory)?;
            let a = Address::parse(&address)?;
            let commit = mem.set(&a, &read_value(value)?, &prov(who, why))?;
            writeln!(out, "set {a} at {} ({})", a.home(), short(&commit))?;
        }
        Cmd::Supersede {
            address,
            value,
            why,
            who,
        } => {
            let mem = Memory::open(&cli.memory)?;
            let a = Address::parse(&address)?;
            let commit = mem.supersede(&a, &read_value(value)?, &prov(who, why))?;
            writeln!(out, "superseded {a} at {} ({})", a.home(), short(&commit))?;
        }
        Cmd::History { address } => {
            let mem = Memory::open(&cli.memory)?;
            let a = Address::parse(&address)?;
            writeln!(out, "{a}  [{}]", a.home())?;
            for e in mem.history(&a)? {
                writeln!(out)?;
                writeln!(out, "{}  {}", e.updated, short(&e.commit))?;
                writeln!(out, "  author: {}", e.author)?;
                writeln!(out, "  human:  {}", e.human.as_deref().unwrap_or("-"))?;
                writeln!(out, "  why:    {}", e.why)?;
                writeln!(out, "  value:  {}", e.value.replace('\n', "\n          "))?;
            }
        }
        Cmd::Resolve { file, annotate } => {
            let mem = Memory::open(&cli.memory)?;
            let text = read_input(file)?;
            write!(
                out,
                "{}",
                resolve(&mem, &text, ResolveOptions { annotate })?
            )?;
        }
        Cmd::GenSample { dir, seed } => {
            let s = sample::generate(&dir, seed)?;
            writeln!(
                out,
                "fake flock at {}: birds {}, {} facts, {} supersedes, notes: {}",
                dir.display(),
                s.birds.join(", "),
                s.facts,
                s.supersedes,
                s.notes.join(", ")
            )?;
        }
        Cmd::Put {
            path,
            file,
            why,
            who,
        } => {
            let mem = Memory::open(&cli.memory)?;
            let text = read_input(file)?;
            let commit = mem.put_file(&path, &text, &prov(who, why))?;
            writeln!(out, "wrote {path} ({})", short(&commit))?;
        }
        Cmd::Shard(ShardCmd::Open { id, human }) => {
            let mem = Memory::open(&cli.memory)?;
            let meta = mem.shard_open(&id, human.as_deref())?;
            writeln!(out, "{}", meta.worktree.display())?;
            eprintln!(
                "mem: opened shard {id} on {} from main at {}",
                meta.branch,
                short(&meta.base)
            );
        }
        Cmd::Shard(ShardCmd::List) => {
            let mem = Memory::open(&cli.memory)?;
            let shards = mem.shard_list()?;
            if shards.is_empty() {
                writeln!(out, "no open shards")?;
            }
            for s in shards {
                writeln!(
                    out,
                    "{}  human: {}  base: {}  last commit: {}",
                    s.meta.id,
                    s.meta.human.as_deref().unwrap_or("-"),
                    short(&s.meta.base),
                    s.last_commit
                )?;
                match &s.status {
                    Some(st) => writeln!(out, "    status: {}  ({})", st.gist, st.at)?,
                    None => writeln!(out, "    status: -")?,
                }
                if !s.meta.soul_pending.is_empty() {
                    writeln!(
                        out,
                        "    soul proposal pending: {}",
                        s.meta.soul_pending.join(", ")
                    )?;
                }
            }
        }
        Cmd::Shard(ShardCmd::Status { id, gist }) => {
            let mem = Memory::open(&cli.memory)?;
            let st = mem.shard_set_status(&id, &gist)?;
            writeln!(out, "{id}: {} ({})", st.gist, st.at)?;
        }
        Cmd::Dinit { id, all_or_nothing } => {
            let mem = Memory::open(&cli.memory)?;
            let r = mem.dinit(&id, all_or_nothing)?;
            print_dinit(&mut out, &r)?;
            if r.aborted {
                return Err(format!("dinit {id} aborted: conflicts with --all-or-nothing").into());
            }
        }
        Cmd::ResolveConflict {
            target,
            take,
            why,
            shard,
            who,
        } => {
            let mem = Memory::open(&cli.memory)?;
            let commit = mem.resolve_conflict(
                &target,
                &Take::parse(&take),
                shard.as_deref(),
                &prov(who, why),
            )?;
            writeln!(out, "resolved {target} (took {take}) ({})", short(&commit))?;
        }
        Cmd::Soul(SoulCmd::Approve {
            shard,
            human,
            author,
            why,
        }) => {
            let mem = Memory::open(&cli.memory)?;
            let p = Provenance::new(author.unwrap_or_default(), human, why);
            let commit = mem.soul_approve(&shard, &p)?;
            writeln!(
                out,
                "approved soul change from {shard} ({})",
                short(&commit)
            )?;
        }
        Cmd::Board { json } => {
            let mem = Memory::open(&cli.memory)?;
            let b = mem.board(chrono::Utc::now())?;
            if json {
                writeln!(out, "{}", serde_json::to_string_pretty(&b)?)?;
            } else {
                print_board(&mut out, &b)?;
            }
        }
        Cmd::Core(CoreCmd::Context) => {
            let mem = Memory::open(&cli.memory)?;
            let ctx = mem.core_context(chrono::Utc::now())?;
            writeln!(out, "{}", serde_json::to_string_pretty(&ctx)?)?;
        }
        Cmd::Watch { once } => loop {
            // Re-open every pass, so shards.toml changes apply at once.
            let pass = Memory::open(&cli.memory).and_then(|m| {
                let poll = m.config().awareness.poll();
                Ok((m.watch_pass(chrono::Utc::now())?, poll))
            });
            match pass {
                Ok((r, poll)) => {
                    print_pass(&mut out, &r)?;
                    out.flush()?;
                    if once {
                        break;
                    }
                    std::thread::sleep(poll);
                }
                Err(e) if once => return Err(e.into()),
                Err(e) => {
                    // Keep watching; the next pass may succeed.
                    eprintln!("mem: watch pass failed: {e}");
                    std::thread::sleep(std::time::Duration::from_secs(5));
                }
            }
        },
        Cmd::Debug(DebugCmd::HoldLock { secs, holder }) => {
            let mem = Memory::open(&cli.memory)?;
            let guards = mem.lock_main(&[], &holder, None)?;
            writeln!(out, "held {}", guards.len())?;
            out.flush()?;
            std::thread::sleep(std::time::Duration::from_secs_f64(secs));
            drop(guards);
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("mem: {e}");
            ExitCode::FAILURE
        }
    }
}
