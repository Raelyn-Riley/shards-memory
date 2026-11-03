// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! `mem`: the one door into a shards memory. A thin layer over `shards-core`.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use shards_core::{
    resolve, sample, Address, DinitReport, Memory, Provenance, ResolveOptions, Side, Take,
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
