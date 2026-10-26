# Agent harness: where it stands, and an EC-native agent with evals

Fork-only (eliasx45). Written 26 October 2026 from the code and commit history on `main` at
`08a8478`, plus a reading of [smhanov/agentloop](https://github.com/smhanov/agentloop) (MIT, Go).
Goal: an agent that **edits video** in EffectCraft and gets better in a way we can measure. This
isn't about checking code.

## 0. Decisions (keep this section current)

Elias's decisions so far, newest last. Change a row only when he changes his mind, and say so in
the history at the end of this file.

| # | Decision | Date |
|---|---|---|
| D1 | **Goal:** an agent that edits video in EffectCraft, measured by evals. Evals are for editing quality, not for checking code | 26 Oct |
| D2 | **Build both and compare.** *Outside the harness:* Claude Code is the loop (route C), with EC's MCP tools, hooks and skills, on the subscription. *Inside the harness:* our own Rust loop (route B), on an API key. Both run the same exam | 26 Oct |
| D3 | **One policy core, two drivers.** The rules (taste/lint gate, context, approvals, budgets, trace) are written once in Rust. Claude Code reaches them through `effectcraft-cli hook …`; our loop calls them directly | 26 Oct |
| D4 | **agentloop is a design reference only.** Clean-room: ideas from its README and behaviour, no code copied. Route A (a Go sidecar) is not the product; at most a throwaway spike | 26 Oct |
| D5 | **API key is fine for testing B.** Whether to keep paying for it long term depends on a measured, significant improvement over C | 26 Oct |
| D6 | **Claude models first.** Other models only as an eval comparison (e.g. through an Anthropic-compatible endpoint) | 26 Oct |
| D7 | **Terms:** the subscription is only used by unmodified Claude Code (interactive, or `claude -p` without `--bare`). B and A never run through subscription credentials. `ANTHROPIC_API_KEY` stays unset for subscription runs, or they bill the key | 26 Oct |
| D8 | **Don't embed Claude Code in the app now.** Build the two gains without embedding: *pins* (frame, region or range sent to the agent) and a *companion panel* (Claude Code turns linked to their revisions). Revisit embedding only if the agent ships to other users | 26 Oct |
| D9 | **Real projects stay local.** Handover tasks built from Elias's projects never go into git or anything the agent can read | 26 Oct |

| D10 | **Build C and B in parallel** after the baseline (E0), both on the policy core (P1), which comes first since both need it | 26 Oct |
| D11 | **Models to compare:** Opus and Sonnet, plus one non-Claude model through an Anthropic-compatible endpoint. Not Haiku for now | 26 Oct |
| D12 | **North star: Elias's edits after handover.** Fewer property changes he makes to a comp an agent handed over = better. Exam pass^3, cost and minutes are secondary | 26 Oct |
| D13 | **Small eval runs to start:** a handful of tasks × 3 trials per driver and model, then decide whether to scale to the full exam | 26 Oct |

| D14 | **API budget: $200 of promotional API credits** for B and other key-billed runs (check their expiry date in the Claude Console) | 26 Oct |
| D15 | **A third driver: Codex CLI on Elias's ChatGPT/Codex plan.** Another outside harness (OpenAI's agent loop and GPT models) on the same EC MCP tools and the same policy core: Codex reads MCP servers from `~/.codex/config.toml` and has hooks, including a `Stop` hook that can block. Kimi comes later (through Claude Code's `ANTHROPIC_BASE_URL`). The same rule as D7: Codex's plan only runs the unmodified Codex CLI, never B | 26 Oct |
| D16 | **Continue locally on the Mac** by teleporting this cloud session (`claude --teleport <session-id>`). The first eval runs happen there | 26 Oct |

### Open questions (ask Elias)

| # | Question | Why it matters |
|---|---|---|
| Q3 | Where do the `ec-handover-snap` snapshots live on the Mac (look in `~/.claude/settings.json` under `hooks` → `Stop`), and may they become local eval tasks? | E3 and D12: the north star is measured from them |
| Q7 | Which handful of tasks for the first small runs? Decide on the Mac. Suggested: `easy-ease`, `lower-third`, `logo-fly-in`, `parenting`, `fix-layout` | D13 |
| Q8 | Does `codex exec` (Codex without its interface, for scripts) run on the ChatGPT plan, or only with an API key? Check `codex login status` and OpenAI's docs on the Mac | D15 evals |

### Words used here

- **Harness / driver / loop:** the program around the model that sends prompts, runs tools and decides when to stop. Claude Code, Codex CLI and our own Rust loop are three.
- **Policy core:** our rules, written once in Rust, that every driver calls.
- **Hook:** a command a driver runs at a fixed moment, such as before a tool call or when the agent wants to stop. It can add text or block.
- **MCP:** the standard plug that lets any agent use EffectCraft's tools.
- **Eval:** a measured test of the agent. Task + trial (one run) + grader (what scores it).
- **pass^k:** all k runs of a task pass.
- **Trace:** the full record of one run.
- **Handover:** the agent says it's done and gives you the comp.
- **Snapshot:** a saved copy of a comp at a moment.
- **Headless:** run without a window or interface, from a script.
- **API key / credits:** pay-per-use access to a model, separate from a subscription.
- **Teleport:** pull a cloud Claude Code session into your own terminal.

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

### Recommendation (now decision D2)

Build **B and C both**, on **one policy core** (D3), and let the exam decide. A stays a
throwaway spike at most (D4).

```text
                   ┌──────────────── policy core (Rust) ────────────────┐
                   │ taste + lint gate · context packet · approvals ·   │
                   │ budgets + dup calls · pins · trace                 │
                   └───────▲─────────────────────────────▲──────────────┘
                           │                             │
     C: Claude Code driver │ effectcraft-cli hook …      │ direct calls   B: EC agent driver
     (subscription)        │ (Stop, UserPromptSubmit,    │ (API key)      (our loop, Anthropic
                           │  PreToolUse, PostToolUse)   │                 Messages API)
                   ┌───────┴───────┐             ┌───────┴───────┐
                   │  claude (CLI) │             │ effectcraft-  │
                   │  + EC MCP     │             │ cli agent "…" │
                   └───────┬───────┘             └───────┬───────┘
                           └──────────► EffectCraft ◄────┘
                              (app via --bridge, or headless)
```

## 5. The plan

**E0: baseline (first).** Run the exam as it is, on C: 21 tasks × 3 trials, one model, settings
pinned (§7). Record the pass rate, pass^3, mean tool calls, minutes and cost in
`docs/fork/agent-evals.md` (a new file; history appended). Read every failed trace and file the
patterns.

**E1: exam upgrades.**
- `--trials N` with pass@k and pass^k in the report.
- `--driver claude-code|ec-agent`, so both drivers run the same tasks.
- Per-trial cost: `total_cost_usd` from Claude Code's result, token usage from B.
- A taste check shorthand (`{"taste": {comp, layer?}, "maxVerdict": "close"}`) so tasks grade ease
  quality.
- A `notes` field in the report for trace tags.
- Tag every task `capability` or `regression`.

**P1: the policy core.** One Rust module, at L5 with the engine, no model calls in it:
- `gate(project) → ok | findings`: `taste.check` + `render.lint`, with each finding's `fix` call.
- `context(project, client) → text`: `context.packet` plus `agent.notices` plus pins.
- `approve(command, params) → allow | ask(preview) | deny`: destructive commands, claims,
  `expectRevision`.
- `budget(trace) → ok | warn | stop`: tool-call budget, repeated identical calls.
- `record(event)`: one trace format for both drivers, linked to revisions.

Tests use recorded traces, no model.

**C1–C4: the Claude Code driver** (outside the harness, subscription):
- **C1:** `effectcraft-cli hook stop` calls `gate`. It **blocks the stop** with the findings until
  taste and lint pass, at most N times, then lets the agent stop and say what's left.
- **C2:** `hook prompt` (`UserPromptSubmit`) and `hook post-tool` (`PostToolUse`) add the
  `context` text. `hook pre-tool` (`PreToolUse`) calls `approve`. `hook post-tool` also calls
  `budget`.
- **C3:** the setup lives in the repo, so evals are reproducible: `integrations/claude-code/`
  holds `settings.json` (the hooks), `mcp.json`, skills (`SKILL.md`: motion recipes, Elias's ease
  rules, library scenes) and a **critic sub-agent** (`agents/motion-critic.md`: contact sheet +
  `get_motion` + taste results, reports what's off before handover).
- **C4:** the exam runs C with that folder only (`--settings`, `--mcp-config`,
  `--strict-mcp-config`), A/B-testing each piece: with and without the gate, with and without the
  critic.

**B1–B5: the EC agent driver** (inside the harness, API key):
- **B1:** loop, budgets, forced final answer, dup detection, trace, scripted provider; tests only.
- **B2:** Anthropic Messages API client (streaming, tool use, prompt caching);
  `effectcraft-cli agent "…"` against the app (`--bridge`) or headless.
- **B3:** the policy core wired in natively:
  - `gate` as the finish veto;
  - `context` as a `RebuildContext` that replaces stale tool output each step (C can only add
    context);
  - `approve` before each tool call;
  - a steering inbox.
- **B4:** the same skills and critic rubric as C3.
- **B5:** in the exam beside C. Keep B long term only if it beats C clearly on pass^3 and
  post-handover edits (D5).

**U1–U2: app pieces, used by both drivers** (D8):
- **U1: pins.** "Pin for agent" in the viewer, Timeline and Graph Editor: a frame, a drawn region,
  a time range or keys, with an optional note. Read through the `get_pins` MCP tool. The next
  reply carries a `notice` that there are new pins.
- **U2: companion panel.** It reads the live Claude Code transcript
  (`~/.claude/projects/<project>/<session>.jsonl`) or B's trace, and lists each turn with the
  revisions it made (matched through the change feed's client and times). Per turn: jump to the
  comp, before/after in the viewer, undo the turn (per-client undo).

**E2–E5: deeper evals** (as before):
- **E2:** trace analysis (`--analyze`).
- **E3:** local handover tasks graded by the distance to Elias's fix.
- **E4:** multi-turn revision rounds, speech, beat and footage tasks.
- **E5:** a model grader calibrated against Elias's verdicts.

**M1: model comparison.**
- Claude models with `--model` on C (subscription) and on B (key).
- A non-Claude model by pointing C at an Anthropic-compatible endpoint (`ANTHROPIC_BASE_URL`,
  e.g. Kimi, with its own key). Same loop, so it's a fair test.

**Order (D10):** E0 → E1 → P1, then **C1–C4 and B1–B5 in parallel**, then U1, U2 and E2–E5;
M1 (D11: Opus, Sonnet, one non-Claude model) with every comparison. Runs stay small (D13) until a
difference shows.

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

## 7. Subscription, API key and the terms (checked 26 Oct)

From Anthropic's [legal and compliance page](https://code.claude.com/docs/en/legal-and-compliance)
and the [headless docs](https://code.claude.com/docs/en/headless). Recheck before anything large or
hosted: terms change.

- **Subscription sign-in (OAuth)** "is designed to support ordinary use of Claude Code and other
  native Anthropic applications". Interactive Claude Code and `claude -p` on Elias's machine are
  that.
- **Products, including Agent SDK ones, use an API key.** Third parties may not "route requests
  through Free, Pro, or Max plan credentials". That's why B never runs through `claude -p`, and why
  `claude -p` can't be B's model backend: it's a whole agent, not a model endpoint.
- **What the Agent SDK is:** Claude Code's own loop as a library for TypeScript and Python. The
  headless docs call `claude -p` "the Agent SDK via the CLI".
- **Limits.** Pro and Max limits "assume ordinary, individual usage of Claude Code and the Agent
  SDK". The pool is shared with Claude chat, with a 5-hour window and a weekly cap. Large eval runs
  eat into normal work.
- **`ANTHROPIC_API_KEY`** in the environment makes Claude Code bill the key instead of the
  subscription.
- **`--bare`** (the recommended scripted mode: no `~/.claude` hooks, skills, memory or `CLAUDE.md`)
  doesn't use the subscription at all; it needs a key.

Consequences for evals:
- **C on the subscription.** These runs load Elias's own `~/.claude` setup, so pin everything
  that matters: `--settings integrations/claude-code/settings.json`, `--mcp-config` with
  `--strict-mcp-config`, a fixed working folder, and the Claude Code version recorded in the
  report.
- **Fully isolated C runs** (`--bare` or `--restricted`) use the key, like B.

**Other models in Claude Code.** Claude Code talks to any server that speaks Anthropic's API format
(`ANTHROPIC_BASE_URL` plus that provider's token). Some providers, Kimi among them, offer one. It's
Claude Code's loop with their model, billed by them. Useful for M1; it doesn't put Claude models
on the subscription into A or B.

## 8. Claude Code inside the app: what it would gain and lose (decision D8)

**Today:** Claude Code runs in the terminal and its `effectcraft-app` MCP server is bridged to the
running app (`--control 9877`). The agent already sees everything live, has its own cursor and
undo, claims comps, and sees Elias's edits through the change feed and `notice`s. Embedding wouldn't
let it see or do more. It changes who starts `claude` and where its output appears.

| Gain from embedding | Without embedding |
|---|---|
| Show by pointing: a region, frame or range goes with the message (`stream-json` input takes images) | **U1 pins** + `get_pins` |
| Turns linked to revisions: undo a turn, before/after a turn | **U2 companion panel**, reading the transcript file |
| Approvals with a rendered preview, answered in an app dialog (`--permission-prompt-tool` can be an EC MCP tool) | Partly: a `PreToolUse` hook can render the preview; the answer stays in the terminal |
| Exact brief → result → Elias's fixes capture | Mostly: the Stop hook + change feed (`ec-handover-snap`) |
| Other users without a terminal | No; only by embedding, or by B |

**Losses from embedding:**
- **Claude Code's terminal UI.** In `-p` mode, built-in commands that only run in the terminal
  interface aren't available: `/login`, pickers, plan-mode approval, permission dialogs, rewind.
  Whatever we don't rebuild is gone.
- **Two sessions.** It's a second session beside the terminal one that builds EffectCraft.
- **Engineering and upkeep.**
  - Process management, `stream-json` parsing, login detection.
  - Three desktop platforms.
  - Impossible in the web build.
  - The protocol changes between Claude Code versions.
- **Closer to the terms line** (§7).
- **Nothing gained over C.** Still Claude Code's loop: no context reshaping, no other models.

Revisit if the agent is meant to ship to EffectCraft users. B with each user's own key is then
likely the cleaner product.

## History

| Date | Change |
|---|---|
| 26 Oct | First version: map, gaps, evals primer, routes A/B/C, plan |
| 26 Oct | Decisions D1–D9 and open questions; plan reworked for both drivers on one policy core; terms (§7); embedding analysis (§8) |
| 26 Oct | D14–D16 (API credits, Codex driver, teleport to the Mac), glossary, Q3/Q7/Q8 |
| 26 Oct | D10–D13 from Elias's answers (parallel drivers, models, north star, small runs); open questions Q1, Q3, Q6, Q7 |
