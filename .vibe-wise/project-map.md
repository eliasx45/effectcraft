# Project Map

## Purpose
EffectCraft: a clean-room, open-source, pure-Rust motion graphics and visual effects compositor (After Effects class). Native on macOS, Windows and Linux; web via WASM. Elias edits client videos in it, and agents build scenes in it.

## Requirements
- Never crash (no unwrap/expect/panic in non-test code).
- Everything is a command; every widget is agent-drivable.
- Exact time (`effectcraft_time::Tick`); keyframe times are layer time.
- Elias, 31 Oct 2026: smooth playback; agents build and he steers; AE parity can be dropped.

## Components
- `crates/engine`: commands, session, lint, recipes (`commands/recipe.rs`), about 134k lines.
- `crates/project`: the document (comps, layers, property tree).
- `crates/render`, `crates/gpu`, `crates/effects`: drawing frames, about 100k lines together.
- `crates/text`, `crates/media` and the encoder crates: text and video.
- `crates/ui-egui`: the desktop UI. `crates/host`, `crates/embed`: running the engine inside other apps (Ghostex, FigCraft).
- MCP servers (`effectcraft-app`, `-remote`), the CLI and the JSON-lines control channel (port 9877).

## Main Flow
Elias / agents --(UI, MCP, CLI, control channel)--> engine commands --(edit)--> project document (property tree) --(render)--> GPU frames --> viewer, cards in Ghostex/FigCraft, export

Agents today: skill `effectcraft-motion` → JSX script with `app.run("command.id", …)` per step (route 1), or recipe JSON `comp.recipe` / `comp.fromRecipe`, which builds a new comp (route 2) → lint → render → compare.

Proposed (AI, not chosen): a scene file that is edited in place, with timing tied to words and eases and presets by name. Unknown: its shape, and whether the file or the comp is the source of truth.

## Data and Trust Boundaries
- Projects are `.ecproj` files on disk. Owner-local assets live in ~/Library/Application Support/EffectCraft/owner-assets.
- The control channel listens locally; several agents share one project, and `comp.claim` reserves a comp.

## Build and Deployment
- `cargo run -p effectcraft -- --control 9877` (desktop app with the control server).
- On the fork, agents do not run long builds or tests; a 01:00 night run tests queued commits.

## Unknowns
- Where the playback lag comes from exactly (not measured; parked by Elias on 31 Oct 2026).
- Which subsystem a rewrite should replace first.
- How the scene file should look, and how Elias's hand edits flow back into it.
