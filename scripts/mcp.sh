#!/bin/sh
# Starts the EffectCraft MCP server for `.mcp.json` without making the client wait for a compile.
#
# The CLI is built with the `local` profile (release without LTO, parallel codegen, incremental) in
# target/local when the workspace defines it, sharing that folder with the desktop app's build
# (scripts/fork/app.sh), so a rebuild only recompiles what changed. Without that profile it falls
# back to `--release` in target/mcp.
#
# - Sources newer than the binary: the existing binary starts at once and one rebuild runs in the
#   background (a lock; a lock left by a build that died is cleared). Reconnect the MCP server
#   (Claude Code: /mcp) to pick it up.
# - A linked worktree serves the main checkout's binary and never builds its own (a cold build per
#   worktree cost 10–20 minutes of every core).
# - No binary yet: the first start compiles in the foreground. Claude Code cloud sessions
#   (CLAUDE_CODE_REMOTE) and EFFECTCRAFT_MCP_BUILD=0 skip that and exit instead.
#
# Usage: scripts/mcp.sh [effectcraft-cli mcp arguments], e.g. `scripts/mcp.sh --demo` or `scripts/mcp.sh --ui --demo`.
set -u

root=$(cd "$(dirname "$0")/.." && pwd)
# The main checkout: the parent of the shared .git dir (the same path in a linked worktree).
main=$(cd "$root" && git rev-parse --path-format=absolute --git-common-dir 2>/dev/null) && main=$(dirname "$main") || main=$root

if grep -q '^\[profile\.local\]' "$main/Cargo.toml" 2>/dev/null; then
    target="$main/target/local" profile=local
else
    target="$main/target/mcp" profile=release
fi
bin="$target/$profile/effectcraft-cli"
lock="$target/mcp-build.lock"
log="$target/mcp-build.log"

build() {
    CARGO_TARGET_DIR="$target" nice -n 10 cargo build --quiet --profile "$profile" -p effectcraft-cli --manifest-path "$main/Cargo.toml"
}

# One background build at a time: mkdir is atomic; the lock holds the background build's pid.
take_lock() {
    if ! mkdir "$lock" 2>/dev/null; then
        owner=$(cat "$lock/pid" 2>/dev/null || echo "")
        if [ -n "$owner" ] && kill -0 "$owner" 2>/dev/null; then
            return 1
        fi
        # No pid yet: another server (all three start together) took it a moment ago.
        if [ -z "$owner" ] && [ -n "$(find "$lock" -maxdepth 0 -mmin -1 2>/dev/null)" ]; then
            return 1
        fi
        rm -rf "$lock"
        mkdir "$lock" 2>/dev/null || return 1
    fi
    return 0
}

mkdir -p "$target"
serve=$bin
# Until the first build in this folder exists, an older one (the earlier target/mcp release build) serves.
old="$main/target/mcp/release/effectcraft-cli"
[ ! -x "$bin" ] && [ -x "$old" ] && serve=$old

stale() {
    [ "$serve" != "$bin" ] || [ -n "$(find "$main/crates" "$main/apps/effectcraft-cli" "$main/Cargo.toml" "$main/Cargo.lock" -newer "$bin" \( -name '*.rs' -o -name '*.toml' -o -name '*.lock' -o -name '*.json' -o -name '*.wgsl' \) -print 2>/dev/null | head -n 1)" ]
}

if [ ! -x "$serve" ]; then
    if [ "$main" != "$root" ] || [ -n "${CLAUDE_CODE_REMOTE:-}" ] || [ "${EFFECTCRAFT_MCP_BUILD:-1}" = 0 ]; then
        echo "effectcraft mcp: no effectcraft-cli build at $bin; build it in the main checkout (scripts/mcp.sh there) and reconnect" >&2
        exit 1
    fi
    echo "effectcraft mcp: first start, compiling effectcraft-cli (if the client times out, wait for this and reconnect)" >&2
    build >"$log" 2>&1 || { cat "$log" >&2; exit 1; }
elif [ "$main" = "$root" ] && stale; then
    # Serve the current binary now, rebuild once in the background.
    if take_lock; then
        echo "effectcraft mcp: sources changed, rebuilding in the background; reconnect to use the new build" >&2
        (
            build >"$log" 2>&1
            rm -rf "$lock"
        ) </dev/null >/dev/null 2>&1 &
        echo "$!" >"$lock/pid" 2>/dev/null
    fi
fi

# Run a frozen copy: the background rebuild above (or the app's) must never replace a running
# server's image (scripts/frozen-exe.sh says why).
if [ -f "$root/scripts/frozen-exe.sh" ]; then
    . "$root/scripts/frozen-exe.sh"
    serve=$(frozen_exe "$serve" "$target/run")
fi
exec "$serve" mcp "$@"
