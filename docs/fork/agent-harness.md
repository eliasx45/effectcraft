# Agent harness: where it stands, and an EC-native agent with evals

Fork-only (eliasx45). Written 26 October 2026 from the code and commit history on `main` at
`08a8478`, plus a reading of [smhanov/agentloop](https://github.com/smhanov/agentloop) (MIT, Go).
Goal: an agent that **edits video** in EffectCraft and gets better in a way we can measure. This
isn't about checking code.

## 1. What agentloop is, briefly

agentloop is a Go library (about 6 000 lines with tests; one dependency, `llmhub`, for model
providers) that implements the agent loop: send the prompt, run the tool calls the model asks for,
feed the results back, repeat until there's an answer. Its ideas are worth more to us than its
code:

| Idea | What it means |
|---|---|
| One `Hooks` struct | `OnStart`, `BeforeGenerate` (append steering messages, or `Skip` to stop), `RebuildContext` (replace what the model sees for one call; the transcript stays whole), `AfterGenerate` (the **finish veto**: refuse to let the run end), `BeforeToolCall` (rewrite args, veto, block for approval), `AfterToolCall`, `OnDelta`, `OnDone`, `OnError`. Both bundled agents are built from these hooks alone |
| Every run is bounded | Tool-call budget, then a *forced final answer* with a "budget spent, answer now" notice; a hard step ceiling; stop after N identical calls in a row; a stream watchdog; `ctx` |
| Tool errors are fed back to the model | A failing tool returns `{"error": …}` to the model, which repairs it; an unknown tool name gets "did you mean …" |
| Skills in layers | `SKILL.md` folders: a one-line catalog in the prompt, the body loaded on demand through a jailed `skill_read`, then reference files, then scripts. **Same format as Claude Code skills** |
| A full trace per run | Every tool call with args, result and duration, the transcript, tokens and cost |
| Scripted fake providers | Tests run the whole loop with no network and no model, deterministically |

## 2. Map: what the EC harness already has

The harness is the **surface an outside agent drives**. Today Claude Code is the loop; EC
provides tools, senses and judgement.

| Layer | What exists | Where |
|---|---|---|
| **Hands** (doing) | ~70 MCP tools over every engine command (`execute_command`, `command_batch`, `run_script`, `set_property`, `add_keyframe`, `animate_layer`, `title_card`, `lower_third`, …), CLI, JSON-lines control channel; per-client undo, `expectRevision`, `comp.claim` | `crates/automation/src/tools.rs`, [mcp.md](../mcp.md), [agents.md](../agents.md) |
| **Plumbing** (HP1, step 1) | Tools accept each other's argument names; `run_script {file}`; `get_result` by id; failing scripts roll back and report the expressions they broke; short replies (`command_batch` inlines small results, lists `failedSteps`) | `tests_plumbing.rs` |
| **Senses** (HS1) | Ease readout in words, timecode in and out (`6:20`, `200f`), the change feed (`edit_feed`: who changed what, in words), `agent.notices` (the user changed something since your last call), `context.packet`, `layer.related` | `senses.rs`, engine `feed.rs` |
| **Eyes** | `render_frame(s)`, contact sheets, `render_compare`, `get_layout`, `screenshot`, `get_motion` (segments in words; HT2: rig nulls and `movedBy`), `analyze_audio`, speech maps | |
| **Judgement** (HU1, HT1) | `render.lint` / `lint_frame` (small or low-contrast text, title-safe, cut-off, overlap, off-beat, text-off-centre, nested-cut-off, exposed-edge, **off-taste**); the **taste check**: every keyed move judged against Elias's ease profile (5 criteria approved 25 Oct from 730 moves + 101 post-handover edits), with a `fix` per finding; `taste.approve` refuses agent clients | `crates/engine/src/taste.rs`, `lint` |
| **Reuse** (HU2) | `library.scene` applies a Motion Library scene by a loose name, swaps its texts and footage; `text.apply` presets | |
| **Attribution** (HG1) | A `client` next to `command` runs as that client; feed entries name the comps they touched | `control_wire.rs` |
| **Outside the repo** | Claude Code hooks on Elias's machine: `ec-handover-snap` (Stop hook: snapshots the comps an agent hands over, before Elias fixes them), `ec-comp-diff` | `~/.claude` (not in git) |
| **Evals** | The **agent exam**: 21 motion tasks (lower third, easy ease, logo fly-in, parenting, track matte, reframe, …), deterministic checks via `exam.check`, `tests_exam::every_task_is_solvable` proves each task is achievable, `cargo xtask agent-exam --run` runs Claude Code headless and writes a report | `evals/agent/`, `xtask/src/agent_exam.rs`, `crates/engine/src/exam.rs` |

### Gaps

1. **No loop of our own.** Claude Code is the loop, so budgets, steering, context and the "you may
   not stop yet" rule live in Claude Code's settings and hooks, or nowhere. Our judgement
   (lint, taste) is advice the model may ignore.
2. **The exam has no recorded result.** I found no report or pass rate in the repo; we don't
   know the baseline.
3. **Exam tasks are synthetic and one-shot.** None comes from a real project, none is multi-turn
   ("now make it snappier"), none uses footage or speech.
4. **The best signal isn't used as an eval yet.** Each handover is an agent's version (snapshot)
   plus the version Elias fixed. That's the ground truth for "good editing", and HT1 already mined
   it for the ease profile, but it isn't an eval.
5. **No trace analysis.** `stream.jsonl` is kept, but nothing classifies *why* a run failed (wrong
   tool, misread reply, missing command, gave up, taste).
6. **One trial per task.** No repeats, so no reliability number and no way to tell noise from
   improvement.
7. **One vendor, one model.** The runner calls `claude`; comparing models or prompts is manual.

## 3. Evals, from the ground up (for this project)

**Vocabulary.**
- **Task**: a starting project, an instruction and the checks.
- **Trial**: one agent run on a task.
- **Grader**: what scores a trial.
- **Harness**: what runs trials and collects results. The agent's harness and the eval harness
  are different things: the agent's harness is what the agent works with; the eval harness
  measures it.
- **Trace**: everything the agent did in a trial.

**Three kinds of grader**, cheapest and most trusted first:
1. **Code graders**: deterministic facts from the saved project. That's `exam.check` today:
   positions, keys, segments, lint, taste. Fast, free, repeatable. Can only check what you can
   state as a fact.
2. **Model graders**: a model looks at frames or the motion summary and scores against a rubric
   ("does the lower third feel premium?"). Covers taste a rule can't capture, but it's noisy and
   biased, so it's only worth trusting once it's **calibrated**: it has to agree with Elias's own
   verdicts on a held-out set before it grades anything.
3. **Human grading**: Elias. The ground truth, slow, so spend it on calibration and on the hard
   cases, never on what a code grader can check.

**Numbers that matter.**
- **pass@k**: at least one of k trials passes. "Can it do this at all?"
- **pass^k**: *all* k trials pass. "Can I trust it?" For a tool you hand real work to, pass^k is
  what counts. A 70 % task has pass^5 ≈ 17 %.
- **Score**: the fraction of checks passed (exists). Use it as partial credit, not as the
  headline.
- **Cost**: tool calls, minutes, tokens and dollars per *passed* task.
- **North star: Elias's edits after handover.** The fewer property changes he makes to a comp an
  agent handed over, the better the agent. The change feed and `ec-handover-snap` already record
  them.

**Two kinds of suite.**
- **Capability suite**: hard tasks, low pass rate. It shows what to build next.
- **Regression suite**: tasks the agent passes reliably. It must stay green after every change to
  the prompt, tools, model or harness.

A task that saturates (always passes) moves from the first to the second.

**Variance is the enemy.** Agents are random. Run every task 3–5 times. Treat a change as real
only when it beats the spread between runs; 21 tasks × 1 trial can't tell a 5-point improvement
from luck.

**Read the traces.** Scores tell you *that* it failed; traces tell you *why*. Most of the gain
comes from reading 20 failed traces and finding the 3 patterns: a confusing reply, a missing
command, a tool description that misleads. The exam README already says this ("a task that agents
keep failing points at a missing command…"). Make it a step with a tool.

**Don't let it see the answers.** The agent must never see `solution` or the checks. Real-project
tasks must stay out of anything the agent can read, and out of git (personal projects stay local,
AGENTS.md §1).

## 4. Where to build the loop: three routes, each pushed to its best and worst

### A. Go sidecar on agentloop

A separate Go program (its own repo) that uses agentloop and calls EffectCraft over MCP or the
control channel.

- **At its best:** it works this week. `llmhub` switches between Anthropic, OpenAI, Gemini, local
  Ollama and others by changing a flag, so a model-comparison eval (same tasks, five models, cost
  per pass) is a config change. The hooks, budgets, loop detection, skills and traces are written
  and tested. It's a 7 MB static binary for a nightly cloud box. The scripted provider gives
  deterministic tests of our own steering logic.
- **At its worst:** it's a second language in a pure-Rust project. Every hook sees EC only
  through JSON over a socket: the finish veto has to call `taste_check` over MCP, it can't read
  engine state directly, and it can't live in the app UI or the browser (wasm). Tool schemas come
  from `list_tools`, so they stay in sync, but types, error shapes and the time format are
  re-parsed on the Go side. It's a dependency on one person's pre-v1 library (the minor version is
  the breaking-change signal). Upstream won't take it, and you'd maintain a Go toolchain on the
  build machine.
- **Ceiling:** an excellent *eval and research rig*; a poor *product*.

### B. Rust-native agent crate (`effectcraft-agent`), a clean-room port of the ideas

A new crate at L5/L6 (it needs HTTP, so it stays out of L0–L4 and the wasm core), with:
- the loop;
- the same hook points;
- budgets with the forced final answer;
- duplicate-call detection;
- skills in Claude Code's `SKILL.md` format;
- a trace;
- a scripted provider for tests;
- an Anthropic Messages API client first, with others behind a trait.

- **At its best:** the agent runs **inside** EffectCraft. Hooks touch the engine directly.
  - The finish veto is `taste::check` + `render.lint` on the live document: the agent can't hand
    over an off-taste or lint-failing comp, because the engine says no, not because a prompt
    asked nicely.
  - `RebuildContext` rebuilds `context.packet` from the real document every step, never stale,
    instead of piling up old tool output.
  - `BeforeToolCall` uses `comp.claim`, per-client undo and `expectRevision` natively. Every
    approval gate is a real UI dialog in the app.
  - Steering is Elias typing in an in-app panel while the agent works (`BeforeGenerate` drains it).
  - The trace *is* the command journal, so any failed trial replays bit-for-bit with repro
    scripts, and the scripted provider plus a saved journal makes an agent bug a normal unit test.
  - The same crate powers the app's assistant panel, the CLI (`effectcraft-cli agent "…"`), the
    exam runner and a web build (HTTP from the browser, L6).
  - One language, one build, never-crash rules and clippy on all of it, upstreamable.
- **At its worst:**
  - We write and maintain an LLM client (HTTP, server-sent-event streaming, tool-call JSON,
    retries, prompt caching) and keep up with API changes; at first, one provider only.
  - It costs weeks before the first eval result: roughly agentloop's size, maybe 4–6 k lines of
    Rust with tests.
  - Hooks that are too clever can hide failures from the model; the loop must stay as simple and
    transparent as agentloop's.
  - It competes with Claude Code, which is a stronger general agent (file editing, its own tools)
    than a first loop will be.
- **Ceiling:** the highest by far. A product feature and an eval rig in one, with judgement the
  model can't route around.

### C. Keep Claude Code as the loop, map agentloop's hooks onto Claude Code's

Claude Code already has hooks (`PreToolUse`, `PostToolUse`, `Stop`, `UserPromptSubmit`, …), skills
in the same format, headless `-p --output-format stream-json`, and `--max-turns`. Elias already
runs a Stop hook (`ec-handover-snap`).
- **At its best:** almost no code. A `Stop` hook that runs `taste_check` + `lint_frame` and
  **blocks the stop** with the findings is agentloop's finish veto. `PreToolUse` is the approval
  gate. `UserPromptSubmit` injects `context.packet`. EC skills (`SKILL.md`) are the procedures. It's
  the strongest agent today, and the exam runner already drives it.
- **At its worst:**
  - One vendor; model comparisons are limited to Claude models.
  - There's no `RebuildContext`: you can't reshape what the model sees, only add to it.
  - Hooks are shell processes, so everything is JSON again.
  - It costs a full Claude Code session per trial, which is expensive at 5 trials × 50 tasks.
  - Its behaviour changes with its own releases, so eval results drift for reasons outside EC.
  - It isn't in the app, and you can't ship it to EffectCraft users.
- **Ceiling:** a strong baseline and a power-user setup on Elias's machine, but you don't own it.

### Recommendation

Do them **in order, not instead of each other**:
1. **Now, route C.** Get a baseline. Add the taste and lint finish veto as a Claude Code Stop hook,
   and measure it with the exam. That's days, not weeks, and it answers the first question: do
   hard gates help?
2. **Then route B.** Build the Rust agent crate, porting agentloop's *design* (clean-room: its
   README and behaviour, not its code, even though MIT would allow it; repos don't share code).
   The Claude Code run stays in the exam as the baseline to beat.
3. **Route A only as a throwaway spike**, if we want a multi-model comparison before B has more
   than one provider. Never as the product.

## 5. The plan

**E0: baseline (first).** Run the exam as it is: 21 tasks × 3 trials, one model. Record the pass
rate, pass^3, mean tool calls, minutes and cost in `docs/fork/agent-evals.md` (a new file; history
appended). Read every failed trace and file the patterns.

**E1: exam upgrades.**
- `--trials N` with pass@k and pass^k in the report.
- Per-trial cost from `stream-json` usage.
- A taste check shorthand (`{"taste": {comp, layer?}, "maxVerdict": "close"}`) so tasks grade ease
  quality.
- A `notes` field in the report for trace tags.
- Tag every task `capability` or `regression`.

**E2: trace analysis.** `cargo xtask agent-exam --analyze DIR` groups failures by cause from the
trace: the tool error text, repeated identical calls, the last tool before giving up, and the
checks that failed. Use a model to tag only what rules can't, and check the tags by hand at first.

**E3: handover tasks (the gold).** From `ec-handover-snap` pairs, build local, gitignored tasks:
- **Start:** the brief's project before the agent ran.
- **Prompt:** the original instruction.
- **Graders:**
  - (a) the code checks the fixed version passes;
  - (b) the **distance to Elias's fix**: how many of his post-handover edits the agent's version
    would still need, read through `edit_feed`-style property diffs;
  - (c) taste and lint clean.

Real projects never leave the machine.

**E4: multi-turn and footage tasks.**
- Revision rounds ("make the pop snappier", "move it to the beat on 'copy'").
- Speech-map and beat tasks with generated audio.
- Footage tasks with own or generated media only.

**E5: calibrated model grader.**
- Frames: a contact sheet from `render_frames` plus `get_motion` text, scored against a rubric.
- Calibration: measure agreement with Elias's verdicts on 30–50 held-out comps.
- Rule: use it only where it agrees ≥ 80 %, and always alongside the code checks.

**B1–B5: the Rust agent crate** (after E0–E2 give us the measuring stick):
- **B1:** loop, budgets, forced final answer, dup detection, trace, scripted provider; tests only.
- **B2:** Anthropic client (streaming, tool use, prompt caching); `effectcraft-cli agent`.
- **B3:** hooks wired to the engine: the taste and lint finish veto, `context.packet` in
  `RebuildContext`, claims and approval in `BeforeToolCall`, a steering inbox.
- **B4:** skills from `SKILL.md` folders (shared with the Claude Code setup).
- **B5:** the exam runs B and Claude Code side by side. B ships in the app only once it beats or
  matches the baseline on pass^3 and post-handover edits.

## 6. How agentloop's hooks map to EffectCraft

| agentloop | EffectCraft use |
|---|---|
| `AfterGenerate` finish veto | Can't hand over while `taste.check` has an `off` verdict or `render.lint` has errors; findings with their `fix` calls are appended so the model repairs in-band |
| `RebuildContext` | Fresh `context.packet` (comp, selection, timecode, eases, recent feed) every step; old `get_comp` dumps dropped |
| `BeforeGenerate` steering | Elias's notes typed during the run; `agent.notices` when he edits the comp meanwhile |
| `BeforeToolCall` | Claims, `expectRevision`, approval for destructive commands (delete layers, replace footage), rewrite times to `{frame}` |
| `AfterToolCall` | Trace into the journal; count edits per comp |
| Budget and forced answer | "Budget spent: save, list what's left undone" instead of a silent stop |
| Dup-call limit | Catches the "render frame, look, render the same frame" loop |
| Skills L0–L3 | Motion recipes (lower thirds, pops, rigs, Elias's ease rules) as `SKILL.md`; scripts = `.jsx` via `run_script {file}` |
| Scripted provider | Agent-loop bugs as unit tests, no model, no network |
