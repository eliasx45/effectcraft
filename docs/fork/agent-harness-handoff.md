# Handoff: agent harness and evals (from the cloud session of 26 Oct 2026)

Paste everything below the line into the new local Claude Code session on Elias's Mac. It replaces
the cloud session `session_019dVTnXxeciwrHQHbXRjsTK` completely. The full record is in
[`agent-harness.md`](agent-harness.md); this file is the briefing on top of it.

---

You're taking over from a cloud Claude Code session that worked with Elias on 26 Oct 2026. It
fully replaces that session. Everything we discussed is summarised here. Read this whole message,
then read `docs/fork/agent-harness.md` (the plan and decisions log) before doing anything.

## 1. Who you're working with, and how

- **Who:** Elias owns the eliasx45 fork of EffectCraft (a Rust After Effects-class compositor).
  He's **new to evals and agent harnesses but wants to go deep**.
- **Explain terms in brackets:** explain every concept concisely in brackets the first time you
  use it in a reply, every time, e.g. "pass^3 (all 3 runs of a task pass)". He asked for this
  explicitly.
- **Keep the decisions log current:** he forgets details. When he decides something, add it to
  the **Decisions** table (§0 of `docs/fork/agent-harness.md`, rows D1…), add a line to its
  History table, and commit. Move answered questions out of "Open questions".
- **Asking:** ask him when a decision is genuinely his. Otherwise pick the sensible default and
  say so.
- **Pushing back:** when he proposes something, say plainly what it gains and loses. He asked for
  pros and cons "pushed to their logical extreme".
- **How he writes:** quickly, with typos. Read for intent.

## 2. Where things are

- **Repo:** `eliasx45/effectcraft`, branch `claude/effectcraft-fork-last-push-t552mc`. It's
  `main` at `08a8478` plus four doc-only commits (`HA0: …`). Not merged to `main`. `main`'s last
  push was 25 Oct 22:31 UTC (`08a8478`, HG1).
- **Files from the cloud session:**
  - `docs/fork/agent-harness.md`: the map, gaps, evals primer, routes, decisions D1–D16, open
    questions, plan, terms, the embedding analysis and a glossary.
  - `docs/fork/agent-harness-handoff.md`: this briefing.
- **`docs/fork/*` is fork-only:** it's listed in `scripts/fork/upstream-exclude.txt`, so it's
  never queued upstream.
- **Repo rules to follow (CLAUDE.md, AGENTS.md, `docs/fork/agent-builds.md`):**
  - Lanes (`cargo xtask agents status` / `lane`).
  - `CARGO_BUILD_JOBS=4 nice -n 10`; `cargo check` plus focused tests while iterating.
  - Never run `cargo xtask ci` unless Elias asks.
  - Never build the app or release CLI after merging; use `scripts/fork/app.sh` /
    `scripts/fork/cli.sh`, which rebuild themselves.
  - Land with a merge of `main` into the branch (no rebase).
  - Never-crash rules: no `unwrap`/`expect`/`panic`/`unsafe` in non-test code.
  - Everything is a command.
  - Commit messages carry a task id. The cloud session used `HA0` for the harness docs; use
    `HA<n>`, or the plan's ids (E0, E1, P1, C1…, B1…, U1…).
- **Also in the cloud session:** `eliasx45/figcraft` and `eliasx45/ghostex` were attached.
  Ghostex had nothing from Elias: its `main` stops at upstream's 30 May release; there are
  branches `elias/fix-ios-cli-json`, `learn-mode`, `update-tab-hotkeys`,
  `feature/codexbar-titlebar-usage` and `add-hover-to-address-bar`, unread. Elias said that
  context doesn't matter: the focus is the EffectCraft harness.

## 3. What happened in the session, in order

1. **Last push.** Elias asked when the fork was last pushed: 25 Oct 22:31 UTC.
2. **Yesterday's work.** He asked what we did yesterday on the harness. The cloud session had no
   memory of it and summarised the commits since 24 Oct instead:
   - the harness steps: HP1, HS1, HU1, HU2, HT1, HT2, HG1, SX7;
   - lots of GPU, decode, proxy, embedding and fidelity work.
3. **agentloop.** He asked to map our state and learn from
   [smhanov/agentloop](https://github.com/smhanov/agentloop).
   - The cloud session read its README and `hooks.go` (Go, MIT, about 6k lines).
   - It inventoried the EC harness.
   - It wrote `docs/fork/agent-harness.md`.
4. **His goal.** An EC-native agent for **video editing** (not code CI), deep evals, and the
   pros and cons of each route.
5. **Subscription.** He asked whether A or B can use his Claude subscription. Researched; answer
   in §6.
6. **Embedding.** He asked whether putting Claude Code inside the app gains anything.
   Investigated; verdict in §7 (not now).
7. **Both, on a key.** He decided to build **both inside the harness and outside it** to compare,
   and is fine testing with an API key.
8. **Questions answered.** Then: budget, Codex, models, north star, teleport.
9. **Handoff.** He asked for this briefing.

## 4. What the EC harness already has (verified in code, 26 Oct)

Today **Claude Code is the loop**. EffectCraft provides the tools, senses and judgement over MCP
(the standard plug that lets agents use our tools).

- **Hands:** about 70 MCP tools in `crates/automation/src/tools.rs` (`execute_command`,
  `command_batch`, `run_script`, `set_property`, `add_keyframe`, `animate_layer`, `title_card`,
  `lower_third`, `render_*`, `ui_*`, …), plus the CLI and the JSON-lines control channel.
  - Per-client cursor and undo, `expectRevision`, `comp.claim`.
  - `.mcp.json` servers: `effectcraft` (headless), `effectcraft-ui` (offscreen app),
    `effectcraft-app` (bridged to the running app on `--control 9877`).
- **HP1 plumbing:** tools accept each other's argument names; `run_script {file}`; `get_result`
  by id; failing scripts roll back and report the expressions they broke; short replies.
- **HS1 senses:** ease readout in words, timecode in and out (`6:20`, `200f`), the change feed
  (`edit_feed`, who changed what), `agent.notices` (a one-time notice when the user changed
  things), `context.packet`, `layer.related`.
- **Eyes:** `render_frame(s)`, contact sheets, `render_compare`, `get_layout`, `screenshot`,
  `get_motion` (HT2: rig nulls, `movedBy`), audio, speech maps.
- **Judgement:**
  - `render.lint` / `lint_frame`, with HU1 adding text-off-centre, nested-cut-off and
    exposed-edge;
  - **HT1 taste check** (`crates/engine/src/taste.rs`): every keyed move is judged against
    Elias's ease profile (5 criteria he approved on 25 Oct, from 730 moves plus 101
    post-handover edits). Commands `taste.check/apply/profile/propose/approve/learn/graph`; the
    lint rule `off-taste`; MCP `taste_check`, `ease_graph`.
- **Reuse (HU2):** `library.scene` applies Motion Library scenes by a loose name.
- **Attribution (HG1):** a `client` next to `command` runs as that client; feed entries name
  their comps.
- **Outside the repo, on the Mac** (in `~/.claude`, not visible to the cloud session):
  - `ec-handover-snap`, a Claude Code **Stop hook** that snapshots comps an agent hands over,
    before Elias fixes them;
  - `ec-comp-diff`.
- **Evals that already exist:** the **agent exam**.
  - `evals/agent/` holds 21 tasks (JSON, schema in `evals/agent/schema.json`).
  - Grading is deterministic, through `exam.check` (`crates/engine/src/exam.rs`).
  - `tests_exam::every_task_is_solvable` proves every task can be solved.
  - The runner is `xtask/src/agent_exam.rs`: `cargo xtask agent-exam --list | --validate |
    --check P --task ID | --run [--tasks …] [--out DIR] [--minutes N] [--max-tool-calls N]
    [--model M]`. It runs `claude -p … --mcp-config mcp.json --strict-mcp-config --output-format
    stream-json --verbose --max-turns … --allowedTools "mcp__effectcraft__*"` per task.
  - **No result has ever been recorded**: there's no baseline.

**Gaps found:**
- no loop of our own, so lint and taste are advice the model can ignore;
- no baseline;
- tasks are synthetic and one-shot;
- the handover pairs aren't used as evals;
- no trace analysis;
- one trial per task;
- one vendor.

## 5. agentloop, and the three routes

**agentloop's ideas** (we use the design only, clean-room, no code copied):
- **One `Hooks` struct.** `BeforeGenerate` lets you steer or stop. `RebuildContext` replaces what
  the model sees for one call. `AfterGenerate` is the **finish veto** (refuse to let the run
  end). `BeforeToolCall` can rewrite, veto or wait for approval. `AfterToolCall`, `OnDone` and
  `OnError` observe.
- **Bounded runs:** a tool budget with a forced final answer, a step ceiling, duplicate-call
  detection, a watchdog.
- **Tool errors are fed back** to the model to repair.
- **Skills in `SKILL.md` folders,** the same format as Claude Code.
- **A full trace per run.**
- **Scripted fake providers** for tests.

**The routes:**
- **A, Go sidecar on agentloop:** fast and multi-model, but a second language in a pure-Rust
  project, sees EC only through JSON, and can't ship in the app or browser. At most a spike.
- **B, our own Rust loop** (`effectcraft-agent`, L5/L6, needs HTTP so it stays out of L0–L4 and
  wasm). The highest ceiling:
  - the engine itself refuses a handover that fails taste or lint;
  - the context is rebuilt from the live document every step;
  - runs replay exactly through the command journal;
  - it can ship in the app, CLI and web.
  
  Costs: weeks of work, and we maintain our own Anthropic client. **Needs an API key.**
- **C, Claude Code as the loop:** EC's MCP plus Claude Code hooks (`Stop` as the finish veto,
  `UserPromptSubmit`/`PostToolUse` for context, `PreToolUse` for approval), skills and a critic
  sub-agent. Cheap and strong. But Claude only, it can only *add* context (no `RebuildContext`),
  and it isn't ours to ship.

## 6. Subscription, API key and terms (researched, with sources in `agent-harness.md` §7)

- **Subscription sign-in** "is designed to support ordinary use of Claude Code". Interactive
  Claude Code and `claude -p` (headless: run from a script without the interface) on his machine
  are fine.
- **Products, including Agent SDK ones, need an API key.** Agent SDK = Claude Code's loop as a
  TypeScript/Python library; the docs call `claude -p` "the Agent SDK via the CLI". Third parties
  may not route requests through Pro/Max credentials. So **B never runs through `claude -p` or
  the subscription.** Technically `claude -p` is a whole agent, not a model endpoint, so it would
  be a bad backend anyway.
- **Limits:** Pro/Max limits assume "ordinary, individual usage". The pool is shared with Claude
  chat (a 5-hour window plus a weekly cap).
- **`ANTHROPIC_API_KEY` in the environment** makes Claude Code bill the key instead of the
  subscription. Keep it unset for subscription runs.
- **`--bare`** (the recommended scripted mode, which skips `~/.claude` hooks, skills, memory and
  `CLAUDE.md`) **doesn't use the subscription**; it needs a key.
- **A correction from mid-session:** non-bare `claude -p` loads Elias's own `~/.claude` setup.
  So exam runs on the subscription depend on his personal config, **including the
  `ec-handover-snap` Stop hook**. Pin settings (`--settings`, `--mcp-config` +
  `--strict-mcp-config`, a fixed folder, the Claude Code version recorded in the report) for
  reproducible evals. Fully isolated runs (`--bare` / `--restricted`) need the key.
- **Other models inside Claude Code:** `ANTHROPIC_BASE_URL` plus a provider token works for
  Anthropic-compatible endpoints (e.g. Kimi). That's Claude Code's loop with their model, billed
  by them.
- **Codex CLI** (OpenAI's terminal agent):
  - reads MCP servers from `~/.codex/config.toml` (`[mcp_servers.<name>]`, or `codex mcp add`);
  - has hooks (`.codex/hooks.json`, possibly behind a feature flag depending on version),
    including a `Stop` hook that can block with `"decision": "block"`;
  - runs on his ChatGPT/Codex plan via `codex login`.
  
  **Unverified:** whether `codex exec` (scripted mode) uses the plan or needs an API key.

## 7. Claude Code inside the app: decided NOT now (D8)

- **What exists today:** the bridged MCP (`effectcraft-app`) already lets the agent see
  everything live, with its own cursor and undo, claims, the feed and notices. Embedding wouldn't
  let it see or do more.
- **Real gains from embedding,** and how to get each without embedding:
  - Show by pointing (a region, frame or range sent with the message) → **U1 pins**.
  - Turns linked to revisions (undo a turn, before/after a turn) → **U2 companion panel**, which
    reads the transcript `~/.claude/projects/<project>/<session>.jsonl`.
  - Approvals with a rendered preview (`--permission-prompt-tool` can be an EC MCP tool) →
    partly, with a hook.
  - Exact handover capture → mostly, with the Stop hook and the change feed.
  - Shipping to other users → only embedding or B gives this.
- **Losses:**
  - Claude Code's terminal UI: `/login`, pickers, plan-mode approval, permission dialogs and
    rewind aren't available in `-p`.
  - A second session.
  - Process management and `stream-json` parsing on three platforms; impossible in the web build;
    the protocol changes between Claude Code versions.
  - Closer to the terms line.
  - No gain over C.
- Revisit only if the agent ships to users. B with each user's own key is then the cleaner
  product.

## 8. Decisions so far (the authoritative copy is §0 of `agent-harness.md`)

| # | Decision |
|---|---|
| D1 | Goal: an agent that edits video in EffectCraft, measured by evals of editing quality (not code checks) |
| D2 | Build both and compare: **C** (Claude Code + EC MCP/hooks/skills, subscription) and **B** (our own Rust loop, API key), on the same exam |
| D3 | **One policy core in Rust, two drivers.** The rules (taste/lint gate, context, approvals, budgets, trace) are written once. Claude Code reaches them through `effectcraft-cli hook …`; B calls them directly |
| D4 | agentloop is a design reference only (clean-room); route A at most a throwaway spike |
| D5 | An API key is fine for testing B; keep it long term only if B is clearly better |
| D6 | Claude models first; others only for comparison |
| D7 | The subscription only runs unmodified Claude Code; never B or A; `ANTHROPIC_API_KEY` unset for subscription runs |
| D8 | Don't embed Claude Code in the app now; build **pins** (U1) and a **companion panel** (U2) instead |
| D9 | Real projects stay local, never in git or anywhere the agent can read |
| D10 | **Build C and B in parallel** after the baseline (E0) and the policy core (P1) |
| D11 | Models to compare: **Opus, Sonnet** and one non-Claude model; not Haiku for now |
| D12 | **North star: Elias's edits after handover.** Fewer property changes he makes to a comp the agent handed over = better. Pass^3, cost and minutes are secondary |
| D13 | **Small eval runs to start:** a handful of tasks × 3 trials per driver and model, then decide whether to scale |
| D14 | **API budget: $200 of promotional API credits** (check their expiry in the Claude Console) |
| D15 | **A third driver: Codex CLI on his ChatGPT/Codex plan**, on the same EC MCP tools and the same policy core through Codex hooks. Kimi later, through Claude Code's `ANTHROPIC_BASE_URL`. Same rule as D7: his Codex plan only runs the real Codex CLI |
| D16 | Continue locally on the Mac (this session); the first eval runs happen there |

## 9. Open questions to ask him (one at a time, explaining the terms)

- **Q3:** where does `ec-handover-snap` save its snapshots (look in `~/.claude/settings.json` →
  `hooks` → `Stop`, and the script it calls)? May they become **local, uncommitted** eval tasks?
  They measure the north star (D12). Read the hook script yourself on the Mac and explain to him
  what it does.
- **Q7:** which tasks for the first small run? Suggested: `easy-ease`, `lower-third`,
  `logo-fly-in`, `parenting`, `fix-layout`.
- **Q8:** does `codex exec` run on his ChatGPT plan or only with a key? Check `codex --version`,
  `codex login status` and OpenAI's current docs together with him.
- **Not settled:** the non-Claude model for later (Kimi named), and the promo credits' expiry.

## 10. The plan (details in `agent-harness.md` §5), and what to do first

**Order (D10):** E0 → E1 → P1, then C1–C4 ∥ B1–B5 (∥ = in parallel) and the Codex driver
(D15), then U1, U2 and E2–E5, with M1 (model comparison) alongside each.

| Id | What |
|---|---|
| E0 | **Baseline:** the chosen tasks × 3 trials on C (Claude Code, Opus, subscription), settings pinned; record pass rate, pass^3, tool calls, minutes and cost in a new `docs/fork/agent-evals.md` with a history; read every failed trace |
| E1 | Exam upgrades: `--trials N` with pass@k/pass^k, `--driver claude-code\|codex\|ec-agent`, per-trial cost, a `taste` check shorthand, `notes`, capability/regression tags |
| P1 | The **policy core**, at L5, no model calls: `gate` (taste.check + render.lint, with each finding's `fix`), `context` (context.packet + notices + pins), `approve` (destructive commands, claims, expectRevision), `budget` (tool calls, repeated identical calls), `record` (one trace format linked to revisions). Tests use recorded traces |
| C1–C4 | The Claude Code driver: `effectcraft-cli hook stop` blocks the stop until the gate passes (at most N times); `hook prompt` / `hook post-tool` / `hook pre-tool`; the setup in the repo under `integrations/claude-code/` (settings.json, mcp.json, skills, `agents/motion-critic.md`); the exam runs C from that folder only and A/B-tests each piece |
| Codex | The same as C with Codex hooks and `~/.codex/config.toml` (or a project `.codex/`) |
| B1–B5 | Our own loop: B1 loop, budgets, forced final answer, dup detection, trace, scripted provider; B2 Anthropic Messages client (streaming, tool use, prompt caching) and `effectcraft-cli agent "…"`; B3 the policy core wired in (gate as finish veto, `RebuildContext` with a fresh context, approvals, a steering inbox); B4 shared skills and critic rubric; B5 in the exam beside C |
| U1 | Pins: "Pin for agent" in the viewer, Timeline and Graph Editor (frame, region, range, keys, note), `get_pins` MCP tool, a `notice` on new pins |
| U2 | Companion panel: reads the Claude Code transcript or B's trace; each turn with its revisions; jump, before/after, undo the turn |
| E2–E5 | Trace analysis (`--analyze`); local handover tasks graded by the distance to Elias's fix; multi-turn, speech, beat and footage tasks; a model grader calibrated against his verdicts (≥ 80 % agreement) |
| M1 | Opus vs Sonnet (`--model`), Codex/GPT (D15), Kimi later |

**What to do first on the Mac:**
1. `git status` in the effectcraft checkout. Make sure you're on
   `claude/effectcraft-fork-last-push-t552mc` and it's up to date. Read
   `docs/fork/agent-harness.md`.
2. Ask Q3 and Q7. Read the `ec-handover-snap` script and explain it to him.
3. Check `env | grep ANTHROPIC_API_KEY` is empty for subscription runs. Check `claude --version`.
4. **E0:** the runner has no `--trials` yet, so run it three times with different output folders.
   Point `EFFECTCRAFT_CLI` at `scripts/fork/cli.sh` so it doesn't build a separate release CLI:

   ```sh
   export EFFECTCRAFT_CLI="$PWD/scripts/fork/cli.sh"
   for t in 1 2 3; do
     cargo xtask agent-exam --run --tasks easy-ease,lower-third,logo-fly-in,parenting,fix-layout \
       --model opus --out target/agent-exam/e0-opus-$t
   done
   ```

   Before running, read `xtask/src/agent_exam.rs`. Check:
   - whether `AGENT_EXAM_CLAUDE_ARGS` should add `--settings` with an empty or pinned hooks file,
     so his personal `~/.claude` hooks (`ec-handover-snap`) don't run inside exam trials;
   - whether `cli.sh` works as the MCP `command` (it's a shell script).
   
   Tell Elias what each run will use from his subscription before starting.
5. Record the results, read the failed traces (`stream.jsonl`), write `docs/fork/agent-evals.md`,
   update the decisions log, commit, push.
6. Then E1 and P1, and start C1 (the Stop hook gate) and B1 in parallel (D10), each in its own
   lane.

## 11. Things that aren't verified yet: check before relying on them

- Whether a Claude Code `PreToolUse` hook can **rewrite** tool arguments, or only allow or deny
  (check the hooks docs).
- `codex exec` auth on the ChatGPT plan (Q8); the Codex hooks' current format and flag for his
  version.
- The promo credits' expiry date.
- Whether `scripts/fork/cli.sh` works as the exam's MCP command.
- Teleport with a session that had three repositories attached (this file exists so it doesn't
  matter).
- The terms were read on 26 Oct from
  https://code.claude.com/docs/en/legal-and-compliance and
  https://code.claude.com/docs/en/headless. Recheck before anything large or hosted.
