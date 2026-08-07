# benkyou-yaro

Ramp into a new domain. You give it a target and what you already know; it builds a
disposable prerequisite graph, interviews you until it knows which parts to skip, then
produces two kinds of practice from what is left: cards for the facts, graded exercises
for the skills.

A flashcard trains the retrieval that skill acquisition is supposed to *delete*. So the
two halves have separate schedulers, and the interesting part is the bridge between them.

Design and evidence: [DESIGN.md](DESIGN.md).

## Status

The v1 loop runs end to end. What works today:

| | |
|---|---|
| graph repair | duplicates collapsed, irrelevant and over-cap nodes dropped, dangling edges cut; cycles reported whole and never cut |
| assessment | closure-leverage question selection, bulk self-declaration, four verdicts, careless-error re-ask |
| planning | budgeted topological order that stays prerequisite-closed |
| exercises | task schema, grading contract, and the twice-run validation gate |
| doing them | `attempt` lays out a workspace, `grade` runs the exercise's own grader over it and records the score |
| scheduling | mastery gating, interleaved sessions, per-hop `encompasses` credit, demotion on a failed attempt |
| cards | concept-stable note identity, allowlist sanitizer, AnkiConnect push |
| orders | `order` names the node, the assumable prerequisites and the guidance level, and hands the agent a fillable contract |

Not built: the agent writes the content. `order` says what to generate and `gate` proves
an exercise is real, but no card, exercise or graph is produced by the binary itself.
No graph editor, no visualiser, no web UI, no knowledge tracing.

## Requirements

- Rust (2021), and a Unix with `/bin/sh`
- Anki with AnkiConnect — needed only for `cards --push`
- Whatever the exercises you write call for. Nothing is required by the tool itself: a
  grader is a shell script you author, so its dependencies are yours to pick. The
  convention worth keeping is that Python graders declare their interpreter and
  dependencies in a PEP 723 header and run under `uv run --no-project`, so a kata that
  needs pandas is not blocked by a box that lacks it.

```sh
cargo build --release   # target/release/benkyou
cargo test              # 233 tests
```

## Driving it from a chat agent

The tool is meant to be operated by whatever assistant you are already talking to, and
`skill/SKILL.md` is the instruction sheet that teaches one to do it. Install both:

```sh
cargo install --path .                        # puts `benkyou` on $PATH
mkdir -p ~/.claude/skills/benkyou
cp skill/SKILL.md ~/.claude/skills/benkyou/   # or wherever your harness keeps skills
```

Or without a checkout — the skill is one self-contained file and the binary needs no
other part of the repo. Both halves have to come from the same generation, so fetch
them as a pair. Pinned to a release:

```sh
cargo install --git https://github.com/brickfrog/benkyou-yaro --tag v0.1.0
mkdir -p ~/.claude/skills/benkyou
curl -sfL https://raw.githubusercontent.com/brickfrog/benkyou-yaro/v0.1.0/skill/SKILL.md \
  -o ~/.claude/skills/benkyou/SKILL.md
```

Or tracking `main`, which is where development happens and may be ahead of the last tag:

```sh
cargo install --git https://github.com/brickfrog/benkyou-yaro
mkdir -p ~/.claude/skills/benkyou
curl -sfL https://raw.githubusercontent.com/brickfrog/benkyou-yaro/main/skill/SKILL.md \
  -o ~/.claude/skills/benkyou/SKILL.md
```

Either way `benkyou --version` tells you which binary you ended up with, and the skill
states which line it documents.

The file is a single self-contained Markdown document with YAML frontmatter — no
scripts, no assets, no repo checkout needed at runtime. Any agent that can read a skill
file and run a binary can drive the loop; nothing below depends on this repo being
present once the binary is installed.

## How it is meant to be used

The binary makes no network calls and holds no API key. It holds the state and emits
structured work orders; the agent you are already talking to does the generating with the
model you are already paying for, and writes the result back.

```sh
benkyou goals                       # what is stored, and where to write a new graph

# The agent writes the graph to ~/.local/share/benkyou/goals/ramp.json, then:
benkyou validate ramp                # repair the generated graph, report every change

# Interview. Repeat until it stops.
benkyou ask ramp                     # -> {"ask": "...", "probe": "...", "gain": 2.0}
benkyou record ramp <node> pass|partial|fail

benkyou fringe ramp                  # what you can do / what you are ready for
benkyou plan ramp <target> --budget-mins 2400

# Procedural half.
benkyou gate ./exercises/foo        # reject the exercise unless it discriminates
benkyou attempt ./exercises/foo     # lay out a workspace; grade finds it again
benkyou grade ./exercises/foo --goal ramp
benkyou session ramp --size 5        # 5 entries, weakest first; repeats if only one is due

# Declarative half.
benkyou cards cards.json --deck benkyou::ramp          # prints notes, writes nothing
benkyou cards cards.json --deck benkyou::ramp --push   # actually writes
```

There is no bundled graph or exercise library, and that is deliberate: the graph is the
product, and it is the one thing nobody can write for you. `benkyou schema` prints a
complete, valid graph — every field, one edge of each type — generated from the real
types rather than kept in sync by hand, so you start from a worked shape without
starting from someone else's domain.

## Three things worth knowing before you rely on it

**An exercise is not real until the gate has run it twice.** The reference solution must
pass and the untouched starting state must fail. Run one alone admits a check that asserts
nothing; run two alone admits an unsolvable exercise. `benkyou gate` exits non-zero on
rejection so a caller cannot go on to show it to you.

**Note identity comes from concept and role, never from card text.** Content-hashed GUIDs
are the usual default and they are wrong here: this tool re-projects a graph, so an edited
concept regenerates its cards, and a content-keyed GUID would land each one as a new note
and discard its review history.

**FSRS is used for cards and not for exercises.** It needs roughly 64 reviews per item
before its optimizer means anything; at one exercise a day that is eighteen months. The
procedural side uses mastery gating and coarse session-level spacing instead, which is the
granularity the evidence actually supports.

## Storage

A goal is referred to by name. `ramp` means `$XDG_DATA_HOME/benkyou/goals/ramp.json`,
defaulting to `~/.local/share/benkyou/goals/ramp.json`; an argument containing a `/` or
ending in `.json` is used as a path exactly as typed, so a goal checked into a repo still
works. `benkyou goals` lists what is stored.

Two plain JSON files per goal, written atomically, in the data directory:

- `<name>.json` — the graph, with learner state embedded. The graph half is disposable and
  regenerated freely; `state` is the part worth keeping.
- `<name>.fluency.json` — practice history, kept separate so regenerating the graph does
  not destroy it.

Workspaces are state rather than data and live under
`$XDG_STATE_HOME/benkyou/exercises/<concept>/<slug>/work`, default `~/.local/state`. They
are scratch for one sitting, rebuildable from the exercise directory, and graded out into
fluency. `attempt` and `grade` derive the same path from the task, so they cannot be
pointed at different directories by accident; `--work` overrides both.

No database, no daemon, no sync. Deleting any of it is a supported operation.

## Releasing

The skill and the binary are one unit shipped from two URLs, so a release is whatever
keeps them pointing at each other. Three files carry the version and all three move
together:

1. `Cargo.toml` — bump `version`.
2. `skill/SKILL.md` — the `--tag vX.Y.Z` in the bootstrap and the `documents benkyou
   X.Y.x` comment beside it.
3. `README.md` — the `--tag` and the raw-URL ref in the pinned install recipe.

Then `cargo test`, commit, `git tag -a vX.Y.Z`, and push both the branch and the tag.
Bump the minor whenever a command, flag, or output field changes, because that is the
contract the skill describes; a patch is for anything the skill would not have to
re-word. The skill on `main` points at the last release rather than at `main`, so the
pair a reader lands on is always one that was tested together.
