---
name: benkyou
description: Drive the benkyou CLI so a learner can learn a new domain quickly — build and repair a prerequisite graph, assess what they already know, then generate flashcards and graded exercises against a real schedule. Use when the user wants to learn a domain systematically, asks for practice exercises or katas, mentions a new job or deadline to prepare for, or names a goal file (goal.json / *.json with `goal`, `nodes`, `edges`). Also use for Anki card generation where review history must survive regeneration.
---

# benkyou

A Rust binary over plain JSON files. **It holds the state and names the work. You write
the content.** It makes no network calls and holds no API key — you are already in a
conversation with a model, so generation happens here, in the conversation, and the
results are written back through the CLI.

Nothing in this skill is optional decoration. Every rule below exists because skipping
it produced a broken artifact.

## The division of labour

| The tool decides | You decide |
|---|---|
| which node is worth working on now | what the card or exercise actually says |
| which prerequisites may be assumed | how to make it discriminate |
| how much of the solution to show | the domain content |
| whether a generated exercise is real | the wording of a probe |

Never guess at the first column. Ask the tool with `benkyou order`.

## Getting the tool

Every command below is the `benkyou` binary. Check it is there before planning anything
around it:

```sh
benkyou --help || cargo install --git https://github.com/brickfrog/benkyou-yaro --tag v0.1.0
benkyou --version                            # this file documents benkyou 0.1.x
```

This file travels on its own, so do not assume a checkout is nearby — `--git` works from
anywhere, and `cargo install --path .` only if you happen to be standing in one. The tag
is deliberate: it installs the binary this document was written against, so the pair is
consistent by construction rather than by anyone noticing it is not.

That still leaves the case where a `benkyou` was already on `$PATH` and no install ran,
which is what the `--version` line is for. Compare only the first two numbers: any
`0.1.z` binary honours the commands described here, because a patch release does not
move the CLI. If it reports `0.2` or later, this file is the stale half — trust the
binary's own `--help`, say the two disagree, and either fetch a newer skill or add
`--force` to the install above to pin the binary back to this one.

`cargo install` puts it on `$PATH` (`~/.cargo/bin`). There is no daemon, no server and
no config file to write — the binary plus a goal file is the whole installation. If the
binary is missing and you cannot build it, say so and stop; do not simulate its output
or hand-maintain the JSON it owns.

## The loop

```sh
benkyou schema                                # the goal-file shape, as a valid example
benkyou goals                                 # what is already being studied
benkyou validate ramp                          # check the graph, always run first
benkyou seed ramp --known a,b --unknown x      # what they already know, in one shot
benkyou ask ramp                               # highest-leverage question
benkyou record ramp <node> pass|partial|fail|skip
benkyou order ramp --kind exercise             # what to generate, and for whom
benkyou gate <dir> --scratch /tmp/x           # prove the exercise is real
benkyou attempt <dir>                         # lay out a workspace for the learner
benkyou grade <dir> --goal ramp                # run their grader, record fluency
benkyou practice ramp <node> 0.6               # a score you judged, where nothing runs
benkyou session ramp                           # what to practise today
```

**Start with `benkyou schema`.** It prints a complete, valid three-node goal file with
one edge of each type. Read the shape off that rather than guessing at it; a missing
field is reported one at a time by the JSON parser, and unknown keys are accepted
silently, so a typo becomes a field that is simply never read.

`validate` first, always — and read what it says rather than the exit code alone. It
applies the mechanical repairs (duplicate ids, dangling edges, nodes under the relevance
floor) and **rewrites the file in place, with no backup**. Nodes with `relevance`
strictly below 0.3 are deleted outright — exactly 0.3 survives — so score honestly
rather than defensively.

A `requires` cycle is the one thing it will not fix. It reports every cycle in full,
leaves **every edge of the cycle** in place, and exits non-zero: which of those edges is
the wrong one is a domain question, and the tool guessing wrong silently rewrites the
curriculum. Remove one edge per reported cycle and run it again.

A cycle does not make the run a no-op. The mechanical repairs still apply and the file
is still rewritten, so a graph carrying both a cycle and a sub-floor node loses the node
on the same run that refuses the cycle. Copy the file before the first `validate` if you
want to be able to compare. A graph that needed heavy repair is a graph to regenerate.

A goal is named, not pathed: `ramp` is `$XDG_DATA_HOME/benkyou/goals/ramp.json` (default
under `~/.local/share`). Run `benkyou goals` first: it reports that directory and creates
it if absent, and lists what is already stored. Write the new graph straight into it. An
argument with a `/` or a `.json` suffix is still used as a literal path instead, which is
how a goal checked into a project repo works. Never invent a directory in `$HOME` for a
learner's files; `attempt` and `grade` derive the same workspace under
`$XDG_STATE_HOME/benkyou/exercises/<concept>/<slug>/work` from the task itself, so pass
`--work` only when the learner asked for somewhere specific.

Practice history lives in a sibling, `<goal>.fluency.json`, written next to the graph in
the same directory. The first `grade` or `practice` creates it; `seed` and the interview
never touch it. The two halves are deliberately separate — the graph is regenerable and
the fluency file is not, so deleting a goal means deleting both, and copying one without
the other silently resets what the learner has drilled.

`benkyou goals` counts two different things and they are not interchangeable.
`practised` is nodes scored directly, by a `grade --goal` or a `practice`. `credited` is
nodes that only ever received propagated `encompasses` credit: zero attempts, review
clock touched, still unproven. `known`/`unknown`/`unresolved` come from the assessment
and sum to `nodes`; they say nothing about whether anything has been drilled. `retired`
counts confidence at or above the retirement ceiling — note that confidence runs `0.0`
to that ceiling (1.5 by default), not to 1.0, so do not read it as a percentage.

`benkyou session` always returns `--size` entries, ordered weakest concept first with no
two neighbours alike. That last guarantee needs two concepts to hold: when only one is
practisable the session is that concept repeated, which is the schedule degrading
honestly rather than shrinking. Read a repeated session as "this is the only thing left
to work on", not as a bug.

## Building the graph

The graph is the product; the cards and exercises are disposable projections of it.

- **Target the real job, not a plausible one.** Ask what the data is actually about,
  what engine, what the day looks like. A graph aimed at an invented target wastes
  every artifact built on it.
- `requires` is a hard prerequisite: `from` is the prerequisite, `to` depends on it.
  Must stay acyclic. `encompasses` means practising `to` credits `from`. `helps` is
  soft ordering only.
- `needs_goals` on an edge is a list of **integer indices** into the `goals` array of
  the edge's `from` node, so a dependency can be on part of a node rather than all of
  it. Indices, not the goal text: a list of strings fails with `invalid type: string
  ..., expected usize`. Empty is fine and normal.
- `kind` decides what artifact a node can carry. `skill` and `tool` are *done* and can
  be exercised. `fact`, `concept` and `context` are *known* and can only be carded.
  `order --kind exercise` refuses the others by name rather than producing a kata that
  grades trivia.
- **`kind` says a node is a performance; `gradable` says a script can mark one.** They
  come apart on exactly the nodes a study tool is tempted to fake. Before writing
  `kind: skill`, answer out loud: what file does the learner produce, and can a script
  compare it to a reference? A spoken performance, a conversation, or free prose you
  intend to read yourself is not exercisable — set `"gradable": false` on it. `order
  --kind exercise` then refuses it by name and points at `practice`, instead of handing
  you a `check.sh` spec for a monologue. It defaults true, so an ungradable node you
  forget to mark is a dead end you walk into later; mark it the moment you notice, and
  never accept an exercise order you cannot gate.
  Without `--node` the scheduler picks, and it passes over an ungradable node to reach
  one it can use. When there is nothing else left it refuses and names the node, because
  that state is what the end of a ramp looks like — the drills are done and all that
  remains is the performance you still owe. It means go and `practice` that node; it
  does not mean the learner is finished, and `benkyou session` will still return it.
- Ground domain nodes in real sources before writing them. Inventing plausible-sounding
  domain structure is the single easiest way to waste the user's time. The file gives
  you almost nowhere to say where a node came from: `provenance` records only `llm`,
  `user` or `job_desc`, and **`reason` is an edge field — nodes do not have one.** An
  unknown key written onto a node parses without complaint and is then dropped by the
  first `validate`, in place and unreported, so a citation put there is gone and you
  will not be told. Put node grounding in the `probe` and `goals` text, or keep it
  outside the goal file.
- Give every node its `goals`. It is optional to the parser and load-bearing everywhere
  else: `ask` and `order` hand it straight to whoever writes the artifact, so a node
  without goals produces a vaguer order and a weaker card.

## Assessing

Prefer `seed` over interviewing. An interview that asks about `python_idioms` when the
learner has written Python for years is spending their attention on nothing. Declare the
background, then ask only what discriminates.

- **The two directions are not symmetric, and this matters.** `--known X` only sets a
  strong prior. Nothing enters `known` without a graded probe, because claiming to know
  is a prior and never evidence — "I know SQL" and "I can write a `GROUP BY` from a blank
  editor" are different claims and only the second one counts. Priming does not skip the
  question; it makes the tool ask it sooner, since a pass on a deep claimed node resolves
  everything beneath it in one answer.
- `--unknown X` does resolve outright, taking everything that depends on `X`. Nobody
  claims not to know a thing they can do, and the costs are lopsided: a wrong `unknown`
  wastes a little time out loud, a wrong `known` silently skips material they needed.
- Conflicts between the two lists are reported, not silently resolved.
- **Read `retracted` in the output.** `--unknown` is the one thing that can overrule a
  graded probe: `known` is prerequisite-closed, so admitting a prerequisite forces every
  node above it back out, exactly as a graded `fail` there would. Those nodes are listed
  rather than dropped quietly. A careless `--unknown` can throw away a session of earned
  evidence, so check the list before moving on.
- Neither spends the question budget. Answers do.
- **`skip` grades the question, not the learner.** Use it the moment a probe is leading,
  ambiguous, or answerable from its own phrasing. Then `order --kind probe` to rewrite
  it. A probe like *"Why is 'we are 95% sure' wrong?"* contains its own answer and
  measures nothing — that is a `skip`, not a `fail`.
- **Read `outcome` on every `record`.** `applied` means the verdict landed. `reask`
  means it was **rejected and nothing changed** — the tool judged it a careless error
  because it contradicts evidence already on file, most often a `fail` that would drag
  several `known` nodes back out with it. Re-probe with a different instance and record
  again, or that verdict is simply lost. Nothing else in the output marks the
  difference, so a `fail` you believe you recorded can quietly not exist.
- `partial` is for "knows it, cannot do it under time pressure". That is the common case
  for a rusty practitioner and it is not a failure. **It credits the prerequisites, not
  the node:** everything the node requires moves into `known`, while the node itself
  stays out at a coin flip so the plan keeps it as cheap review. A `partial` on a node
  already resolved `known` withdraws that resolution, and takes its known descendants
  with it, because half of it is not all of it. If they cannot produce half, that is
  `fail`.
  That review state is not permanent, and the reason is the closure doing its job: a
  later `pass` on anything that *requires* the node is evidence for the node, so it is
  pulled into `known` at full belief and the review quietly disappears. Probe the
  shallow thing before the deep one if you want the review to survive the session.
- **`practice <goal> <node> <score>` is the way in for anything no grader can run.** You
  assign the score, and it propagates along `encompasses` edges exactly as `grade` does,
  so a conversation drill or a whiteboard session still advances the schedule. Be exact
  about what propagates, though: the credited node gets **half the score as confidence
  and a touched review clock**, not a score. Its `attempts` stays 0 and its `best_score`
  stays 0.0 until it is worked directly, which is why `benkyou goals` reports those
  nodes as `credited` and never as `practised`. Credit moves the schedule; it does not
  discharge the node. Edge `strength` is not consulted — the rate is per hop, halving
  again at each one.
- `practice` will score **any** node, including one a grader could have judged; the
  guardrail lives on `order`. It flags that case in a `warning` field rather than
  refusing, because a kata done on paper is a real attempt — but if the node is
  `gradable`, write the kata and let it do the scoring.

## Filling a cards order

Note identity is `concept_id + role`, **never the card text**. Regenerating a card
updates the existing note in place and its Anki review history survives. This is the one
property most card generators get wrong.

- One card per role, maximum: `definition`, `application`, `contrast`, `cloze`.
- The front must be answerable in seconds. A front needing a paragraph is an exercise.
- Never put the answer in the question.
- `cloze` uses `{{c1::...}}`; each span becomes its own card from one note.
- Default is a dry run. `--push` writes to a collection the user may have curated for
  years — show them the dry run first unless they have already said go.

## Filling an exercise order

An exercise directory is `task.toml`, `instruction.md`, `setup/`, `solution/solve.sh`,
`check/`. `setup/` is copied to the learner. `check/` never is. Transcribe the order's
`write.task_toml` into `task.toml` exactly as it is nested — that object mirrors the file
one for one.

`write.path` is a suggestion, relative to wherever the exercise library lives. If you are
not working inside a checkout, put the directory anywhere and pass `gate`, `attempt` and
`grade` an absolute path. **Gate the copy the learner will actually sit**: `attempt`
refuses a directory with no `[gate]` block, so an exercise gated only in `/tmp` can never
be attempted. The copy-to-`/tmp` rule below is for the throwaway discrimination runs, and
for shared fixtures and templates that must not acquire a `[gate]` block of their own.

**The gate is necessary and nowhere near sufficient.** `benkyou gate` proves only that
the reference passes and an empty stub fails. That bar is low enough to be met by an
exercise that grades nothing.

Before submitting, run the discrimination check by hand:

1. Copy the exercise to `/tmp`, replace `solution/solve.sh` with a **plausible wrong**
   answer — the mistake the learner would actually make — and gate it. It must be
   `Rejected`. Do at least two.
2. Replace it with a **differently written correct** answer. It must be `Validated`.

If a wrong answer passes, the exercise is decoration. Change the hidden data or tighten
the required output until it discriminates. This is where the value is: a kata that
rejects an empty file and accepts everything else teaches nothing.

Other rules that cost real debugging:

- **Grading data in `check/` must differ from the sample in `setup/`.** Grading on the
  data you showed rewards hardcoding. The rule assumes the exercise grades a
  transformation over data. In a recall domain — conjugation, declension, vocabulary —
  reproducing the specific forms *is* the skill and there is nothing to vary: keep the
  format example in `setup/` on material that is never graded, put the graded items only
  in `check/`, and say so in `instruction.md`. Do not vary the graded items themselves;
  that makes the exercise unwinnable.
- **Portable SQL in references.** `COUNT(CASE WHEN ... THEN ... END)`, not
  `COUNT(*) FILTER (WHERE ...)` — SQL Server has no `FILTER`. Write what the learner
  will type at work, not what is shortest.
- **Pin the output exactly** in `instruction.md`: columns, order, and what an empty
  group should produce. An ordering the grader checks but the instruction never states
  is a trap, not a lesson.
- **`check.sh`'s exit code reports grader health, not the grade.** Non-zero means the
  grader itself broke. The score goes in `out/reward.json`.
- **Scrub every string you interpolate into `out/reward.json` with `tr '[:cntrl:]' ' '`.**
  `sqlite3 -csv` emits CRLF and `diff` output carries tabs; a single raw tab makes the
  file unparseable, and an unparseable `reward.json` is read as grader breakage — which
  discards the attempt rather than scoring it, so a wrong answer costs the learner even
  the record of having tried. Squeezing newlines alone is not enough. Use `[:cntrl:]`,
  never `-c '[:print:]'`, which mangles UTF-8; apply any length cap last.
  You can see when this bites: the verdict comes back `CheckBroken` with `practice:
  null` and no row in the fluency file. **A `CheckBroken` is always your bug, never the
  learner's** — fix `check.sh` and re-grade; never report it as the learner failing.
- **Exit status alone cannot tell a low score from a broken grader.** `gate` exits
  non-zero when the exercise is rejected, and `grade` exits non-zero whenever the
  attempt fails, both printing their full JSON on stdout first. Branch on the JSON
  `verdict` — `Pass`, `Fail`, `CheckBroken` — not on the exit code.
- **`--work DIR` names the run directory, not the workspace.** `attempt` and `grade`
  both use `DIR/work`; pointing either at the workspace itself gives
  `<dir>/work: no workspace here yet`. The run directory is cleaned up afterwards, so
  `out/reward.json` is not left around to inspect.
- **Python graders run under `uv run --no-project`, never bare `python3`.** Declare
  the interpreter and every dependency in a PEP 723 header at the top of the script:

  ```python
  # /// script
  # requires-python = ">=3.11"
  # dependencies = ["pandas>=2"]
  # ///
  ```

  A missing library is then a resolution step, not a reason to rewrite the exercise.
  Never water a kata down to what happens to be installed — a `pandas_groupby` node
  graded on hand-rolled `collections` code records fluency the learner did not earn.
- **The verdict is the last line of output, not the exit code.** `uv` exits 1 when it
  cannot build the environment, which is the same code a wrong answer uses; keying on
  it scores a correct solution 0.0 on an offline box. Print `VERDICT pass <detail>` or
  `VERDICT fail <detail>` as the final line, collapsed to one line, and have `check.sh`
  read only `tail -n 1`. Matching anywhere in the output lets a solution that merely
  prints `VERDICT pass` grade itself.
- **A solution that will not import, or returns junk, is a wrong answer.** Wrap the
  module load and keep every comparison total — no bare `float(x)` on learner output.
  A grader that crashes reports itself broken and the attempt is discarded, so a
  learner's typo silently costs them the record of having tried.
- Ties in the expected output must be broken by a stated rule, or the exercise is
  nondeterministic and will fail correct answers at random.
- Never gate a shared fixture or a template in place — gating writes a `[gate]` block
  into `task.toml`. Copy to `/tmp` and gate the copy.

## Judging free-text answers

When you grade an answer the same model generated, you are the biased party:
self-preference in rubric-based evaluation is well documented, and it survives even
programmatically verifiable rubrics. So: **mechanical checks decide pass or fail; your
prose is advisory and prints after the verdict.** Never let a judgement you authored be
the thing that gates progress.

## What does not exist

Be straight with the user about this rather than implying otherwise:

- No graph generator. You write the graph.
- No card or exercise generator. `order` tells you what to write; you write it.
- No UI, no visualiser, no web app.
- Unix only — the runner uses `/bin/sh` and process groups. It will not run on Windows.
- No schema validation beyond serde. Unknown keys are accepted silently, so a misspelled
  field is not an error — it is a field that is never read. Start from `benkyou schema`.
- No way to cite a source. `provenance` is only `llm`, `user` or `job_desc`.
- Nothing *infers* whether a `skill` node is machine-gradable. You declare it with
  `"gradable": false` and the tool honours that; unmarked, it assumes an exercise is
  possible.
- No sandbox. During grading the check scripts sit beside `work/` and the verify command
  runs in that directory, so a solution that goes looking can read its own grader — and
  a Python grader imports the learner's module into its own process. `attempt` keeps
  `check/` out of the learner's workspace, which stops you stumbling onto the answer; it
  is not a defence against deciding to find it. Whoever writes the solution should not
  read `check/`.
- No backups. `validate` rewrites the goal file in place.
