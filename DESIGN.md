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

**FSRS is used for cards and not for exercises**, and the reason is arithmetic rather
than philosophy. FSRS needs roughly 64 review logs *per item* before its optimizer means
anything. At one exercise a day, 64 reviews of a single exercise is eighteen months. The
data density never arrives. Separately, SM-2 and FSRS are both fit to binary recall of
short verbal items at sub-ten-second latencies; no published work validates either on a
twenty-minute task.

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
has been run twice:

1. apply `solution/solve.sh` → correctness must be 1.0, else discard
2. untouched `setup/` → correctness must be < 1.0, else discard as vacuous

Without both runs you are shipping prose with a test file next to it. For debugging
tasks, additionally record the checks that must *stay* green, so the exercise cannot be
passed by deleting things.

The rule is enforced where it can be bypassed, not where it is convenient: both
`attempt::open` and `attempt::grade` refuse an exercise whose `[gate]` block is
absent, and `benkyou gate` writes that block into `task.toml` on success. Grading is
the stricter of the two doors — a grader nobody proved discriminating would still
write a score into the learner's fluency, where it decides what they see next.

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

---

## 4. Cards

Anki does one thing better than anything else: it is a durable, synced, mobile,
FSRS-tuned queue. It gets to do exactly that and nothing more.

Its limits are structural rather than missing features: answer time is capped at 60
seconds by default, so it cannot even *measure* a long task; grading is four self-report
buttons with no channel for "passed 7 of 9 assertions"; the per-card side channel is
under 100 bytes with keys of 8 bytes or less; and there is no way to express "don't show
B until A is mature". Ordering lives in our graph; Anki receives the result.

**Note GUIDs derive from `concept_id + card_role`, not from content.** Content-hashed
GUIDs mean that editing one concept returns every affected note as a *new* note and
throws away its review history. A tool that re-projects a persistent graph hits this on
the second run.

Card content is code-heavy, so field values are sanitized with an allowlist
(`pre`, `code`, `span[class]`) rather than escaped wholesale — escaping every field
renders code blocks as literal markup and silently defeats the styling.

---

## 5. Scheduling the procedural track

Not an SRS. The model is mastery gating, taken from keybr — a tool whose whole value
proposition is adaptive practice and which contains no scheduler at all: no due dates, no
intervals, no forgetting curve.

- per-concept `confidence` measured against a target
- a node unlocks only when every `requires`-predecessor has confidence ≥ 1
- among unlocked concepts below target, the single weakest becomes the generation focus

Layered on top, the one thing keybr lacks and the spacing literature supports: coarse
**session-level** spacing on days-since-last-touched, with exponential decay of
confidence.

Session composition is interleaved across concepts, never blocked by topic — the
guarantee being that no two adjacent entries share a concept whenever two or more are
practisable. It has to degrade rather than fail when they do not: a session is always
`session_size` entries, so with a single practisable concept it is that concept
repeated. Interleaving is a property of the ordering, not a promise of variety the
schedule cannot always keep.

State per concept is four numbers: `{ best_score, last_practiced, attempts, confidence }`.

Retire on a mastery criterion rather than reviewing forever. Individual learning curves
are exponential with a hard asymptote, not power-law — the power law is an averaging
artifact (Heathcote, Brown & Mewhort 2000).

Retirement has to be reversible or it is a trap. Confidence otherwise only accumulates
— `confidence += score` — so a concept that reached `retire_at` could never come back,
and failing its exercise would add zero and leave it retired forever. That is exactly
backwards: failing is the strongest available evidence that a concept is *not* mastered.
So a direct attempt scoring below `lapse_at` discards the confidence accumulated so far
instead of adding nothing to it, and the concept is schedulable again on the next
session. `best_score` and `attempts` are untouched: what the learner once managed
remains true.

Only a direct attempt can demote. Credit arriving over an `encompasses` edge is a
verdict on the node that was attempted, not on its neighbours, and must never cost them
the confidence they earned.

---

## 6. Shape of the tool

A single Rust binary over plain JSON files, plus a skill that teaches an agent to drive
it. It makes **no network calls and holds no API key**, and the only loopback traffic is
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

There is no sandbox. The code being graded is the learner's own solution to their own
kata, on their own machine, so there is no adversary to isolate from — and isolation
would cost a hard dependency on Linux namespace tooling to defend against a threat that
does not exist. What the runner does enforce is a wall-clock deadline, killing the whole
process group: an infinite loop in a draft solution is an ordinary mistake, and the gate
has to report it as one instead of hanging. Killing only the shell is not enough — a
backgrounded grandchild keeps the output pipes open, and reading them to end then never
returns.

What this costs is *not* capability. An exercise that wants a dependency can build a
venv in its own run directory and install into it — verified: `python3 -m venv .venv &&
.venv/bin/pip install six` succeeds in the runner. The nsjail configuration it replaced
could not do that at all, having no network namespace and a read-only `/usr`. A fence is
not an environment provider.

What it costs is **hermeticity**. The run sees the host's interpreter, the host's
network, and whatever the last run left behind. So a grader's result is only as
reproducible as the machine it ran on, and a kata that damages state outside its run
directory will succeed in doing so. Both are properties of an *image plus a throwaway
filesystem* — a container or a microVM. Namespace fencing was never going to supply
them, which is the real reason its removal costs less than it looks like it should.

If hermeticity is wanted later, the answer is a container backend behind the same
`Runner` interface, chosen per task, not a fence bolted onto the host filesystem.

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
