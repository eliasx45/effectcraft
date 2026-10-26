# EffectCraft — instructions for agents

EffectCraft is a clean-room, open-source, pure-Rust motion graphics and visual effects compositor targeting Adobe After Effects parity (and beyond). Native on macOS, Windows, Linux; web via WASM. Sibling of `../photocraft` (Photoshop), `../pdfcraft` (Acrobat), `../vectorcraft` (Illustrator), `../filmcraft` (Premiere), `../lightcraft` (Lightroom) and `../designcraft` (InDesign), with the same conventions.

## Start every session here
`plan/` is maintainer-local (gitignored). Public equivalents: [`ROADMAP.md`](ROADMAP.md), [`docs/`](docs/).
1. **Work on `main`, in this checkout** (Elias, 26 Oct: lanes slow his dev work down). Commit only the files you changed (`git add <paths>`, never `-A`), and leave others' uncommitted work alone. Lanes/worktrees (`cargo xtask agents lane`) only when Elias approves, for tasks that heavily overlap; `cargo xtask agents status` shows who else is working.
2. Read the task in `plan/execution-plan.md` §3, the relevant section of `plan/architecture.md`, and the crate README/docs you touch. Feature ids: `plan/aftereffects/feature-catalog.md`; effects: `plan/aftereffects/effects-list.md`; UI reference: `plan/aftereffects/README.md`.
3. Read [`docs/gaps.md`](docs/gaps.md): every known shortfall, ranked, and the prioritised workstreams; the numbers are in [`docs/target-app-parity.md`](docs/target-app-parity.md). Fidelity, reliability and stability (G1–G3) come before new features.
4. Follow the autonomous operation protocol (`plan/execution-plan.md` §7). Don't stop to ask.
5. **On the eliasx45 fork:** read [`docs/fork/agent-builds.md`](docs/fork/agent-builds.md) before building anything. **No long compiles, tests or builds** (Elias runs them himself at night): write the tests, don't run them, and say "tests not run" in the commit. Commit straight to `main` and push. Never build the app or release CLI after merging (it builds itself on launch: `scripts/fork/app.sh`), and never run CI (`cargo xtask ci` / `--affected`) unless Elias asks. These rules override the per-change gates below.

## Non-negotiables
**Read [`AGENTS.md`](AGENTS.md) first; its rules override everything here.** No Adobe assets, every asset licensed + attributed (`cargo xtask assets`).
- **Never crash** (AGENTS.md "Never crash"; outranks feature work: a crash loses people's work). Non-test code has no `unwrap()`/`expect()`/`panic!`/`unreachable!`/`todo!`/`unimplemented!` and no `unsafe`: return `Result` and use `?`, `ok_or`, `let … else`. Input-derived indices, slices and arithmetic use `get()` and checked/saturating math; recursion is depth-limited; locks handle poisoning. The app's last-resort guard turns an escaped panic into an error and keeps the document. Every crash fix gets a regression test. Standard: [craftrules `standards/never-crash.md`](https://github.com/storytold/craftrules/blob/main/standards/never-crash.md).
- **Clean-room.** Behaviour and public docs only. No GPL/LGPL/AGPL code. ffmpeg only as an external test oracle.
- **Pure Rust.** Media codecs come from FilmCraft crates (git deps behind `crates/media`, see `plan/adr/0001`).
- **Layering** (`cargo xtask layers`): nothing below L5 depends on egui/eframe/winit/rfd/cpal; L0–L4 build for wasm32.
- **Exact time:** `effectcraft_time::Tick` (254 016 000 000/s). Keyframe times are **layer time**.
- **Property tree:** everything animatable is a `Property` in the layer's `PropGroup` tree, addressed by paths (`transform/position`, `effects/#1/blurriness`, `@uid`).
- **Everything is a command** (`crates/engine`): id, label, menu path, shortcut, params, enabled(), run(). UI, CLI, control channel and MCP dispatch by id.
- **Everything is agent-drivable:** every interactive widget registers an automation id; UI state is serde.
- **Community links** (Help menu, About, header Discord button): https://discord.gg/artcraft, https://getartcraft.com, https://getartcraft.com/apps/effectcraft, https://github.com/storytold/effectcraft.
- **Quality gates** before every commit: `cargo xtask ci` (fmt, clippy -D warnings, tests, layers, assets, wasm). Agents: `cargo xtask ci --affected` per change (only the crates the change can affect, see `docs/testing.md`); full `cargo xtask ci` before release.
- **Integration tests are one binary per crate** where a crate has many (`ui-egui`: `--test ui`; `host`, `export`, `effects`: `--test host|export|effects`): `tests/<name>/main.rs` declares each `tests/*.rs` as a module (`#[path = "../x.rs"] mod x;`, a test fails until a new file is declared). One file's tests: `cargo test -p effectcraft-ui-egui --test ui ui_viewer::`.
- **Commits:** one task id per commit (`M6.3: trim paths`). Only green states.
- parity.md cells hold the current state; append history to docs/parity-log.md (union-merged).

## Running and looking at the app
- `cargo run -p effectcraft -- --control 9877` opens the desktop app with the JSON-lines control server (`docs/control-protocol.md`).
- For UI work, **look at the result**. Headless (no window, works while the user is busy on the machine): `cargo run -p effectcraft-ui-egui --example snapshot -- --out ui.png --step '{"method":"engine.execute","params":{"command":"layer.select","params":{"layers":["#2"]}}}'` (steps are control-channel requests; `--script steps.jsonl`; `{"method":"snap","params":{"path":…}}` for intermediate shots). Or drive the live app via the control channel and take `ui.screenshot` (needs the window visible on screen).
- After Effects reference: drive AE via ExtendScript (`osascript -e 'tell application "Adobe After Effects 2026" to DoScriptFile "x.jsx"'`) and capture only AE's window (`screencapture -l <windowid>`); keep captures in `plan/aftereffects/ref/` (gitignored), never commit them, never capture the Home screen.
- Parallel agents work on `main` too (above); a lane (`cargo xtask agents lane`) only with Elias's OK for heavily overlapping tasks. Don't set `RUSTC_WRAPPER=sccache` (off on this machine, see `docs/fork/agent-builds.md`). `cargo xtask agents claim` gives you `target/agent-<slug>` and a free control port (`EFFECTCRAFT_CONTROL_PORT`; 9877 is the owner's app); `cargo xtask agents release <slug>` when done. Many agents share 10 cores: build with `CARGO_BUILD_JOBS=4` and `nice -n 10`. `cargo xtask agents gc` lists merged + clean worktrees and orphan target dirs (dry run; `--yes` removes them).
