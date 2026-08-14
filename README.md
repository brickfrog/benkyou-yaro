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
- Isolation, for the four commands that run a script. `bubblewrap` (`bwrap`) is the
  default wherever it works, which is Linux only: it isolates with Linux namespaces.
  `docker` or `podman` is the other way, and the one a mac gets, so macOS runs the whole
  loop and not half of it. `gate`, `attempt`, `grade` and `serve` refuse when there is
  neither. Everything else — the graph, the assessment, the schedule, the orders and the
  cards — is plain file work and needs no backend at all.
- Anki with AnkiConnect, for `cards --push` only
- `uv`, for `benkyou warm` under the sandbox; warming for a container uses the `pip`
  inside the image instead
- The programs that your own exercises call

A grader is a shell script that you write. It runs with no network, so it can use only
what is there before it starts. System Python packages are visible. A virtualenv is not.

If an exercise needs a package the machine does not have, name it in `task.toml` with an
exact version:

```toml
[deps]
python = ["pandas==3.0.5"]
```

Then run `benkyou warm <exercise-dir>` once. That installs the packages, and is one of
the two commands here that use a network — `benkyou runner --pull` is the other. Every
later run reads them from a read-only copy, so grading still runs with no network at all.
`gate`, `attempt`, `grade` and `serve` refuse an exercise whose packages are not warmed
yet, and tell you which ones.

A warmed set is keyed by the runtime that will import it, so warming and gating must
agree on the backend. The host set is keyed by the interpreter's ABI tag; a container set
by the image id, the architecture it resolved to, and that image's own ABI.
`benkyou warm <exercise-dir> --container` warms the container set. Warm one runtime and
gate under the other and the package is plainly installed and still not importable.

Only names with an exact version are accepted, and only pre-built wheels. A bare name
or a version range is refused, so the same declaration always means the same packages.
A URL, a file path or a package that must be built are all refused, because the list comes
from a generated file. Warming for the sandbox runs on your machine with your own rights,
so a build step there is arbitrary code. Warming for a container is confined to the image
and a staging directory, and the rule stays: nobody read the list either way.

`gate`, `attempt`, `grade` and `serve` run generated scripts isolated. There is no
network, no access to your files, and no access to your study state. Missing isolation is
a refusal and never a downgrade: with neither backend present the tool stops and names
both of the things it looked for.

CAUTION: `--unsafe-host` turns isolation off, whichever backend would have provided it.
The scripts then run with your own user permissions, over your whole filesystem. Read a
generated `check/check.sh` and `solution/solve.sh` before you use that flag.

```sh
cargo build --release   # target/release/benkyou
cargo test
```

### Which backend runs your code

Selection is ordered and not negotiated: the sandbox where there is one, a container
where there is not, `--unsafe-host` only when you name it. A mac therefore lands on
`docker` or `podman` without being asked. `--container` asks for a container on a machine
that also has `bubblewrap`, which is how a Linux author gates against the runtime a mac
will use.

The runtime is what separates the two. The sandbox binds your own read-only `/usr`, so a
verdict is silent about which interpreter earned it. A container replaces `/usr` with an
image pinned by digest, which makes that question answerable, so `.gate.json` records the
resolved image id and `attempt`, `grade` and `serve` refuse the exercise under any other
image, naming both and telling you to gate again. A newer engine version only warns.

`--image REF`, or `$BENKYOU_RUNNER_IMAGE`, names a different image. The default carries
python3, `pip` and the usual shell tools and nothing else, so an exercise whose grader
calls `sqlite3` needs one of its own. A run gets that image, no network, no capabilities,
a read-only root, and your own uid — so what it writes into the workspace belongs to you
and not to root.

`benkyou runner` reports the engine, its version, the image and whether the image is
present, and exits non-zero when it is not. `--pull` fetches it, and is the only place
here that fetches an image at all: `gate`, `attempt`, `grade` and `serve` resolve it
locally and refuse when it is absent, so a runtime is never downloaded in the middle of a
verdict.

```sh
benkyou runner --pull            # once, before the first container run
benkyou gate ./exercises/foo     # on a mac, already a container run
```

macOS has a sandbox of its own and this tool does not use it: `sandbox-exec` can bound
neither a process tree nor a scratch filesystem, and it is deprecated. DESIGN.md carries
that argument in full.

### Anki or the browser on another machine

The tool runs where the isolation is — a sandbox on Linux, or a container engine, which is
what a mac uses. Anki and your browser may be somewhere else. One SSH connection carries
both directions.

`cards --push` writes to AnkiConnect, which listens on `127.0.0.1:8765` on the machine
that runs Anki. `--anki-addr HOST:PORT`, or `$BENKYOU_ANKI_ADDR`, names a different one.
The push report prints `anki_addr`, so you can see which collection took the write.

`serve` binds `127.0.0.1` and never anything else. That is the containment, so it stays.
Forward the port instead and open the URL that `serve` printed, token and all.

```sh
# Run this on the machine that has the browser and Anki, not on the runner.
#   -L: your browser        -> `benkyou serve --port 43117 --no-open` on the runner
#   -R: the runner's --push -> your own AnkiConnect
ssh -N -L 43117:127.0.0.1:43117 -R 8765:127.0.0.1:8765 you@runner
```

Keep the goal file, the fluency file and the exercise bank on the machine that executes.
They are one body of state, and a gate verdict is refused on a machine other than the one
that earned it. Do not sync them both ways while the tool is running.

## Use it from a chat agent

Any assistant you already talk to can operate this tool. Nothing in the skill is specific
to one harness: it is one Markdown file that describes a CLI, and the tool holds no key
and calls no model. `skill/SKILL.md` teaches one how. Install both parts.

`~/.claude/skills/benkyou/` below is one harness's skill directory. Put the file wherever
yours keeps them — some install a skill by URL instead, which needs no `curl` at all.

From a checkout:

```sh
cargo install --path .                        # puts `benkyou` on $PATH
mkdir -p ~/.claude/skills/benkyou
cp skill/SKILL.md ~/.claude/skills/benkyou/
```

The skill is one file, and the binary needs no other part of the repo. Both parts must
come from the same generation, so get them as a pair.

Pinned to a release:

```sh
cargo install --git https://github.com/brickfrog/benkyou-yaro --tag v0.5.0
mkdir -p ~/.claude/skills/benkyou
curl -sfL https://raw.githubusercontent.com/brickfrog/benkyou-yaro/v0.5.0/skill/SKILL.md \
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

The binary holds no API key and calls no model. Two commands reach a network: `benkyou
warm` installs an exercise's declared dependencies from a package index, and `benkyou
runner --pull` fetches the runner image. Nothing on the gating or grading path reaches
anything. It holds the state and prints structured work orders. Your agent writes the
content with the model that you already pay for. The agent then writes the result back.

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

## Four things to know first

**An exercise is not real until the gate runs it twice.** The reference solution must
pass. The untouched start state must fail. Without the second run, a grader that always
passes looks correct. Without the first run, an exercise that nobody can solve looks
correct. `benkyou gate` exits non-zero after a rejection, so a caller cannot show you a
bad exercise.

**Every exercise must name a wrong answer.** Write it in `task.toml` as `[[known_bad]]`,
with the mistake it stands for. The gate puts that answer in a fresh workspace and runs
the grader, which must fail it. One is the minimum, and the gate rejects an exercise
with none.

This catches the failure the two runs cannot see. One model writes the concept, the
task, the answer and the tests. If it reads the concept wrongly, all four agree and both
runs pass. Your own wrong answer is the one part that can disagree.

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
benkyou serve ./exercises/foo f84641d236fd --goal ramp
```

The queue is the argument list, in that order: directories, or digests from the bank.
There is still no queue built from a goal. The bank knows which concept each exercise
belongs to, but nothing yet chooses between several exercises for the same concept,
and picking the first would be a policy invented on the spot. You name what you want.

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

A gated exercise is copied into the bank at
`$XDG_DATA_HOME/benkyou/items/<digest>/`. The directory an exercise is authored in is
usually under `/tmp` and does not survive the week, and the graph would then hold a
score with nothing behind it. The bank is what lets you sit down to the same kata
again, and what lets you read the exercise that produced an old result.

A bundle holds the authored files and nothing else, so it hashes to the directory name
it lives under. Beside it, `attestations.jsonl` gains one line per gate run: the whole
verdict, including the runner it was earned under. Old lines are never replaced. A
bundle that passed in March and fails in August has two true records, and the pair is
the useful one.

`benkyou items` lists the bank. Anywhere that takes an exercise directory also takes a
digest, or enough of the front of one to be unambiguous:

```sh
benkyou items --concept pandas_groupby
benkyou attempt f84641d236fd
benkyou serve f84641d236fd
```

A path that exists always wins over a digest, so a directory named in hex still works.
A banked exercise is re-checked before it is shown: edited bytes, a different runner,
or a verdict from a machine this is not all refuse and tell you to gate it again.

There is no database, no daemon, and no sync. You can delete any of it.

## Releasing

The skill and the binary are one unit that ships from two URLs. A release keeps the two
pointed at each other. Four files carry the version. Move all four together.

1. `Cargo.toml` — raise `version`.
2. `Cargo.lock` — the `benkyou` package entry's `version`, which mirrors the manifest.
   A build rewrites it anyway, so a lock left behind is either a dirty tree at tag time
   or an unexplained one-line diff on the next branch that builds.
3. `skill/SKILL.md` — the `--tag vX.Y.Z` line in the bootstrap, and the
   `documents benkyou X.Y.x` comment beside it. Those two are the only version literals
   in the skill, and the prose around them must stay version-free: a third copy is a
   third thing to update, and the copy that goes stale tells an agent to reject the
   binary it was shipped with.
4. `README.md` — the `--tag` line and the raw URL in the pinned install.
5. Run `cargo test`.
6. Run the isolation suites in required mode, and accept the run only if it skipped
   nothing:

   ```sh
   BENKYOU_REQUIRE_SANDBOX=1 BENKYOU_REQUIRE_CONTAINER=1 cargo test -- --nocapture
   ```

   No line of the output may contain `skipping`. Plain `cargo test` lets a machine
   without bubblewrap or without the runner image pass those suites by not running
   them, and a suite that skips reports passes it did not earn. The two variables turn
   a missing prerequisite back into a failure, and `tests/support/mod.rs` is where that
   choice is made. The `ci` workflow proves the same two contracts on every push, in two
   steps rather than one: the runner has a container engine but no bubblewrap, so the
   sandbox half runs inside a privileged container. A Linux box with both installed can
   do it in the one command above, which is why the release step is a local repeat rather
   than a different gate.
7. Commit the change.
8. Run `git tag -a vX.Y.Z`.
9. Push the branch and the tag.

Raise the minor number after a change to a command, a flag, or an output field. That is
the contract that the skill describes. A patch is for a change that needs no new wording
in the skill.

The skill on `main` points at the last release and not at `main`. A reader therefore
always lands on a pair that was tested together.
