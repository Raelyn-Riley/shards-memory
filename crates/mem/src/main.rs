// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 Raelyn Rose

//! `mem`: the one door into a shards memory. A thin layer over `shards-core`.

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use shards_core::{resolve, sample, Address, Memory, Provenance, ResolveOptions};

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
            writeln!(out, "set {a} at {} ({})", a.home(), &commit[..12])?;
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
            writeln!(out, "superseded {a} at {} ({})", a.home(), &commit[..12])?;
        }
        Cmd::History { address } => {
            let mem = Memory::open(&cli.memory)?;
            let a = Address::parse(&address)?;
            writeln!(out, "{a}  [{}]", a.home())?;
            for e in mem.history(&a)? {
                writeln!(out)?;
                writeln!(out, "{}  {}", e.updated, &e.commit[..12])?;
                writeln!(out, "  author: {}", e.author)?;
                writeln!(out, "  human:  {}", e.human.as_deref().unwrap_or("-"))?;
                writeln!(out, "  why:    {}", e.why)?;
                writeln!(out, "  value:  {}", e.value.replace('\n', "\n          "))?;
            }
        }
        Cmd::Resolve { file, annotate } => {
            let mem = Memory::open(&cli.memory)?;
            let text = match file {
                Some(p) if p.as_os_str() != "-" => std::fs::read_to_string(p)?,
                _ => {
                    let mut s = String::new();
                    io::stdin().read_to_string(&mut s)?;
                    s
                }
            };
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
