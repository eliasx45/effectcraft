# Sourced by scripts/mcp.sh and scripts/fork/app.sh (POSIX sh): long-running servers and the app run
# a frozen copy of their binary, never the build output itself.
#
# Why: a rebuild replaces target/<profile>/effectcraft-cli while servers still run the old image.
# macOS then finds a different file at a running process's path, so every process that server
# starts (vm_stat, sysctl, ps, … each tried once per PATH folder) is blocked and the new 115 MB
# file is malware-scanned again. Seven stale MCP servers did that ~66 times a second, starved
# syspolicyd and froze WindowServer until the watchdog killed it (23 Oct 2026). A copy per build
# that nothing ever overwrites keeps every running image matching its file.

# frozen_exe BIN DIR: prints the path to run BIN from: DIR/<name>-<inode>-<mtime>, made once per
# build (an APFS clone, so no extra disk until the build output changes). Copies of earlier
# builds that no process runs any more are removed. Prints BIN itself when the copy can't be made.
frozen_exe() {
    src=$1 dir=$2
    name=$(basename "$src")
    id=$(stat -f '%i-%m' "$src" 2>/dev/null || stat -c '%i-%Y' "$src" 2>/dev/null) || { echo "$src"; return 0; }
    dst="$dir/$name-$id"
    if [ ! -x "$dst" ]; then
        mkdir -p "$dir" 2>/dev/null || { echo "$src"; return 0; }
        tmp="$dst.tmp.$$"
        # -c clones on APFS (instant); plain cp elsewhere.
        # touch: a clone keeps the build's mtime, and the cleanup below goes by the copy's age.
        if ! { cp -c "$src" "$tmp" 2>/dev/null || cp "$src" "$tmp" 2>/dev/null; } || ! touch "$tmp" || ! mv -f "$tmp" "$dst"; then
            rm -f "$tmp"
            echo "$src"
            return 0
        fi
    fi
    for old in "$dir/$name"-[0-9]*; do
        [ -e "$old" ] && [ "$old" != "$dst" ] || continue
        # Spare a copy made in the last two minutes: another server may be about to start it.
        [ -n "$(find "$old" -maxdepth 0 -mmin -2 2>/dev/null)" ] && continue
        case "$old" in
        *.tmp.*) rm -f "$old" ;;
        *) pgrep -f "^$old( |\$)" >/dev/null 2>&1 || rm -f "$old" ;;
        esac
    done
    echo "$dst"
}
