#!/bin/sh
# Runs effectcraft-cli, rebuilding it first when the sources changed (fork). It shares app.sh's build:
# the same `local` profile in target/local, the same lock and stamp, one cargo run for app + CLI, so a
# merge is compiled once whichever of the two is used first. Point agents here instead of a copied
# effectcraft-cli (e.g. EFFECTCRAFT_CLI=…/scripts/fork/cli.sh): it is never stale.
#
# Usage: scripts/fork/cli.sh <effectcraft-cli args>   (exit status: the CLI's, or 1 when the build failed)
set -u
root=$(cd "$(dirname "$0")/../.." && pwd)
target="$root/target/local"
cli="$target/local/effectcraft-cli"
# Same test as app.sh's stale() (its --status says `running` whenever the app is open, stale or not).
stale() {
    [ -x "$cli" ] && [ -x "$target/local/effectcraft" ] && [ -f "$target/built.stamp" ] || return 0
    [ -n "$(find "$root/apps" "$root/crates" "$root/assets" "$root/Cargo.toml" "$root/Cargo.lock" -newer "$target/built.stamp" -type f -print 2>/dev/null | head -n 1)" ]
}
if stale; then
    echo "effectcraft-cli: sources changed, building (log: target/local/build.log)…" >&2
    "$root/scripts/fork/app.sh" --build-only || { echo "effectcraft-cli: build failed, see $root/target/local/build.log" >&2; exit 1; }
fi
exec "$cli" "$@"
