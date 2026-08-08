# benkyou-yaro

benkyou-yaro helps you learn a new domain quickly. You give it a target and a list of
what you know. It builds a disposable prerequisite graph. It then asks you questions
until it knows which parts to skip. From the rest it makes two kinds of practice: cards
for the facts, and graded exercises for the skills.

A flashcard trains recall. Skill practice must make recall unnecessary. For this reason
the two halves have separate schedulers. The bridge between the two schedulers is the
hard part of the design.

Design and evidence: [DESIGN.md](DESIGN.md).

## Status

The v1 loop runs end to end. This works today:

| | |
|---|---|
| graph repair | Merges duplicate nodes. Deletes irrelevant nodes and nodes over the cap. Deletes dangling edges. Prints each cycle whole and never cuts one. |
| assessment | Asks the question that settles the most other nodes. Accepts bulk self-declaration. Gives four verdicts. Asks again after a careless error. |
| planning | Makes a budgeted order that stays prerequisite-closed. |
| exercises | Task schema, grading contract, and a gate that runs each exercise twice. |
| attempts | `attempt` lays out a workspace. `grade` runs the grader of the exercise over that workspace and records the score. |
| scheduling | Mastery gating, interleaved sessions, per-hop `encompasses` credit, and demotion after a failed attempt. |
| cards | Concept-stable note identity, allowlist sanitizer, and AnkiConnect push. |
| orders | `order` names the node, the prerequisites you can assume, and the guidance level. It hands the agent a fillable contract. |
| browser runner | `serve` gives a queue of exercises an editor, a Run button and a Submit button on `127.0.0.1`. The code runs in this process, not in the tab. |

The binary writes no content. `order` states what to write, and `gate` proves that an
exercise is real. The binary itself makes no card, no exercise, and no graph. There is
no graph editor, no visualiser, and no knowledge tracing.

## Requirements

- Rust (2021), and a Unix system with `/bin/sh`
- `bubblewrap` (`bwrap`), for the sandbox. Every command that runs a script needs it.
- Anki with AnkiConnect, for `cards --push` only
- The programs that your own exercises call

A grader is a shell script that you write. It runs with no network, so it can use only
what the machine already has. System Python packages are visible. A virtualenv is not.
An exercise that needs pandas needs pandas installed on the machine, and the gate
rejects the exercise when it is missing.

`gate`, `attempt`, `grade` and `serve` run generated scripts in a sandbox. There is no
network, no access to your files, and no access to your study state. The default needs
`bubblewrap`. Without it the tool stops and tells you so.

CAUTION: `--unsafe-host` turns the sandbox off. The scripts then run with your own user
permissions, over your whole filesystem. Read a generated `check/check.sh` and
`solution/solve.sh` before you use that flag.

```sh
cargo build --release   # target/release/benkyou
cargo test              # 293 tests
```

## Use it from a chat agent

Any assistant you already talk to can operate this tool. `skill/SKILL.md` teaches one
how. Install both parts.

From a checkout:

```sh
cargo install --path .                        # puts `benkyou` on $PATH
mkdir -p ~/.claude/skills/benkyou
cp skill/SKILL.md ~/.claude/skills/benkyou/   # or wherever your harness keeps skills
```

The skill is one file, and the binary needs no other part of the repo. Both parts must
come from the same generation, so get them as a pair.

Pinned to a release:

```sh
cargo install --git https://github.com/brickfrog/benkyou-yaro --tag v0.1.1
mkdir -p ~/.claude/skills/benkyou
curl -sfL https://raw.githubusercontent.com/brickfrog/benkyou-yaro/v0.1.1/skill/SKILL.md \
  -o ~/.claude/skills/benkyou/SKILL.md
```

From `main`, which can be ahead of the last tag:

```sh
cargo install --git https://github.com/brickfrog/benkyou-yaro
mkdir -p ~/.claude/skills/benkyou
curl -sfL https://raw.githubusercontent.com/brickfrog/benkyou-yaro/main/skill/SKILL.md \
  -o ~/.claude/skills/benkyou/SKILL.md
```

`benkyou --version` prints the version of the binary. The skill states which version it
documents.

The skill file is one Markdown document with YAML frontmatter. It needs no scripts, no
assets, and no repo checkout at runtime. Any agent that can read a skill file and run a
binary can drive the loop.

## How the loop runs

The binary makes no network call and holds no API key. It holds the state and prints
structured work orders. Your agent writes the content with the model that you already
pay for. The agent then writes the result back.

```sh
benkyou goals                        # what is stored, and where to write a new graph

# The agent writes the graph to ~/.local/share/benkyou/goals/ramp.json, then:
benkyou validate ramp                # repair the graph and print every change

# Interview. Repeat until it stops.
benkyou ask ramp                     # -> {"ask": "...", "probe": "...", "gain": 2.0}
benkyou record ramp <node> pass|partial|fail

benkyou fringe ramp                  # what you can do, and what you are ready for
benkyou plan ramp <target> --budget-mins 2400

# Procedural half.
benkyou gate ./exercises/foo         # reject the exercise unless it discriminates
benkyou attempt ./exercises/foo      # lay out a workspace, which grade finds again
benkyou grade ./exercises/foo --goal ramp
benkyou session ramp --size 5        # 5 entries, weakest first

# Or do the same work in a browser instead of an editor.
benkyou serve ./exercises/foo ./exercises/bar --goal ramp

# Declarative half.
benkyou cards cards.json --deck benkyou::ramp          # prints notes, writes nothing
benkyou cards cards.json --deck benkyou::ramp --push   # writes the notes
```

This tool ships no graph and no exercise library. That is deliberate. The graph is the
product, and it is the one thing that nobody can write for you. `benkyou schema` prints
a complete and valid graph, with every field and one edge of each type. The tool builds
that output from the real types, so it cannot go stale. You start from a worked shape
and not from the domain of somebody else.

## Three things to know first

**An exercise is not real until the gate runs it twice.** The reference solution must
pass. The untouched start state must fail. Without the second run, a grader that always
passes looks correct. Without the first run, an exercise that nobody can solve looks
correct. `benkyou gate` exits non-zero after a rejection, so a caller cannot show you a
bad exercise.

**A gate result is bound to the files it ran on.** `benkyou gate` writes `.gate.json`
beside the exercise. It holds a hash of `task.toml`, `instruction.md`, `setup/`,
`check/` and `solution/`. Change any of them and `attempt`, `grade` and `serve` refuse
the exercise until you gate it again. The gate writes nothing else, so your own files
come back unchanged.

**Note identity comes from the concept and the role, and never from the card text.**
Content-hashed GUIDs are the usual default, and they are wrong here. This tool projects
a graph again after each edit. An edited concept therefore writes its cards again. A
content-keyed GUID lands each card as a new note, and Anki deletes the review history.

**FSRS is for the cards and not for the exercises.** FSRS schedules one item and shows
it to you again. An exercise is used up when you solve it. A second attempt tests your
memory of your own solution, and not the skill. The unit that survives repetition is
the concept, with a new exercise each time. The procedural half therefore uses mastery
gating and coarse session-level spacing.

## The browser runner

`benkyou serve` gives a queue of exercises an editor, a Run button and a Submit button.
It prints one URL and opens it. The URL holds a token for that one session.

```sh
benkyou serve ./exercises/foo ./exercises/bar --goal ramp
```

The queue is the argument list, in that order. There is no queue from a goal, because
nothing here maps a concept to a directory: this tool ships no exercises and does not
name your exercise library. You give the paths, as you do for `attempt` and `grade`.

**Your code runs in this process, and never in the tab.** Run and Submit both go
through the same runner and the same grader the CLI uses. Put a second engine in the
page, such as Pyodide, and it gives you a green in the browser and a red from `grade`.
You then believe the first one you saw. The page edits files and shows output.

`Run` needs a command to call. Declare it in `task.toml`, and the gate then checks it
against the reference solution and warns you if it fails:

```toml
[workspace]
run_cmd = "python3 solution.py"
```

Without it the Run button does not appear, and Submit still grades.

Each attempt writes `attempt.jsonl` beside the workspace: one line for each open, run,
submit and step. Durations come from a monotonic clock. This record is history and it
is read by a person. **No part of it reaches the scheduler.**

## Storage

A goal has a name. `ramp` means `$XDG_DATA_HOME/benkyou/goals/ramp.json`. The default is
`~/.local/share/benkyou/goals/ramp.json`. An argument that holds a `/`, or that ends in
`.json`, is a path exactly as typed. A goal inside a repo therefore still works.
`benkyou goals` lists what is stored.

Each goal has two plain JSON files in the data directory. The tool writes both files
atomically.

- `<name>.json` — the graph, with the learner state inside it. The graph half is
  disposable, and you can build it again at any time. The `state` field is the part
  worth keeping.
- `<name>.fluency.json` — the practice history. It is a separate file, so a new graph
  does not destroy it.

A workspace is state and not data. Each one lives under
`$XDG_STATE_HOME/benkyou/exercises/<concept>/<slug>/work`, default `~/.local/state`. A
workspace is scratch for one sitting, and you can build it again from the exercise
directory. `attempt` and `grade` derive the same path from the task, so you cannot point
them at different directories by accident. `--work` overrides both.

There is no database, no daemon, and no sync. You can delete any of it.

## Releasing

The skill and the binary are one unit that ships from two URLs. A release keeps the two
pointed at each other. Three files carry the version. Move all three together.

1. `Cargo.toml` — raise `version`.
2. `skill/SKILL.md` — the `--tag vX.Y.Z` line in the bootstrap, and the
   `documents benkyou X.Y.x` comment beside it.
3. `README.md` — the `--tag` line and the raw URL in the pinned install.
4. Run `cargo test`.
5. Commit the change.
6. Run `git tag -a vX.Y.Z`.
7. Push the branch and the tag.

Raise the minor number after a change to a command, a flag, or an output field. That is
the contract that the skill describes. A patch is for a change that needs no new wording
in the skill.

The skill on `main` points at the last release and not at `main`. A reader therefore
always lands on a pair that was tested together.
