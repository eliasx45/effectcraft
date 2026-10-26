# Agent builds on this fork (eliasx45/effectcraft)

> **Since 26 Oct (Elias): work on `main` in the main checkout, no lanes, and no long compiles, tests or builds; he runs them at night.** Write tests but don't run them, commit only your own files straight to `main` and push. Lanes (below) only with his OK, for tasks that heavily overlap. Where this file says otherwise, this note wins.

Fork-only rules. They are not part of upstream (`storytold/effectcraft`); keep this folder out of
upstream pull requests. Where they differ from `CLAUDE.md` / `AGENTS.md` / `docs/testing.md`, they win
on this fork.

## Why

The owner's machine is one M1 Max (10 cores, 64 GB), shared by many agents and sessions at once.
On 10 October 2026, ten agents each running full builds and tests put about 51 compile jobs on the
10 cores (load average 114): every build took roughly ten times longer, and some agents waited
hours. sccache hit 8 of 2,122 compilations (0.4 %), so each worktree compiled the whole dependency
tree from scratch. The fix is to build less per agent, and to make each build cheaper.

## The rules

**While you iterate (every agent):**
- Use `cargo check -p <crate>` (add `--all-targets` when you touch tests) to see whether it compiles.
  It skips code generation and linking, so it's several times cheaper than a build.
- Run only the tests you wrote or touched: `cargo test -p <crate> --lib <filter>`, or one
  integration test binary with a filter (UI tests: one binary, filter by file, see below).
- Don't set `RUSTC_WRAPPER=sccache`: sccache is off on this machine (it only hit for rebuilds in
  the same target dir, and every compile queued behind its one server).
- Cap your jobs: `CARGO_BUILD_JOBS=4` (or `-j 4`). Several agents share 10 cores; more jobs than
  cores make everyone slower, not faster.
- Don't run `cargo xtask ci`, `cargo xtask ci --affected`, a whole crate's test suite or a release
  build per change.

**Before you hand back:** `cargo fmt`, `cargo clippy -p <crates you touched> --all-targets -- -D warnings`,
and your focused tests. Report exactly what you ran.

**Landing your work:**
1. Commit on your branch.
2. Merge the latest `main` into your branch **once**, at the end (merge, don't rebase: a rebase
   replays every commit and, when `main` moves again, means another rebuild). Then `cargo check`
   the crates you touched: no full build, no test suite.
3. Merge into `main`, **push `main`** (the nightly cloud run only sees what's on GitHub), then
   `cargo xtask agents release <slug>` (a lane goes back to `main` with its build folder kept warm).
   Outside a lane: delete your worktree, branch and `target/agent-<name>`.
4. Queue it for upstream: `scripts/fork/queue-pr.sh` (see "Sharing with upstream" below).
5. Stop there. **Don't build the app or the release CLI after merging**, and don't ask another
   session to: see "Building the app" below.

**CI only when Elias says so.** No agent runs `cargo xtask ci` or `ci --affected` on its own,
not even the coordinating session after merging. Elias starts CI himself, often on another
computer.

**Day and night.** During the day, agents write the regression test but don't run it: `cargo
check` and clippy on the touched crates, then merge, saying in the merge message which tests
weren't run. Every night at 01:00 UTC (02:00 Mac time) a cloud routine ("EffectCraft nightly: CI, tests, fixes,
community PRs") does the heavy part. At 01:40 Mac time a LaunchAgent
(`scripts/fork/push-main.sh`, log `~/Library/Logs/EffectCraft push-main.log`) pushes `main` first
in case a merge wasn't pushed: fast-forward only, never other branches. The routine then it merges upstream, runs those tests and the full CI, fixes
main until green, keeps the queued upstream branches current (below), and leaves a morning
summary.

**The watchdog** (`.github/workflows/watchdog.yml`, set up at Elias's request) runs `cargo xtask ci
--affected` on a free GitHub-hosted Linux runner after every push to `main`. It never blocks a push.
A failure shows as the red ❌ on the commit (`gh run list --workflow watchdog.yml`) and in GitHub's email;
with issues turned on it also opens one `watchdog` issue that the next green run closes. If main is
red, fixing it comes before new work.

**Formatting is checked before it lands.** A commit or merge on `main` whose changed Rust files
aren't rustfmt-formatted is refused (`scripts/fork/hooks/`, about a second; installed once with
`git config core.hooksPath scripts/fork/hooks`, shared by every worktree). Run `cargo xtask fmt`
and commit again. On 10 October 2026, four of the watchdog's red runs were only formatting.

## Sharing with upstream (queue at landing)

Finished work goes to storytold/effectcraft when it lands, not in a nightly catch-up (on 10 October
2026 alone, 289 commits landed): every agent that lands shareable work on `main` also queues it as
an upstream PR branch, right after pushing `main`.

```sh
scripts/fork/queue-pr.sh <topic-slug> --title "fix(viewer): …" [--depends upstream/<other>] \
    [--body-file what-why-tested.md] [--update] [--no-check] [--no-push] [--allow-leaks] <commit-or-range>...
```

It builds `upstream/<topic-slug>` from `upstream/main` in a temporary worktree (your checkout is
untouched), cherry-picks the commits with `-x`, drops the fork-only paths in
`scripts/fork/upstream-exclude.txt`, runs `rustfmt --check` and `cargo check` on the affected crates
(`CARGO_BUILD_JOBS=4`, `target/agent-upstream`), pushes the branch to origin and writes
`queue/<topic-slug>.md` (title, branch, depends, then the PR body) on origin's `pr-queue` branch.

- **Title:** upstream's style, `fix(scope): …`, `perf(scope): …`, `feat(scope): …` (for example
  `fix(render-queue): honor height-only locked resize edits`), not the fork's task ids.
- **One theme per PR, and not too small** (Elias, 11 October 2026: no flood of tiny PRs). A PR
  covers one area and one theme, aiming for about 1,500–4,000 changed lines. One small fix on its
  own is too small: if a queued branch with the same theme has no PR yet, add your commits to it
  with `--update` instead of queueing a new slug, and widen its title and body to match. Two
  unrelated areas are still two PRs.
- **Body:** What / Why / How tested. Pass `--body-file` when you can; without it the body is built
  from the commit messages, with a "How tested" placeholder listing their test notes.
- **Builds on fork work that isn't upstream yet:** a cherry-pick that conflicts stops with nothing
  left behind and names the commit. Queue the prerequisite first, then pass
  `--depends upstream/<prerequisite>`: the branch then starts from that branch, so the cherry-pick
  applies. Open a dependent PR only once its prerequisite is merged on storytold; the
  nightly then merges `upstream/main` into the dependent branch so its diff shrinks to its own
  commits.
- **More commits for a queued topic:** `--update` adds them to the branch (never a force push).
- **Fork-only hunks** in shared files (`Cargo.toml`'s `[profile.local]`, `CLAUDE.md` step 5,
  `AGENTS.md` §1.9, `.mcp.json`, `local_assets` calls; the `hunk:` lines of the exclude list) can't
  be dropped by path: the script warns, and stops when added lines name fork-only things. Land them
  as their own commits, so the rest can be queued alone.
- **Exempt:** fork-only material (the exclude list: this folder, `scripts/fork/`, the watchdog,
  lanes, owner-local icons, the Motion Library) and half-done work. Half-done work is queued when
  it's finished.
- **The gap:** `scripts/fork/sync-gap.sh` prints the shareable commits and lines not yet queued or
  upstream, and the task ids with the most (`--json`; the nightly uses `--fail-if-growing <file>`).

**Who opens the PRs.** The Mac scheduled task `effectcraft-open-queued-prs` (Claude app, 10:00 and
18:00; runs when the app is open) opens queued branches as Elias, at most 12 of ours open at once. A
file whose `depends:` branch has no merged PR waits; once it is merged, the task merges storytold
`main` into the dependent branch first so its diff shows only its own commits. It then deletes the
opened `queue/*.md` files on `pr-queue` (commit `queue: opened storytold #…`). The cloud routine
(02:00 and 14:00) can't open PRs on storytold; it keeps the queue green and fills it. Other agents
never open PRs on storytold unless Elias asks in that session.

## Lanes: warm build folders

A new worktree with an empty build folder compiles all ~600 crates (11 minutes on a 4-core cloud
machine, 15–25 under load here), even for a one-line change. Measured on 10 October 2026: a build
folder is reusable from another checkout (third-party crates stay fresh), and the workspace crates
only rebuild because a fresh checkout gives every file a new date.

So agents work in **lanes**: a few permanent worktrees (`../effectcraft-lane-<n>`, 4 by default,
`EFFECTCRAFT_LANES` overrides), each with its own build folder (`target/lane-<n>`).
`cargo xtask agents lane <slug> --task "…"` takes a free lane and switches it to a new branch
`agents/<slug>` from `main` (`--branch` picks another): git only rewrites the files that differ, so
cargo rebuilds only what changed. It prints the `cd` and environment to use. `release` refuses a
lane with uncommitted changes, puts it back on `main` and deletes the branch if it is merged.
`gc` never removes a lane. All lanes busy: wait for one; at most 4 agents build at once, which also
keeps the cores from drowning. The first agent in a new lane builds everything once.

## The MCP servers

`.mcp.json` starts three MCP servers per Claude session through `scripts/mcp.sh`. When the sources
are newer than the CLI it rebuilds it once in the background, with the `local` profile in
`target/local` (shared with the app), not `--release` (thin LTO, one codegen unit, no incremental
reuse: 10–20 minutes of every core, on almost every session start). A linked worktree serves the
main checkout's CLI and never builds its own; Claude cloud sessions don't build it at all.

## Building the app

Nobody builds the app after a merge. It's built when it's opened, once, however many merges
happened in between:

- **Dock:** `~/Applications/EffectCraft.app` (pinned to the Dock). A click opens the app at once
  when nothing changed. When the code changed it shows "Building EffectCraft…" with a Stop button
  and opens the app when the build is done. Stop, closing that window, Cmd-Q or Quit from the Dock
  stop the build (the next click carries on where it stopped). A running app is never quit or
  replaced: the click brings it to the front.
- **Command line:** `scripts/fork/app.sh` (same thing; `--status` prints `running` / `stale` /
  `ready`, `--build-only` builds without opening).
- The same build makes the CLI, `target/local/local/effectcraft-cli`, so agents that drive
  EffectCraft headless (Claude Code, the motion kit) get the current version too. Point
  `scripts/fork/cli.sh` runs it and rebuilds first when the sources changed (same lock and stamp,
  so app and CLI build once between them): point agents there (`EFFECTCRAFT_CLI=…/scripts/fork/cli.sh`,
  or a symlink named `effectcraft-cli` on `PATH`) and they never run a stale CLI.
- Long-running processes never run the build output itself: `scripts/mcp.sh` (MCP servers) and
  `scripts/fork/app.sh` (an app opened from the command line) run a frozen copy in
  `target/local/run/` (`scripts/frozen-exe.sh`: an APFS clone per build, removed once nothing runs
  it), and `app.sh --build-only` doesn't build while the app runs. A rebuild under a running
  process left its image mismatching its file: macOS then blocked and malware-scanned every
  process it started, and seven stale servers froze the Mac (23 Oct 2026). One-shot CLI runs
  still use the build output.
- It builds with `[profile.local]` in the root `Cargo.toml` (inherits `release`; `lto = false`,
  `codegen-units = 16`, `incremental = true`) in `target/local`, one build at a time (a lock), so a
  rebuild only recompiles what changed. Exit status 130 means the build was stopped, 1 that it
  failed (log: `target/local/build.log`). Published releases still use `release`.
- The files: `scripts/fork/app.sh` (the build-and-open logic), `scripts/fork/launcher.swift` (the
  Dock app's window and Stop button) and `scripts/fork/install-dock-app.sh` (builds the launcher
  with `swiftc` into `~/Applications/EffectCraft.app` and adds it to the Dock once). After changing
  the launcher, run the install script again.
- Privacy permissions (Full Disk Access, Files and Folders) belong to `EffectCraft.app`. The
  launcher starts the app itself and stays running, hidden, until the app quits, so one grant
  covers every rebuilt binary. An app started from a terminal or by an agent (`scripts/fork/app.sh`)
  gets that program's permissions instead. Reinstalling the launcher re-signs it: switch
  EffectCraft off and on again in Full Disk Access.

## Cheaper builds

Landed in AG2.16 (details: docs/testing.md, "One test binary per crate").

- UI tests (`crates/ui-egui/tests/*.rs`) compile into **one** test binary instead of one binary per
  file (each of which linked the whole app). Run one file's tests with
  `cargo test -p effectcraft-ui-egui --test ui <file_name>::` (for example `ui_viewer::`). The same
  goes for `effectcraft-host`, `-export` and `-effects` (`--test host`, `--test export`,
  `--test effects`). A new test file needs its `#[path = "../x.rs"] mod x;` line in
  `tests/<binary>/main.rs`; a test fails until it has it.
- Dev and test builds carry line tables only (`debug = "line-tables-only"`; dependencies none):
  panics still show file and line, and linking is faster.
- Later: splitting `crates/engine` (about 68,000 lines in one crate; any edit recompiles it and
  everything above it) would cut rebuilds further.

## Later: a cloud build machine

For large agent fleets, run the agents and CI on a cloud Linux machine with 32–64 cores, and keep
the Mac for the app, After Effects and UI checks. This goes with the plan to speed up CI in the
cloud.

## Owner-local icons

On the owner's machine the app shows After Effects' own icons and cursors (layer-source and
Project item icons, the toolbar, the viewer footer, the timeline trim cursor), extracted from the
owner's licensed install into `~/Library/Application Support/EffectCraft/owner-assets` (outside
every checkout; `EFFECTCRAFT_ICON_REFERENCE` points elsewhere). `crates/ui-egui/src/local_assets.rs`
loads them at runtime. This is the owner-approved exception in `AGENTS.md` §1.9: the artwork is
never committed, embedded, shipped or attached anywhere, code names an asset only by file name, and
every icon keeps its code-drawn fallback, so builds without the folder (CI, other machines,
upstream) look as before.

The owner's `approved-icons.json` pins the local atlas and PNGs by SHA-256. The Mac launcher runs
`scripts/fork/check-owner-icons.py` before building/launching, so missing or changed approved files
produce an actionable error instead of silently replacing the owner's artwork. Machines without an
approved owner set still use the normal fallbacks. Keep the approval file and all artwork outside the
checkout; update the approval only after visually checking an intentional asset change. UI regression
tests use synthetic textures to check timeline atlas cells, glyph sizing, and RGB tint preservation.
