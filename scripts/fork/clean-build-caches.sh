#!/usr/bin/env bash
# Weekly trim of EffectCraft's Rust build caches (Elias's fork), run by launchd
# (~/Library/LaunchAgents/com.eliasx45.effectcraft.clean-build-caches.plist, Sundays 04:30).
#
# Deletes the `incremental/` folders under target/ that no build touched for DAYS days (default 3):
# they are most of the size (a lane grows to 100+ GB) and only make the next build of that lane
# faster. Compiled dependencies stay, so a lane stays warm-ish.
#   - never target/local (the app Elias launches, scripts/fork/app.sh)
#   - never a folder a running cargo/rustc is using
#
#   clean-build-caches.sh            delete
#   clean-build-caches.sh --dry-run  only list what would go
set -u
root="$(cd "$(dirname "$0")/../.." && pwd)"
days="${DAYS:-3}"
dry=0; [ "${1:-}" = "--dry-run" ] && dry=1
log() { printf '%s %s\n' "$(date '+%F %T')" "$*"; }

busy="$(ps -eo args | grep -E '(cargo|rustc)' | grep -v grep || true)"
freed=0
while IFS= read -r dir; do
  [ -n "$dir" ] || continue
  case "$dir" in "$root/target/local/"*) continue ;; esac
  # The target folder this cache belongs to (target/lane-2, target/debug, ...).
  rel="${dir#"$root/target/"}"; top="target/${rel%%/*}"
  if printf '%s' "$busy" | grep -q -- "$top"; then log "skip (building): $dir"; continue; fi
  # Anything in it touched recently: in use, keep.
  if [ -n "$(find "$dir" -maxdepth 2 -newermt "-${days} days" -print -quit 2>/dev/null)" ]; then continue; fi
  kb="$(du -sk "$dir" 2>/dev/null | cut -f1)"; kb="${kb:-0}"
  if [ "$dry" = 1 ]; then log "would delete $((kb / 1048576)) GB: $dir"; else rm -rf "$dir" && log "deleted $((kb / 1048576)) GB: $dir"; fi
  freed=$((freed + kb))
done < <(find "$root/target" -maxdepth 3 -type d -name incremental 2>/dev/null)
log "$([ "$dry" = 1 ] && echo 'would free' || echo 'freed') $((freed / 1048576)) GB; free now: $(df -h / | awk 'NR==2{print $4}')"
