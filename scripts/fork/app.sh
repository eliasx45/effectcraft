#!/bin/sh
# Opens the EffectCraft desktop app, building it (and effectcraft-cli, which agents drive) first only
# when the sources changed since the last build (fork rule, docs/fork/agent-builds.md "Building the app"). Agents never build the app after
# merging: merges pile up and the next launch builds once.
#
# The build uses the `local` profile (release without LTO, parallel codegen) in target/local, so a
# rebuild only recompiles what changed. Only one build runs at a time (a lock); a second launch
# waits for it. A running app is never quit or replaced.
#
# Usage: scripts/fork/app.sh [--status | --build-only] [app args]
#   --status      print `running`, `stale` or `ready` and exit
#   --build-only  build if needed, don't launch
# Exit status 130 when the build was stopped (SIGTERM / SIGINT / SIGHUP), 1 when it failed.
set -u
# Started from the Dock, PATH lacks cargo.
PATH="$HOME/.cargo/bin:/opt/homebrew/bin:/usr/local/bin:$PATH"
export PATH

root=$(cd "$(dirname "$0")/../.." && pwd)
target="$root/target/local"
bin="$target/local/effectcraft"
cli="$target/local/effectcraft-cli"   # agents: point EFFECTCRAFT_CLI here, it's rebuilt with the app
stamp="$target/built.stamp"
lock="$target/build.lock"
log="$target/build.log"
mkdir -p "$target"

# The app runs from a frozen copy in $target/run (scripts/frozen-exe.sh), or from $bin when the
# Dock launcher started it.
running() { pgrep -f "^($bin|$target/run/effectcraft-[0-9]+-[0-9]+)( |\$)" >/dev/null 2>&1; }

# Sources newer than the last successful build (any file: a change that doesn't affect the app
# costs one no-op cargo run, then the stamp moves on).
stale() {
    [ -x "$bin" ] && [ -x "$cli" ] && [ -f "$stamp" ] || return 0
    [ -n "$(find "$root/apps" "$root/crates" "$root/assets" "$root/Cargo.toml" "$root/Cargo.lock" -newer "$stamp" -type f -print 2>/dev/null | head -n 1)" ]
}

case "${1:-}" in
--status)
    if running; then echo running; elif stale; then echo stale; else echo ready; fi
    exit 0
    ;;
esac

# Owner-local artwork is pinned outside the checkout; catch accidental replacements before launch.
python3 "$root/scripts/fork/check-owner-icons.py" || exit 1

# Sign the CLI with a FIXED identity after every build. cargo leaves it ad-hoc signed with a new
# identifier each build ("effectcraft_cli-<hash>"), so macOS privacy (TCC) sees a new app every rebuild
# and asks again for Downloads/Documents, which stopped night runs (Elias, 8 Nov 2026: "i NEVER EVER
# want these … they keep happening during night runs"). With one identity, a grant (Full Disk Access)
# survives rebuilds. Never blocks: a codesign that waits on a keychain prompt is killed after 20 s and the
# CLI stays ad-hoc. Override with EFFECTCRAFT_SIGN_ID; EFFECTCRAFT_SIGN_ID=- skips.
sign_cli() {
    id="${EFFECTCRAFT_SIGN_ID:-Apple Development: mohamed elias hamadouche (537WFR6N58)}"
    [ "$id" = "-" ] && return 0
    # Sign a copy and swap it in (new inode): CLIs running the old file keep their image untouched.
    cp -p "$cli" "$cli.signing" || return 0
    codesign -f -s "$id" --identifier com.elias.effectcraft-cli "$cli.signing" >>"$log" 2>&1 &
    sp=$!
    ( sleep 20; kill "$sp" 2>/dev/null ) &
    kp=$!
    if wait "$sp"; then
        mv -f "$cli.signing" "$cli" && echo "signed effectcraft-cli as $id" >>"$log"
    else
        rm -f "$cli.signing"; echo "effectcraft-cli NOT signed (stays ad-hoc)" >>"$log"
    fi
    kill "$kp" 2>/dev/null
    return 0
}

build_pid=""
stop() {
    # Stop the whole build (cargo and every rustc it started), then free the lock.
    if [ -n "$build_pid" ]; then
        kill -TERM -"$build_pid" 2>/dev/null || kill -TERM "$build_pid" 2>/dev/null
        wait "$build_pid" 2>/dev/null
        [ -f "$lock/pid" ] && [ "$(cat "$lock/pid" 2>/dev/null)" = "$$" ] && rm -rf "$lock"
    fi
    echo "build stopped" >>"$log"
    exit 130
}
trap stop TERM INT HUP

build() {
    # One build at a time: mkdir is atomic. A lock left by a process that's gone is cleared.
    while ! mkdir "$lock" 2>/dev/null; do
        owner=$(cat "$lock/pid" 2>/dev/null || echo "")
        if [ -n "$owner" ] && kill -0 "$owner" 2>/dev/null; then
            sleep 2
        else
            rm -rf "$lock"
        fi
    done
    echo "$$" >"$lock/pid"
    # The stamp is taken before the build, so changes merged while it runs make the next launch
    # build again.
    : >"$stamp.new"
    echo "building EffectCraft (local profile) $(date)" >"$log"
    # Job control puts the build in its own process group, so stopping it stops every rustc too.
    set -m
    CARGO_TARGET_DIR="$target" cargo build --profile local -p effectcraft -p effectcraft-cli --manifest-path "$root/Cargo.toml" >>"$log" 2>&1 &
    build_pid=$!
    set +m
    wait "$build_pid"
    status=$?
    build_pid=""
    if [ "$status" -eq 0 ]; then
        sign_cli
        mv -f "$stamp.new" "$stamp"
    else
        rm -f "$stamp.new"
    fi
    rm -rf "$lock"
    return "$status"
}

# The Dock launcher (scripts/fork/launcher.swift) starts EffectCraft itself after the build, so it
# keeps the launcher's privacy permissions: started by it, only build. (Launchers installed before
# it passed --build-only ran this without it and got a second copy of the app.)
case "$(ps -o comm= -p "$PPID" 2>/dev/null)" in
*.app/Contents/MacOS/EffectCraft) set -- --build-only "$@" ;;
esac

if [ "${1:-}" = "--build-only" ]; then
    shift
    # Never replace a running app's binary: its image would no longer match its file, and macOS
    # blocks and rescans every process it starts (scripts/frozen-exe.sh).
    if running; then
        echo "EffectCraft is running: not rebuilding under it" >&2
        exit 0
    fi
    if stale; then build || exit 1; fi
    exit 0
fi

if running; then
    # Never quit or replace a running app (it may hold unsaved work): bring it forward instead.
    echo "EffectCraft is already running" >&2
    exit 0
fi
if stale; then
    build || { echo "build failed, see $log" >&2; exit 1; }
fi
cd "$root" || exit 1
. "$root/scripts/frozen-exe.sh"
nohup "$(frozen_exe "$bin" "$target/run")" "$@" >"$target/app.log" 2>&1 &
exit 0
