# benkyou-yaro

A tool for ramping into a new domain fast. You give it a target and what you already
know. It builds a disposable prerequisite graph, interviews you until it knows which
parts you can skip, and then produces two kinds of practice from what's left: cards for
the facts, graded exercises for the skills.

It is invoked by a coding agent you are already talking to. It holds the state; the
agent does the generating.

---

## The thesis

A flashcard trains the retrieval that skill acquisition is supposed to *delete*.

In ACT-R terms (Anderson 1982; Taatgen & Anderson 2002) a skill starts as declarative
facts interpreted step by step through working memory, and becomes production rules
through *proceduralization* — two rules merging into one, with the declarative retrieval
compiled away. Cards and drills are therefore not two flavours of the same thing. The
card is scaffolding you are trying to demolish.

So the tool has separate tracks with separate schedulers, and the interesting engineering
is in the bridge between them.

---

## Three tracks

| Track | Unit | Scheduler | Status |
|---|---|---|---|
| Declarative | fact / card | FSRS | v1 |
| Procedural | graded task, minutes to an hour | mastery gating + interleaved sessions | v1 |
| Tool fluency | keystroke, CLI flag, seconds | latency threshold, retire when fluent | later |

**FSRS is used for cards and not for exercises**, and the reason is that the two tracks
schedule different objects. FSRS schedules an *item* for re-presentation: it predicts the
probability of recalling that item at time t. An exercise is consumed by being solved.
Attempt it a second time and you are testing recall of your own solution, not the skill.
The procedural unit that survives repetition is the *concept*, practised with a fresh item
each time, so there is no item-level interval for FSRS to compute.

The input channel does not fit either. FSRS consumes `(rating ∈ {1,2,3,4}, delta_t)`. A
graded attempt produces a mechanical correctness in 0.0-1.0 plus the assistance the
learner leaned on. Compressing that into a four-point subjective recall scale invents
information nobody measured.

Data density is a real but *secondary* problem, and it is worth stating correctly because
an earlier draft of this document got it wrong. The optimizer does not need N reviews of
one item: parameters are fit across the whole review history of a preset, and Anki's
health check flags a pool under "a few hundred" reviews. At one exercise a day a
procedural pool reaches that in about a year. That is an argument about when tuned
parameters beat the defaults, not about whether the model fits the task.

Separately, SM-2 and FSRS are both fit to binary recall of short verbal items at
sub-ten-second latencies; no published work validates either on a twenty-minute task.

What the evidence does support for skills is *session-level* spacing. Spruit et al. 2014
(RCT, laparoscopic simulator) found 1×75min/week × 3 beat 3×75min in one day at
acquisition, two-week retention, and one-year retention. But Cecilio-Fernandes et al.
2017 (systematic review) concludes "the optimal gap between re-study sessions is
unclear", and Gonzalez et al. 2011 found weekly versus monthly made no statistical
difference. The granularity an SRS agonizes over is not where the effect lives.

The lever that *is* well supported is the composition of the session: interleaved across
concepts rather than blocked by topic (Rohrer & Taylor 2007; Rohrer et al. 2020 RCT).

So the procedural scheduler is four numbers per concept and a sort. See §5.

---

## 1. The graph is disposable; state is not

Every hand-authored public concept graph has died of curation cost: Metacademy (last
substantive commit 2019, content repo self-declared deprecated, site down), Learney
(company dissolved 2025-07-22), Orbit (dormant since 2024-10). The one living
counterexample, Math Academy, spent a decade and a funded team on K-12 maths.

Therefore: the graph is **per-goal, LLM-generated, regenerated in seconds, and thrown
away**. It is a plain file in the repo. The only durable artifact is learner state.

Expect roughly a quarter to a third of generated edges to be wrong — supervised
prerequisite mining tops out near F1 0.74 in the literature and an LLM lands in the same
band. Design for a wrong graph rather than against it:

- every edge carries a one-line `reason` a human can reject at a glance
- `helps` is the default edge type, where being wrong costs nothing
- `requires` — the only type that blocks — must be justified

### Node identity

**A node is defined by the probe that proves it, never by a noun.** "Understand
transformers" and "know what a residual connection is" are not the same kind of object,
and unstable granularity is the most common way these graphs rot. ALEKS avoids it by
building its domain out of *problem types* rather than concepts; we copy that.

### Edge types

- `requires` — hard prerequisite. Defines the closure and the fringes. Must be acyclic.
- `helps` — soft. Affects ordering and priority, never blocks.
- `encompasses` — mastery of the target grants practice credit for the source.

`encompasses` is the bridge between the two halves. It lets the scheduler serve the
smallest set of tasks covering everything due, so **one exercise can retire six cards**.
Without it the procedural track is a parallel chore competing with the card queue; with
it, the exercise is how you discharge the queue.

### File

One file per goal, plain and diffable:

```jsonc
{
  "goal": { "id": "...", "target": "...", "deadline": "...", "budget_hours": 40 },

  "nodes": [{
    "id": "partial_pooling",              // stable slug, [a-z0-9_]
    "title": "Partial pooling",
    "kind": "fact | concept | skill | tool | context",
    "probe": "...",                       // the question that proves it. NOT a noun.
    "goals": ["...", "..."],              // specific learning objectives
    "cost_minutes": 90,
    "relevance": 0.9,                     // to THIS goal; prune below threshold
    "provenance": "llm | user | job_desc"
  }],

  "edges": [{
    "from": "...", "to": "...",
    "type": "requires | helps | encompasses",
    "strength": 1.0,
    "reason": "one line, human-rejectable",
    "needs_goals": [0],                   // only goal 0 of the prereq is required
    "provenance": "llm", "confidence": 0.8
  }],

  "state": {                              // the only thing that is backed up
    "known": ["..."],                     // downward-closed under `requires`
    "unknown": ["..."],
    "belief": { "node_id": 0.35 },        // p(mastered) for unprobed nodes
    "evidence": [{ "node": "...", "probe": "...", "verdict": "pass|partial|fail",
                   "at": "...", "source": "interview|exercise" }]
  }
}
```

`needs_goals` is borrowed from Metacademy's `source_goals`: an edge can require *part*
of a prerequisite rather than all of it. It is the correct fix for over-pruning and
almost nobody implements it.

### Invariants the core enforces

- the `requires` subgraph must be acyclic — LLMs emit cycles, and the core refuses them
  rather than repairing them: nothing in an edge distinguishes a hallucinated
  prerequisite from a sound one, so cutting by `confidence` just deletes whichever
  correct edge was described most modestly
- `known` is downward-closed under `requires` — marking a node known marks its
  `requires`-ancestors known, and this closure is where the pruning leverage comes from
- `outer_fringe(known) = { n ∉ known : every requires-pred ∈ known }`
- `inner_fringe(known) = { n ∈ known : known \ {n} is still downward-closed }`
- plan = topological order over `reachable(goal) \ closure(known)`, cheapest first within
  a level, truncated at the hour budget

---

## 2. Assessment is the product

No assessment means no pruning means four hundred nodes of homework. Metacademy asked
you to tick boxes by hand; nobody ticks four hundred boxes.

ALEKS is the only rigorous deployed solution and its mechanics are published (Falmagne,
Doignon, Cosyn & Thiéry). Model the learner as a downward-closed set of mastered nodes,
and report only the two fringes — they uniquely determine the state. Roughly 9–11 items
describe an 80-item state; a 24-question assessment resolved one state out of 57,147.

We cannot enumerate states, so we replace "probability mass over feasible states" with
closure leverage, which is computable in microseconds on a graph this size:

```
gain(n) = p[n]      · |({n} + requires-ancestors(n))   \ known|    // PASS collapses these
        + (1 - p[n]) · |({n} + requires-descendants(n)) \ unknown|  // FAIL collapses these

ask argmax gain(n)
  ties → max worst-case resolution, i.e. max min(ancestors, descendants) still open
  ties → p[n] closest to 0.5
  ties → node id
```

`n` counts in both terms because answering always resolves at least itself, so an
isolated node is worth exactly 1.0 rather than 0.0.

The worst-case tie-break earns its place. On a uniform chain every node has the *same*
expected gain — what an answer wins in one direction it loses in the other — so the
tie-break decides everything, and breaking on node id picks an arbitrary endpoint whose
PASS resolves nothing but itself. The interview then walks the chain one node at a time
and the closure leverage never materialises. Preferring the most balanced node maximises
what is resolved *whichever way the answer goes*. Measured on a four-node chain, it
finishes in three questions instead of four; the gap widens with depth.

Breaking the remaining tie toward 0.5 is ALEKS's own rule: a coin-flip node is where
information gain is maximal. In practice a decent prior separates the gains and none of
this fires.

Loop:

1. **Prior.** One batch call assigns every node `p(known)` from the user's background and
   the target description. A good prior is the difference between eight questions and
   thirty.
2. **Ask.** Emit `node.probe` verbatim as free response. Never multiple choice — ALEKS is
   open-response specifically to kill lucky guesses, and a self-assessing user
   over-recognizes. **"I don't know" is a first-class answer** and is the cheapest to act
   on; ALEKS notes it shortens the assessment.
3. **Grade** against `node.goals`, three verdicts:
   - `PASS` → `known ∪= {n} ∪ requires-ancestors(n)`
   - `PARTIAL` → ancestors only; keep `n` in the plan as cheap review
   - `FAIL` → `unknown ∪= {n} ∪ requires-descendants(n)`
4. **Stop** when max gain < 2 nodes, or at ~30 questions, or when what remains fits the
   budget anyway.

Report the fringes, never the full state: inner fringe as a sanity check, outer fringe as
the plan.

**Self-report is a prior, never evidence.** Every entry in `known` must trace to a graded
probe or to closure from one. People are wrong about what they know; that is the entire
reason ALEKS exists. Log both so the overconfidence rate is visible after week one.

The two directions of self-report are therefore handled differently, and the asymmetry
is deliberate. A claim of knowledge sets a strong belief and nothing more, which does
not skip the question — under closure leverage it makes the question *more* worth asking,
because a pass on a deep claimed node discharges its whole ancestor cone in one answer.
An admission of ignorance resolves immediately and takes its dependents with it: nobody
claims not to know a thing they can do, and the error costs are lopsided. Wrongly marking
something unknown wastes a little time out loud; wrongly marking it known silently skips
material the learner needed, and silence is the failure mode worth engineering against.

A `FAIL` that contradicts two passes on `requires`-descendants is re-asked once with a
different instance before being accepted — ALEKS's careless-error handling, cheaply.

---

## 3. Exercises

One exercise is one directory. The shape is Exercism's, trimmed.

```
exercises/<concept_id>/<slug>/
  task.toml          metadata, limits, verification contract
  instruction.md     what the learner reads; names exact output paths
  setup/             copied to the workspace before the learner starts
  solution/solve.sh  REQUIRED — the reference, used by the gate
  check/check.sh     REQUIRED — the grader; never copied into the learner's workspace
```

Non-negotiables copied from `exercism/problem-specifications`:

- **every gradable check carries a v4 UUID, and checks are immutable.** A fix is a *new*
  check carrying `reimplements: <uuid>`. This is what gives stable identity across
  regenerations — when the model rewrites an exercise, UUIDs say what actually changed.
- `practices[]` and `prerequisites[]` are the edges back into the graph
- curriculum selection lives in a separate file keyed by UUID, so pruning never mutates
  content

One field Exercism lacks: **`guidance_level ∈ {worked, faded, blank}`**. Worked examples
help novices and measurably *harm* knowledgeable learners (expertise reversal — Kalyuga,
Ayres, Chandler & Sweller 2003). Since the prune step already establishes expertise per
concept, showing a worked example to someone who knows the concept is not neutral, it is
worse than nothing. Fade backward: drop the last solution step first, then the last two.

### Verification contract

`check.sh` runs in the run directory, with the learner's workspace beside it at `work/`,
itself at `check/`, and an output directory at `out/`. It writes a reward file there:

```json
{ "correctness": 0.0, "<dim>": 0.0, "detail": "one line per failed criterion" }
```

**Its exit code reports grader health, not the grade** — Exercism's rule, and worth
copying exactly. Zero means "graded"; non-zero means the grader itself broke and the
harness reports `CHECK BROKEN`, never `FAIL`. Conflating the two is the classic source of
unreadable grading failures.

Pass means every dimension listed in `must_pass` is 1.0. `detail` is the learner-facing
feedback, and for differential tasks it carries the shrunk counterexample — "wrong on `[]`
and on duplicate keys" teaches; "3/7 failed" does not.

### The validation gate

**Non-negotiable, runs at generation time.** A generated exercise is not shown until it
has been run 2 + N times:

1. apply `solution/solve.sh` → correctness must be 1.0, else discard
2. untouched `setup/` → correctness must be < 1.0, else discard as vacuous
3. each `[[known_bad]]` candidate → must fail, and must not break the grader

Without the first two you are shipping prose with a test file next to it. For debugging
tasks, additionally record the checks that must *stay* green, so the exercise cannot be
passed by deleting things.

The third direction answers a question the first two cannot. They compare artifacts
written by the same model in the same sitting: the concept, the instruction, the
reference and the checks all share an author, so a misreading is common to all of them
and both directions pass happily. Reading it back does not help — the reader is the
writer. Sampling N generations does not help either, because the error is a property of
the model rather than of the sample; and asking the model to judge its own output is
worse than useless, since self-preference in rubric-based evaluation survives even
programmatically verifiable rubrics.

A named wrong answer breaks the symmetry by making the author commit to a prediction
the machine can test: *this specific answer must fail, for this reason*. Either it does
and the grader discriminates on something, or it does not and the contradiction is
arithmetic rather than editorial.

Two details are load-bearing. A candidate is **static file content**, never a command:
an executable `apply` step would be one more generated script inside the execution
boundary, and a candidate that can compute is a candidate that can read `check/` on the
way past. And a candidate that makes the grader *crash* is not counted as caught — a
grader that cannot parse anything would otherwise score full marks against a whole
suite of traps.

What this does not buy, stated because it is tempting to think otherwise: it is a
mutation test for the grader, not evidence the exercise teaches the concept. A model
wrong about the concept can be consistently wrong across the reference, the checks and
its own candidates. It also cannot tell whether a candidate failed for the reason named
or for an unrelated one — a syntax error counts as a catch. Both are why `trap` is prose
for a human and not a field the tool matches on.

The rule is enforced where it can be bypassed, not where it is convenient: `attempt`,
`grade` and `serve` all refuse an exercise the gate has not validated. Grading is the
stricter of the three doors — a grader nobody proved discriminating would still write a
score into the learner's fluency, where it decides what they see next.

A verdict has to be bound to what it was a verdict *about*, or it outlives its subject:
edit a hidden case after gating and the exercise stays showable on the strength of a run
that no longer describes it. So `.gate.json` carries a SHA-256 over `task.toml`,
`instruction.md`, `setup/`, `check/` and `solution/`, and every door re-derives it. The
digest is taken before the runs and again after; disagreement rejects the exercise
rather than certifying bytes that were never executed.

It is a sidecar rather than a `[gate]` block inside `task.toml` for one reason that
decides the rest: a tool that writes into the file it is hashing cannot hash it exactly,
and would have to argue about which differences count. Nothing writes to the authored
files, so they are hashed byte for byte — comments, formatting, and sections this binary
does not yet parse included. Gating became non-destructive as a side effect, which also
ended a real bug: reserialising `task.toml` had been silently deleting any table the
`Task` struct did not declare.

What the digest cannot see is the environment. An interpreter or a package that moved
underneath an exercise changes its behaviour without changing a byte of it, and that
closure is not enumerable from the directory — so the record keeps a fingerprint
(binary version, OS, architecture) as *evidence*, warns when it differs, and never gates
on it. Real environment drift surfaces as a failing grade. This is a gap, not a
guarantee.

### Doing one

`attempt` copies `setup/` into a workspace and stops. `grade` runs the exercise's own
grader over whatever is in there and, given a goal file, records the score as practice
fluency for the task's `concept_id`. Two commands, because the learner does the work
in between.

`verify.hidden` decides where the grading happens. When set, it runs in a throwaway
directory that is deleted before the command returns: a checker copied next to the
learner's answer hands over the reference on the first grade, which is the whole
failure the hidden cases exist to prevent. When clear, the run is left beside the
workspace, because picking over it is then the point. Either way `work/` is the one
directory never written to.

### Grading, by kind

| Kind | Mechanism | Reliable |
|---|---|---|
| kata | hidden tests + differential rounds against the reference | yes |
| debug | pinned repro flips red→green, guard set stays green | yes |
| sql | run learner and reference against one fixture, compare result sets | yes |
| terminal | assert observable state, one script, two-token output | yes |
| artifact | compare produced file to reference: schema, dtypes, values with tolerance | yes, if the spec is exact |
| "which approach is right, and why" | judge | **no** |
| style, explanation, writeup | judge | **no** |

**Hard rule: if verification cannot be expressed as a command whose exit status or
artifact comparison decides the outcome, it is not an exercise.** It is a card or a
reflection prompt. Refusing to generate it is a feature.

Parameterize inputs where possible, so a repeated exercise cannot be passed from memory.

### The judge

Mechanical checks decide pass/fail. The judge writes advisory prose *after* the verdict
and is structurally incapable of changing it.

This is not fastidiousness. Self-preference bias survives even programmatically
verifiable binary rubrics: on criteria the generator actually failed, judges were more
than 50% likelier to mark them satisfied when the output was their own (Pombal, Rei &
Martins 2026; the position/verbosity/self-enhancement biases are from Zheng et al. 2023).
One model generating the exercise, writing the reference, *and* grading the attempt is
the worst case for this bias, and it is also the default design.

Two determinism rules, learned from other people's flaky graders: pin the seed and the
image digest, and grade floats with an explicit tolerance. A grader that flakes twice
teaches you to distrust the tool, and then the tool dies.

### The bank

An exercise directory is caller-owned and usually in `/tmp`. That followed from
treating an exercise as consumed by being solved: the graph recorded that you practised
a concept, so the material could go. The consequence was only visible from the outside.
The system remembered the practice and threw away the instrument — you could see that
you scored 1.0 on `python_sets_and_order` four times, and could not see, redo, or
audit a single one of the exercises that produced those numbers.

A gate now banks what it validated, under
`$XDG_DATA_HOME/benkyou/items/<digest>/`. The key already existed: §3 hashes the
authored bytes so a verdict can be bound to them, and that hash is a perfectly good
name.

**A bundle is the authored files and nothing else**, so it re-hashes to the directory
it lives in — checked on the way in, because a content-addressed store whose key does
not describe its contents is worse than no store at all. It notably excludes
`.gate.json`. Copying the whole directory was the obvious implementation and it swept
that file in, which would have made one machine's verdict travel inside the exercise as
though it were a property of it — and `attempt` trusts that sidecar, so a bundle would
have been showable on a machine it had never run on.

**Verdicts live beside the bundle, not inside it.** `attestations.jsonl` gains one line
per gate run, and the line is the whole `Gate` record. A tidier `{ at, env, backend }`
summary was tried first and cannot work: deciding whether a banked exercise is showable
here runs through `Runner::stale`, which needs the runner the gate actually used, and a
string like `"sandbox"` has already thrown that away. Old lines are never replaced —
a bundle that passed in March and fails in August has two true records, and the pair is
the useful one. Appended with `O_APPEND` rather than read-modify-written, so two
concurrent gates cannot lose one another's report.

Reuse re-checks rather than trusting the name. `read_gate` falls back to the newest
attestation when there is no sidecar, and every existing refusal then applies unchanged:
edited bytes, a stale runner, a gate that rejected. Verified by editing a banked
`instruction.md` and by rolling back a recorded `semantics` — both refuse and name the
fix.

What is deliberately absent is any notion of *which* banked exercise to serve. That is
a learning decision rather than a lookup, and it is not made here.

---

## 4. Cards

Anki does one thing better than anything else: it is a durable, synced, mobile,
FSRS-tuned queue. It gets to do exactly that and nothing more.

Its limits are structural rather than missing features. Answer time is capped at 60
seconds *by default* and the cap is adjustable, so the honest objection is not that Anki
cannot measure a long task — it is that duration is never an input to scheduling. FSRS
consumes `(rating, delta_t)`; recorded seconds exist for the statistics screen and
nothing else. Beyond that: grading is four self-report buttons with no channel for
"passed 7 of 9 assertions"; the per-card side channel is under 100 bytes with keys of 8
bytes or less; and there is no way to express "don't show B until A is mature". Anki has
gather, sort and display-order controls, but none of them is a prerequisite gate.
Ordering lives in our graph; Anki receives the result.

**Note GUIDs derive from `concept_id + card_role`, not from content.** Content-hashed
GUIDs mean that editing one concept returns every affected note as a *new* note and
throws away its review history. A tool that re-projects a persistent graph hits this on
the second run.

Card content is code-heavy, so field values are sanitized with an allowlist
(`pre`, `code`, `span[class]`) rather than escaped wholesale — escaping every field
renders code blocks as literal markup and silently defeats the styling.

---

## 5. Scheduling the procedural track

Not an SRS in the FSRS sense — nothing here schedules an *item* for re-presentation.
The model is mastery gating, taken from keybr, a tool whose whole value proposition is
adaptive practice and which contains no scheduler at all.

The state per concept is four numbers: `{ best_score, last_practiced, attempts,
mastery }`. The load-bearing distinction is between the last two:

- **`mastery` is evidence.** It accumulates from graded attempts and is never reduced
  by elapsed time.
- **Due-ness is a question about the calendar**, answered separately by `due_in`:
  `review_after_days * (mastery / target)` days after the last attempt. More evidence
  buys a longer interval.

Collapsing those two was the original error, and it is worth recording because the
code read plausibly for months. One `confidence` field stood for historical mastery,
current retention, prerequisite readiness, ordering priority and retirement at once,
and the value was decayed exponentially before every read. Three consequences, all
verified against the shipped binary before being fixed:

- **A one-day gate flicker.** Admission compared decayed confidence against a target
  of exactly 1.0, and one perfect pass landed on exactly 1.0. A day later the
  prerequisite sat at `1.0 × 0.5^(1/21) = 0.9675` and every dependent closed. Nobody
  forgets a skill in 24 hours; the schedule simply could not represent "proven, and a
  little stale".
- **A lapse cliff.** From a confidence of 0.99, scoring 0.50 gave 1.49 and scoring
  0.49 gave 0.49 — a hundredth of a point deciding a whole point of evidence, on
  generated exercises of uneven difficulty.
- **Mastery with no attempt.** Two `encompasses` hops could carry a node to target
  with `attempts` still at zero, and that node then unlocked its dependents on
  evidence no grader ever produced.

So the rules now:

- **Admission is monotonic.** A node unlocks when every `requires`-predecessor has
  undecayed `mastery ≥ target` *and* at least one direct attempt. Once open, it stays
  open until a direct failure says otherwise. An `encompasses` edge is an assertion
  about the graph, not a check that ran, so a node carried to target by credit alone
  stays schedulable and unlocks nothing until someone sits down to it.
- **Time raises priority, never lowers evidence.** A concept past its interval becomes
  `DueForCheck`, most overdue first.
- **The ceiling is not retirement.** `mastery_ceiling` caps how much evidence one
  concept can bank, and evidence buys interval, so a finished concept returns rarely
  rather than never. Heathcote, Brown & Mewhort (2000) show individual acquisition
  curves fit exponentials with a hard asymptote rather than power laws — that is a
  claim about how quickly improvement stops, not a promise that performance never
  decays, and it cannot license never checking again. The earlier text leaned on it
  for exactly that, which was more than the paper says.
- **Lapses are proportional.** A direct attempt below `lapse_at` keeps
  `mastery × (score / lapse_at)`: total failure erases the balance, a near miss costs
  almost nothing, and the two meet continuously at the threshold.
- **Only a direct attempt can demote.** Credit arriving over an `encompasses` edge is
  a verdict on the node that was attempted, and must never cost its neighbours what
  they earned.

What to practise next is an ordered `Reason` — `BelowTarget`, then `Unproven`, then
`DueForCheck` — rather than a scalar that happens to sort. Unfinished work outranks an
unproven claim, which outranks a routine re-check. Stating the order as a policy is
honest about the fact that these three are not commensurable quantities.

Session composition is interleaved across concepts, never blocked by topic — the
guarantee being that no two adjacent entries share a concept whenever two or more are
practisable. It has to degrade rather than fail when they do not: a session is always
`session_size` entries, so with a single practisable concept it is that concept
repeated. Interleaving is a property of the ordering, not a promise of variety the
schedule cannot always keep.

None of these constants are calibrated. `target`, `review_after_days`,
`mastery_ceiling` and `lapse_at` are policy, chosen to be defensible rather than
fitted, because a single-user tool has no outcome data to fit them to. The structure
is meant to be correct — evidence separate from staleness, an ordering that is stated
rather than emergent — and the numbers are meant to be replaceable.

---

## 6. Shape of the tool

A single Rust binary over plain JSON files, plus a skill that teaches an agent to drive
it. It **holds no API key and calls no model**. Exactly one command reaches a network - `benkyou warm`, which installs declared packages from an index so that gating and
grading never have to - and the only loopback traffic is
to AnkiConnect.

State is two files per goal: `<goal>.json` holds the graph with the learner state
embedded, and `<goal>.fluency.json` holds practice history. They are separate because
the graph is regenerated freely and practice history has to survive that. Both are
written atomically — a crash mid-write must not leave a goal file that no longer parses,
because the state in it is not reconstructible. No database, no daemon, no sync: the
whole store is deletable without ceremony, which is the discipline that replaces
statelessness for a tool that must accumulate.

Those files have a home and the binary knows it, because a study tool that demands a path
on every invocation gets a `~/benkyou` invented for it by whoever runs it first. Goals and
fluency are **data** — graded evidence that cost the learner real time and is
reconstructible from nothing — so they live in `$XDG_DATA_HOME/benkyou/goals`, addressed
by bare name. Workspaces are **state**: scratch for one sitting, rebuildable from the
exercise directory, with only the typed answer at stake, and that is graded out into
fluency before it matters. They live in `$XDG_STATE_HOME/benkyou/exercises`, on a path
derived from the task so that `attempt` and `grade` cannot disagree about where the work
is. Anything that still looks like a path is treated as one, so the exercises in this repo
run from a checkout without ceremony.

Generation lives in the host agent. The CLI emits a structured *generation order*; the
agent — already in a conversation, already paying for a model — fills it and writes the
result back through a second call.

An order is not a template, and the difference is the reason the command exists. What it
carries is what the agent cannot see: which node the schedule says to work on, which
prerequisites are *proven* rather than merely assumed, which dependents must not be
spoiled, and how much of the solution to show. That last one is decided from the
assessment rather than by taste — a worked example helps a novice and actively harms a
learner who has already demonstrated the concept — so `guidance_level` is `worked` for a
node the learner failed, `blank` for one they passed, and `faded` in between.

The order also refuses work that cannot exist. Only `skill` and `tool` nodes can carry a
graded exercise, because an exercise is passed by *doing* something a grader can run;
asking for a kata on a bare fact yields one that grades trivia, so the request is
rejected and pointed at `--kind cards` instead.

This is forced rather than chosen. MCP's `sampling` (a server asking the host's model to
generate something) was **deprecated in spec revision 2026-07-28, SEP-2577**, with the
official migration path being "integrate directly with LLM provider APIs" — and no major
harness implemented it anyway. A server cannot borrow the host's model. Its alternative
is its own API key, which is exactly what turns a companion into a separate application.

### The execution boundary

Gate and grade execute code written by a model and by a learner. `solution/solve.sh`
and `check/check.sh` come out of a generator; the workspace command is whatever was
typed. The runner treats all of it as untrusted.

An earlier draft of this document argued the opposite — that the code being graded is
the learner's own solution on their own machine, so there is no adversary to isolate
from. That sentence was false about the code it described. The learner never writes
`check.sh`, and it runs at *gate* time, before anyone has sat down to anything.

The threat it got wrong is also not the interesting one. A malicious author is a
possibility; a mistaken generator is a certainty. The failures that actually turn up
are a delete with an unset variable in its path, a relative path that resolves out of
the workspace, a loop that fills a disk, a process tree that will not die, a script
that reads a credential file because it was in the training data. A warning printed
before the fact contains none of them.

So there is one execution interface and two backends behind it. A caller builds a job —
a run directory, a list of children it may see and whether each is writable, a working
directory, a script, a deadline — and hands it over. Every execution in the tool goes
this way: both gate directions, the advisory `run_cmd`, `grade`, and the browser's Run
and Submit.

**Sandbox** is the default: a user, mount, PID, IPC, UTS and network namespace via
`bubblewrap`, a read-only `/usr`, a synthetic one-line `/etc/passwd`, a bounded tmpfs
for `/tmp`, a throwaway `$HOME`, an environment allowlist, and resource ceilings. The
job's view of the filesystem is the list it was given and the read-only runtime.
Nothing else is there — not the exercise directory, not the study state, not the user's
home.

**UnsafeHost** is the escape hatch, reachable only by passing `--unsafe-host`. It runs
as the user, with the user's rights, over the whole filesystem. There is no prompt:
`gate` runs unattended inside a generation loop, so consent has to be expressible as a
flag. The name is the documentation, which is why it is not called `native` or
`direct`.

A missing sandbox is a refusal, not a downgrade. Claiming isolation while only changing
the working directory is worse than not having it, because then the warnings stop being
read.

The two backends differ in isolation and deliberately in nothing else: same environment
allowlist, same limits, same relative layout. When an exercise passes under one and
fails under the other, the difference is isolation, and the search does not also have to
cover a stray `PYTHONPATH`.

Two things fall out of the view mechanism rather than being asked for. The gate's
reference solution no longer sees `check/`, so a `solve.sh` cannot pass the first
direction by reading the tests it is supposed to be independent of. And a grader cannot
reach the exercise directory it was copied from, which retires a whole class of
mid-gate corruption that the digest could previously only detect after the fact.

What the sandbox does *not* give: reproducibility. `/usr` is bind-mounted from the host,
so the interpreter and its packages are the host's. A verdict is not portable to another
machine and never claimed to be — see the environment fingerprint above.

One limit is namespace-only. `RLIMIT_NPROC` counts processes per user, not per tree, so
on the host it is measured against the whole logged-in session: set below it nothing
forks, set above it a fork bomb still has headroom. It needs a namespace to mean
anything, and it is applied only where there is one.

The wall-clock deadline predates all of this and still matters most. An infinite loop in
a draft solution is an ordinary mistake and the gate has to report it as one rather than
hanging. Killing only the shell is not enough — a backgrounded grandchild keeps the
output pipes open, and reading them to end then never returns. Under the sandbox the PID
namespace makes this total; on the host the process group is all there is.

### The browser runner

`serve` puts an editor, a Run button and a Submit button on `127.0.0.1`. It exists
because the editor-and-two-commands loop asks the learner to hold the workspace path in
their head between `attempt` and `grade`, and because a queue of exercises has no
representation at all outside a shell history.

**Execution stays in the process; the page is a view.** Run and Submit both go through
the same `Runner` and the same grader the CLI uses. The tempting middle — WASM runs the
code, the CLI grades it — is the worst cell in the matrix: two execution environments
mean the gate's twice-run guarantee holds in neither, and the learner gets a green in
the browser and a red from `grade` with nothing to arbitrate. Pyodide 314.0.4 is Python
3.14 with pandas 3.0.2, so the temptation is real; it is declined on those grounds and
not on capability.

That also settles the confidentiality question, which is otherwise fatal. A page that
graded client-side would have to ship the grader to the client, and view-source defeats
`hidden`. Grading in the process keeps hidden cases where they already were.

The queue is the argument list: directories, or digests of banked exercises. The
"nothing maps a concept to a directory" argument that used to sit here has expired —
the bank records each bundle's `concept_id`, so the index exists. What is genuinely
missing is a *selection policy*. Several exercises can serve one concept, and choosing
between them is a learning decision, not a lookup: an exact repeat measures speed on a
known route, a fresh one measures whether the skill transferred, and the right answer
depends on which of those the session is for. Picking the least recently seen would be
a policy invented at a call site, so until that decision is made deliberately the
caller names what it wants.

Two locks, with opposite policies, because the failure modes are not symmetric. An
execution takes the workspace lock with `try_lock` and *refuses* on contention: queueing
a double-clicked Run would execute the learner's code again after they already have
their answer. A save takes the same lock and *waits*: refusing would discard something
they typed, and a save landing mid-`grade` yields an artifact that is half of one
revision and half of another — a verdict nothing can reproduce.

Each attempt appends `attempt.jsonl` beside the workspace: open, run, submit, next,
done, with durations from a monotonic clock. The contrast with Anki is narrower than it
is tempting to write. Anki *does* record answer seconds, and its 60-second cap is an
adjustable default; what it does not do is let duration reach the scheduler, because
FSRS consumes `(rating, delta_t)` and nothing else. The same rule holds here, by
choice rather than by limitation: **recorded, never scheduled on.** What this log adds
over `revlog` is structure — run and submit are separate events, with exit codes —
not the mere fact of a timestamp. Wall-clock in a browser tab measures whether the tab
was open; monotonic duration at least measures the process. Neither is evidence of
mastery, and §5 already has its four numbers.

There is no keystroke replay. The events are the ones with a meaning a person can read
six months later; a keystroke stream is a recording of typing, not of learning.

What this cost, at the sandbox, was per-exercise dependencies. There is no network, so
`pip install` fails and a PEP 723 header under `uv run` buys nothing — `uv` itself runs
fine; resolution is what dies. A venv can still be *created*; it is simply empty, and
nothing can be installed into it.

### Declared dependencies

The recovery is not to give runs a network. It is to move the one step that needs one
out of the grading path entirely, into a command a person runs on purpose:

```toml
[deps]
python = ["pandas==3.0.5"]
```

`benkyou warm` installs that list on the host, with the network, into
`$XDG_CACHE_HOME/benkyou/sets/<abi>/<digest>`. Every later run binds the directory
read-only and sets `PYTHONPATH` to it. Nothing resolves at grade time, nothing writes to
the set, and no generated script ever reaches an index.

Five decisions carry the design, and each was a wrong turn first:

**`uv pip install --target`, not a venv and not a cache overlay.** The first attempt
bound a warm `uv` cache with `--overlay-src … --tmp-overlay` and ran `uv run --offline`
inside. It works — measured — but it keeps a resolver in the grading path and needs the
cache writable. A `--target` directory is a plain relocatable tree of packages, so the
sandbox needs one read-only bind and an env var. A venv is the wrong shape for the same
reason it looks right: it records absolute paths in `pyvenv.cfg` and in every console
script, so one built at a cache path and bound elsewhere is subtly broken.

**Keyed by the interpreter's ABI, not just the packages.** `uv` will install a
`cpython-313` wheel for a sandbox running 3.14, and the failure is a
`ModuleNotFoundError` four frames inside numpy that names nothing relevant. Warming pins
`--python` to the interpreter the sandbox mounts, and `SOABI` is part of the set's path,
so an interpreter upgrade misses the cache and says so.

**Exact pins, enforced.** A set is keyed by its requirement list. `pandas` would name one
directory on Monday and different bytes in a month: a stable key over changing content,
which is precisely what the rest of this tool refuses to do with a digest. `==` is
required and a range is rejected — including `==1.0,!=1.0.1`, since one exact comparator
among several still admits whatever the index decides `1.0` means today.

**Registry names only, and only wheels.** Warming runs on the user's machine with the
user's rights, and its argument list comes out of a *generated* file. `git+https://…`
clones and builds, `./thing` and `-e .` build from a path, a leading `-` is a flag rather
than a package and `--index-url` alone redirects the whole install. A small PEP 508
allowlist admits a name, optional extras and one exact version; `--only-binary :all:`
covers the transitive tree, because a plain registry name can still resolve to an sdist
whose `setup.py` would run here.

**The manifest is the set's identity.** A pin fixes the names the author wrote and
nothing below them, so `warm` records every `.dist-info` it installed and the gate keeps
that list beside its verdict. A set whose manifest is missing or unparseable is refused
rather than read as empty — the alternative is a verdict claiming to name a tree nobody
can identify. A later change to the tree warns, like `Env` drift and for the same reason:
it describes the environment a verdict was earned in, not the exercise.

### What isolation still does not buy

**Hermeticity.** `/usr` is the host's, read-only, so the interpreter and every
system-installed package still come from the machine, and a grader's result is only as
reproducible as that machine. Declared dependencies narrow this — the packages an
exercise names are pinned and recorded — but they sit on top of a host interpreter.

What isolation does buy is *isolation*: no network, no host filesystem, a scrubbed
`HOME`, a private tmpfs, and a fresh workspace per run, so nothing a run leaves behind
reaches the next one or the user. Those are different properties, and only the second is
something a namespace can supply.

If hermeticity is wanted later, the answer is still an image behind the same `Backend`
interface, chosen per task — not more flags on the fence.

---

## v1: one thin path, end to end

Graph → prune → both kinds of practice, for a single goal, with one gradeable kind
(`kata`) implemented.

1. graph generated from a target, validated (slugs deduped, low relevance dropped, node
   count capped around 150; a `requires` cycle is reported and refused, not repaired)
2. ~10-question prune, fringes reported
3. a handful of cards for one node, emitted and importable, GUIDs concept-stable
4. **one exercise that passes the validation gate**, is run, and is graded
5. state persists and the second run behaves differently from the first

Item 4 is the one that proves the thesis. Item 5 is the one that makes it a tool rather
than a function.

### Explicitly not in v1

Graph editor. Graph visualizer — mermaid in the agent's output is sufficient; leading
with the picture is how Learney ended up with a drawing and no engine. Shared or
community content. Knowledge tracing — BKT and DKT need thousands of interactions per
skill and n=1 is noise. FSRS over exercises. Lab or VM infrastructure. A web UI. Multiple
languages. A task registry. Any scoring that sorts.

The failure mode to watch for is spending the whole budget perfecting graph generation.
It is fun, infinitely tunable, and the graph is scaffolding.

---

## Evidence

Skill acquisition — Anderson 1982, *Psych Review* 89(4); Taatgen & Anderson 2002,
*Cognition* 86. Practice curves — Heathcote, Brown & Mewhort 2000, *Psychon Bull Rev*
7(2):185–207. Deliberate practice — Ericsson, Krampe & Tesch-Römer 1993; Macnamara,
Hambrick & Oswald 2014, *Psych Science* 25(8) (deliberate practice explains ~26% of
variance in games, 21% music, 18% sports, 4% education, under 1% in professions — sell
individualization, not ten thousand hours). Interleaving — Rohrer & Taylor 2007,
*Instructional Science* 35:481–498; Rohrer et al. 2020 RCT. Expertise reversal — Kalyuga,
Ayres, Chandler & Sweller 2003, *Educational Psychologist* 38(1):23–31; backward fading
from Renkl & Atkinson. Procedural spacing — Spruit et al. 2014, *Surgical Endoscopy*
(PMID 25318372); Cecilio-Fernandes et al. 2017 (PMID 28843958); Gonzalez et al. 2011
(PMID 21167363). Knowledge space theory — Doignon & Falmagne, *Learning Spaces*, Springer
2011; Falmagne, Doignon, Cosyn & Thiéry, "The Assessment of Knowledge, in Theory and in
Practice". Frontier effect — Zou, Ma, Ma & Baker, AIED 2019. Judge reliability — Zheng et
al. 2023, arXiv:2306.05685; Pombal, Rei & Martins 2026, arXiv:2604.06996.

Prior art read: Metacademy (schema and content format), Learney (post-mortem), Orbit and
Matuschak's "Fluid practice for fluid understanding", Math Academy ("The Math Academy
Way", encompassings and FIRe), ALEKS, Exercism `problem-specifications` and the test
runner interface, Killercoda scenario format, keybr `guided.ts`, Execute Program,
SadServers, Katacoda (dead 2022-06-15).
