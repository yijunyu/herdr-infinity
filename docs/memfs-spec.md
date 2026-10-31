# The MemFS bridge: specification

Letta's MemFS and Claude Code's memory directory store memory the same way —
one fact per Markdown file, with YAML frontmatter whose `description` decides
when the fact is relevant. That shared substrate is what makes a bridge
possible. This document specifies the bridge's observable behaviour: what each
command guarantees, and where the two formats cannot be reconciled.

Each requirement has an identifier (`M1`, `F2`, …) used by the regression
tests, so a failing test names the clause it broke.

## 1. The two layouts

|                   | Letta MemFS                | Claude Code                       |
| ----------------- | -------------------------- | --------------------------------- |
| location          | `$MEMORY_DIR/system/*.md`  | `<memory>/[a-z-]*.md`             |
| index             | none — the directory is it | `MEMORY.md`, one line per file    |
| extra frontmatter | `name`, `description`      | `name`, `description`, `metadata.type` |

### M1 — Layout is detected, never imposed

MemFS appears in two shapes in practice: `<root>/system/*.md`, which the
bundled authoring instructions describe, and `<root>/*.md`, which Letta's local
backend actually creates under
`~/.letta/lc-local-backend/memfs/<agent>/memory/`. A sync writes back into
whichever layout already holds memory files, so it never reorganises an
agent's memory. An empty or absent root falls back to `system/`.

### M2 — A directory holds memory only if a file parses as memory

Presence of `.md` files does not make a directory a memory store: a root may
hold only `MEMORY.md`, or a stray README. Detection requires at least one file
that parses as a memo.

### M3 — A missing directory is empty, not an error

Either side may not exist yet. Reading an absent directory yields no memories
rather than failing.

### M4 — Non-memory files are skipped, not rejected

A file without frontmatter is not a memory fact, and `MEMORY.md` is an index
rather than a memory. Both are ignored, so unrelated files in the same
directory do no harm.

## 2. The memo

### F1 — `description` is the only required field

A file without parseable frontmatter carrying a `description` is not a memo.
Identity is the `name` field, falling back to the file stem — MemFS files
carry no `name` of their own.

### F2 — An undeclared type is `reference`, and knows it

MemFS has no equivalent of `metadata.type`, so a memo imported from MemFS is
typed `reference`. The memo also records that the type was *not* declared by
the file, because a type filter must not match a memo that never declared
one: defaulting to `reference` and then filtering on `reference` would select
every untyped memory in a project.

### F3 — Both formats are written in full

Claude Code's format carries `name`, `description` and `metadata.type`. MemFS
carries `name` and `description`: both are required, because Letta validates
memory on commit and rejects a file missing either, so a `description`-only
file is written and then refused by the MemFS git hook. `metadata` has no
MemFS equivalent and is omitted.

### F4 — A name is free text; a filename is not

Memory names in the wild carry whole sentences, with `/`, em dashes and
punctuation. A `/` in a filename makes the write fail with ENOENT against a
directory that does not exist. Lowercase alphanumerics and single hyphens
survive; dots survive inside a word but never lead; everything else collapses
to a hyphen. A name of pure punctuation would yield an empty filename, so it
becomes `memo`.

## 3. Syncing

### S1 — Nothing is written implicitly

Memory is long-lived context that steers an agent's behaviour, so a surprise
write is expensive to undo. Syncing happens only when asked; nothing runs on a
timer. Every operation reports what it changed, and `--dry-run` reports the
plan while writing nothing.

### S2 — Only `description` and body decide staleness

A memo's type is Claude Code-only, so comparing it would report a spurious
change on every file of a MemFS round trip. Bodies compare trimmed.

### S3 — Round trips settle

`import` then `export` leaves both sides reporting "nothing to do". Repeating
either command is a no-op: the second run reports every memo as already in
sync. Bodies compare trimmed (S2), so a hand-edited file with trailing
whitespace does not make a round trip oscillate.

### S4 — Claude Code's index is derived, never merged

On import, `MEMORY.md` is rebuilt from the files that exist, so a stale entry
pointing at a deleted file cannot survive. The index is not written on export:
MemFS treats the directory itself as the index (see M1), so writing one there
would add a file Letta does not expect.

## 4. Seeding a Letta agent

Seeding is export under a budget Claude Code does not have. Letta renders
every root memory file into the agent's context on *every turn* — the root
files *are* the system prompt. Letta's own guidance is to keep them lean:
"compact indexes and behavioral rules over bulk content", with detail in
indexed child directories. Seeding a project's accumulated memories into the
root therefore spends the agent's permanent context budget before it has
learned anything. In practice one agent reached 77% of the 65536-character cap.

### I1 — `--into <dir>` keeps the root lean

`--into` writes `<memory>/<dir>/*.md` instead of the root. Measured on three
agents, the root drops from 12k–50k characters to 777 (about 1% of the cap),
with the bulk moved into the child directory.

### I2 — A child directory needs its own index

A child directory counts as memory only when it holds its own
`MEMORY.md`, and that index must be frontmatter-free. `--into` writes it, so
the agent discovers the facts through the index rather than carrying them all
in its prompt. This is the one place the bridge writes an index on the MemFS
side, and the exception M1/S4 would otherwise forbid.

The index is rebuilt from the directory's contents, not from the current run's
selection, so a second seeding run does not make previously seeded facts
undiscoverable by dropping them from the index.

### I3 — A subdirectory name cannot escape or hide

The name must be a single plain directory name. An empty name, one containing
`/`, or one starting with `.` is rejected: a path would write outside the
agent's memory, and a dotfile would hide from the index.

### I4 — `--into` is export-only

Import has no corresponding notion — there is no budget to protect on the
Claude Code side — so `--into` with `import` is an error rather than a
silently ignored flag.

### I5 — `--only-types` seeds selectively

`--only-types feedback,reference` copies only memories whose declared type is
in the list. `feedback` and `reference` hold durable guidance, while `project`
and `memory` are mostly dated status snapshots. An empty filter keeps
everything. Per F2, an undeclared type matches nothing.

## 5. Resolution

### R1 — `--memory-dir` falls back to `$MEMORY_DIR`

Letta sets `MEMORY_DIR` for an agent session. Without either, the command
fails with a message naming both options rather than guessing.

### R2 — `--claude-dir` is derived from the project path

Claude Code derives its memory directory from the project path by replacing
path separators with dashes, under
`~/.claude/current/projects/<slug>/memory`.

## 6. Known external limit

`letta -n <name>` fails with "--name cannot be used with --agent" even when no
`--agent` is passed, and even for a pre-existing pinned agent. This is a letta
0.34.31 bug, not a consequence of anything the bridge does. Address a seeded
agent with `letta --agent <id>`.
