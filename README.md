# shards

`shards` is a shared memory for AI agents. A *memory* is a plain git repository of
markdown fact files. Every fact lives at exactly one path, and that path is its only
home: `facts/devices/rotoforge-ctl/ip.md` is addressed everywhere as
`devices/rotoforge-ctl/ip`. Notes and chat use pointers such as
`{{fact:devices/rotoforge-ctl/ip}}` in place of copied values, so a fact changes in one
place and every reader sees the new value. Each change is one commit on `main`, stamped
with the time, the shard (session) that made it, the human at the keyboard and the
reason. Old values stay readable through history, and nothing in the tool can rewrite
that history.

All reads and writes go through one small tool, `mem`, which is a thin CLI over the
`shards-core` library. This repository holds the first slice: storing facts, creating
and superseding them, history, pointers, and a fake-data generator for tests. Locks,
shards/branches/merging, bulletins and the web UI come later. See
[`docs/design.md`](docs/design.md) for the full design. Licensed AGPL-3.0-or-later.

## Build

Requires a stable Rust toolchain (via [rustup](https://rustup.rs)) and `git` ≥ 2.28 on
`PATH`.

```sh
cargo build --release     # binary at target/release/mem
cargo test
```

### Why shell out to `git` instead of libgit2

`shards-core` runs the `git` binary through a small wrapper (`crates/shards-core/src/git.rs`)
instead of linking `git2`/libgit2. The reasons:

- **The guarantee is easy to check.** The wrapper accepts only an allowlist of
  subcommands (`init add commit log show cat-file rev-parse rev-list symbolic-ref status
  diff-tree ls-files`) and refuses `--amend`, `--force`, `-f`, `--fixup` and
  `--squash` outright. No code path can rebase, reset, force-push or amend, and a unit
  test asserts this.
- **It behaves exactly like git.** Users and agents inspect the memory with the same
  `git log` and `git show` that the tool uses.
- **No C dependency.** That means no libgit2, OpenSSL or pkg-config build chain.

The cost is one process per git call. That is fine at this scale.

## Commands

Global: `-C, --memory <dir>` (or `SHARDS_MEMORY`) picks the memory and defaults to
`.`. Writes need an author from `--author` or `SHARDS_AUTHOR`. A human can be given with
`--human` or `SHARDS_HUMAN`. A value of `-` reads the value from stdin.

| Command | Example |
| --- | --- |
| `mem init <dir>` creates a new memory (`facts/`, `soul/`, `people/`, README, first commit). | `mem init ~/flock-memory` |
| `mem get <address>` prints the value, then `home: <path>`. An unknown address exits 1. | `mem get devices/rotoforge-ctl/ip` |
| `mem set <address> <value> --why ...` creates a **new** fact. It refuses only an exact existing address (use `supersede`). Near-duplicates such as the same name elsewhere are left to the later drift checker, which flags them for a human. | `mem set devices/rotoforge-ctl/ip 10.0.4.40 --why "first seen on the bench"` |
| `mem supersede <address> <value> --why ...` replaces an existing fact's value as one commit that touches only that file. | `mem supersede devices/rotoforge-ctl/ip 10.0.4.41 --why "moved to the new switch"` |
| `mem history <address>` shows every version, newest first: time, commit, author, human, why and value. | `mem history devices/rotoforge-ctl/ip` |
| `mem resolve [file\|-] [--annotate]` replaces every `{{fact:...}}` with the current value. `--annotate` gives `10.0.4.41 [devices/rotoforge-ctl/ip]`. Any unresolvable pointer is an error that names it. | `mem resolve notes/runbook.md --annotate` |
| `mem gen-sample <dir> --seed N` builds a fake flock: 3 birds, 30 facts in 4 sections, 3 supersedes, 3 notes with pointers. The same seed gives byte-identical commits. | `mem gen-sample /tmp/flock --seed 7` |

Address rules: at least `section/name`. Segments use only `a-z 0-9 - _ .`, start with a
letter or digit, and carry no `facts/` prefix or `.md` suffix. Lowercase only, so two
homes can't differ just by case.

## Fact file format

```markdown
---
address: "devices/rotoforge-ctl/ip"
updated: "2026-11-03T15:04:05Z"
author: "shard-7f3a"
human: "Tess"
why: "moved to the new switch"
---
10.0.4.41
```

- The file opens with YAML front matter. Keys come in the fixed order above, and each
  value is a double-quoted string with JSON escaping, which is also valid YAML.
- `updated` is RFC 3339 UTC, written by the tool. It is never edited by hand.
- `human` is omitted when no human is known. `author`, `human` and `why` are single-line.
- The body is the value followed by exactly one newline. Values may span several lines.
- Unknown front-matter keys are ignored when reading.

## Commit message format

```text
supersede: devices/rotoforge-ctl/ip

Shards-Op: supersede
Shards-Address: devices/rotoforge-ctl/ip
Shards-Author: shard-7f3a
Shards-Human: Tess
Shards-Why: moved to the new switch
```

- The subject is `<op>: <address>`. The ops are `init` (subject `init: memory`), `set`,
  `supersede` and `note` (where the subject is the note's path).
- The body is git trailers, so `git log --format='%(trailers:key=Shards-Why,valueonly)'`
  works. `Shards-Human` is omitted when there is no human.
- The git author and committer are both the shard: name `<author>`, email
  `<author>@shards.invalid`. Both git dates equal the fact's `updated` time.
- Every write commits exactly one path (`git commit --only -- <path>`), even when
  other changes sit staged in the repo. Writes are refused unless `main` is checked out,
  and refused if the target file has uncommitted edits.
