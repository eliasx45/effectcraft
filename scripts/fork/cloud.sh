#!/usr/bin/env bash
# One command to use a vast.ai GPU machine Elias rented as the cloud preview (plan/cloud-render.md CR10).
#
#   scripts/fork/cloud.sh connect root@<ip> -p <port> [--project F.ecproj] [--idle MIN]
#   scripts/fork/cloud.sh status
#   scripts/fork/cloud.sh disconnect
#
# connect: check the machine, make sure its effectcraft-cli is this Mac's main (built there from
# source, nothing uploaded), mirror the project's media + fonts (CR7: <= 100 MB from the Mac, bigger
# files from Google Drive links, Motion Library cards fetched by the server from R2 with the read
# token it installs), start the frame server (nohup), open the SSH tunnel, set
# "Preview On: Cloud" in the running Mac app. Running it twice is safe.
# It never rents, stops or destroys an instance: that stays manual in the vast.ai console.
# Docs: docs/fork/cloud.md
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CONFIG_DIR="${EFFECTCRAFT_CONFIG_DIR:-$HOME/Library/Application Support/EffectCraft}"
STATE="$CONFIG_DIR/cloud-connection.env"      # ip, port, remote ports: no secrets
TOKEN_FILE="$CONFIG_DIR/cloud-frame-token"    # the app reads it (CR5)
SSH_KEY="${EC_CLOUD_KEY:-$HOME/.ssh/id_ed25519}"
SOCK="${EC_CLOUD_SOCK:-/tmp/ec-cloud-$(id -u).sock}"   # ssh ControlMaster = the tunnel
WD_PID="$CONFIG_DIR/cloud-watchdog.pid"         # the tunnel watchdog (CR11)
WD_LOG="$CONFIG_DIR/cloud-watchdog.log"
WD_STATE="$CONFIG_DIR/cloud-watchdog.state"
REMOTE_DIR=/workspace/effectcraft
SKILL_SCRIPTS="$HOME/.claude/skills/effectcraft-remote-gpu/scripts"
BANNER='vast.ai|Have fun|AI agents'

say() { printf '%s\n' "$*"; }
die() { printf '❌ %s\n' "$*" >&2; exit 1; }
quiet_banner() { grep -v -E "$BANNER" || true; }

usage() {
  cat <<'EOF'
cloud.sh connect root@IP -p PORT [options]   check, build, mirror, start the server, open the tunnel, switch the app to Cloud
  --project F.ecproj      mirror this project (default: the project open in the app)
  --frames-port N         frame server port on the machine (default 9878)
  --local-port N          port on this Mac for the tunnel (default: same as --frames-port)
  --app-port N            the Mac app's control port (default $EFFECTCRAFT_CONTROL_PORT or 9877)
  --idle MIN              stop the serve process (not the instance) after MIN minutes with no app connected (off)
  --no-mirror             skip the media/fonts mirror
cloud.sh status          tunnel, server commit, GPU, free VRAM/disk, pending uploads
cloud.sh disconnect      app back to This Mac, close the tunnel, stop the server process (the instance stays: destroy it yourself)
cloud.sh tunnel          reopen the tunnel and (re)start its watchdog, nothing else (no build, no mirror, no server restart)
EOF
}

# ---------- state file ----------
state_get() { [ -f "$STATE" ] && sed -n "s/^$1=//p" "$STATE" | head -1 || true; }
state_write() {
  mkdir -p "$CONFIG_DIR"
  {
    echo "# written by scripts/fork/cloud.sh; no secrets"
    echo "target=$TARGET"
    echo "ssh_port=$SSH_PORT"
    echo "frames_port=$FRAMES_PORT"
    echo "local_port=$LOCAL_PORT"
    echo "app_port=$APP_PORT"
    echo "idle_min=$IDLE_MIN"
    echo "connected_at=$(date -u +%FT%TZ)"
  } >"$STATE"
}
load_state() {
  [ -f "$STATE" ] || die "no connection saved ($STATE). Run: scripts/fork/cloud.sh connect root@IP -p PORT"
  TARGET="$(state_get target)"; SSH_PORT="$(state_get ssh_port)"; FRAMES_PORT="$(state_get frames_port)"
  LOCAL_PORT="$(state_get local_port)"; APP_PORT="$(state_get app_port)"; IDLE_MIN="$(state_get idle_min)"
}

# ---------- ssh ----------
ssh_opts() { # ssh_opts [CONNECT_TIMEOUT] [ALIVE_INTERVAL]   (ssh keeps an option's first value: set them here)
  printf '%s\n' -o IdentitiesOnly=yes -i "$SSH_KEY" -p "$SSH_PORT" -o ConnectTimeout="${1:-15}" -o StrictHostKeyChecking=accept-new \
    -o ServerAliveInterval="${2:-15}" -o ServerAliveCountMax=3 -o LogLevel=ERROR
}
rssh() { # rssh 'remote command'   (banner filtered)
  local opts=(); while IFS= read -r l; do opts+=("$l"); done < <(ssh_opts)
  ssh "${opts[@]}" "$TARGET" "$@" 2>&1 | quiet_banner
}
rbash() { # rbash ARGS... < script   runs the script on the machine with args
  local opts=(); while IFS= read -r l; do opts+=("$l"); done < <(ssh_opts)
  ssh "${opts[@]}" "$TARGET" bash -s -- "$@" 2>&1 | quiet_banner
}

# ---------- Mac app control channel (JSON lines on 127.0.0.1) ----------
app_call() { # app_call METHOD 'PARAMS-JSON' -> reply line (empty if the app is not reachable)
  local line
  { exec 3<>"/dev/tcp/127.0.0.1/$APP_PORT"; } 2>/dev/null || return 1
  printf '{"id":1,"method":"%s","params":%s}\n' "$1" "$2" >&3
  IFS= read -r -t 30 line <&3 || true
  exec 3>&- 3<&- || true
  printf '%s\n' "$line"
}
app_cmd() { app_call engine.execute "$(jq -cn --arg c "$1" --argjson p "$2" '{command:$c,params:$p}')"; }
app_up() { { exec 3<>"/dev/tcp/127.0.0.1/$APP_PORT"; } 2>/dev/null && { exec 3>&- 3<&-; return 0; } || return 1; }

# ---------- Mac side pieces ----------
mac_cli() {
  local c
  for c in "${EC_CLI:-}" "$REPO/target/local/local/effectcraft-cli" "$REPO/target/local/effectcraft-cli" \
    "$HOME/Developer/effectcraft/target/local/local/effectcraft-cli" "$HOME/Developer/effectcraft/target/local/effectcraft-cli" \
    "$(command -v effectcraft-cli || true)"; do
    if [ -n "$c" ] && [ -x "$c" ] && "$c" --help 2>&1 | grep -q 'drive-pull\|mirror USER@HOST'; then printf '%s\n' "$c"; return 0; fi
  done
  die "no effectcraft-cli with the mirror command on this Mac. Set EC_CLI=/path/to/effectcraft-cli (the app builds it on launch: scripts/fork/app.sh)."
}
ensure_token() {
  mkdir -p "$CONFIG_DIR"
  if [ ! -s "$TOKEN_FILE" ] || [ "$(wc -c <"$TOKEN_FILE")" -lt 9 ]; then
    (umask 077; openssl rand -hex 16 >"$TOKEN_FILE")
    say "🔑 new frame token saved to $TOKEN_FILE (Mac only; it goes to the server in its environment, not in the state file)"
  fi
  chmod 600 "$TOKEN_FILE"
}
# The Motion Library READ token (never the publish token) for the server: library-pull fetches
# applied cards from R2 there, and the browser desktop app's Motion Library panel works too.
# It travels on ssh stdin (not in a command line, a log or the template) into
# ~/.config/effectcraft/motion-library.json (root, for library-pull) and /home/user's (the app), 0600.
LIBRARY_READ="$CONFIG_DIR/motion-library.read.json"
INSTALL_LIBRARY='umask 077; f=$(mktemp); cat >"$f"; grep -q "\"token\"" "$f" || { rm -f "$f"; echo "LIB=bad"; exit 0; }
d="$HOME/.config/effectcraft"; mkdir -p "$d"; install -m 600 "$f" "$d/motion-library.json"
if [ -d /home/user ] && id user >/dev/null 2>&1; then mkdir -p /home/user/.config/effectcraft; chown user /home/user/.config/effectcraft
  install -m 600 -o user "$f" /home/user/.config/effectcraft/motion-library.json; fi
rm -f "$f"; echo "LIB=ok"'
ensure_library_token() {
  [ -f "$LIBRARY_READ" ] || { say "   ⚠️  no $LIBRARY_READ: Motion Library cards will be uploaded from this Mac"; return 0; }
  local pub="$CONFIG_DIR/motion-library.json" t p
  t="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("token",""))' "$LIBRARY_READ" 2>/dev/null || true)"
  p="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("token",""))' "$pub" 2>/dev/null || true)"
  if [ -z "$t" ] || [ "$t" = "$p" ]; then say "   ⚠️  $LIBRARY_READ has no read token (or the publish token): not sent"; return 0; fi
  case "$(rssh "$INSTALL_LIBRARY" <"$LIBRARY_READ")" in
    *LIB=ok*) say "🔑 Motion Library read token installed on the machine (the server fetches applied cards from R2)" ;;
    *) say "   ⚠️  could not install the Motion Library read token there: cards will be uploaded from this Mac" ;;
  esac
}
tunnel_up() { ssh -S "$SOCK" -O check "$TARGET" >/dev/null 2>&1; }
# The tunnel's ssh notices a dead network within 15 s (5 s keepalives, 3 missed) so the watchdog
# can reopen it; EC_CLOUD_SSH_OPTS adds ssh options (tests put a ProxyCommand there).
tunnel_ssh() { # tunnel_ssh: start the ControlMaster; prints ssh's error, returns its status
  local opts=(); while IFS= read -r l; do opts+=("$l"); done < <(ssh_opts 5 5)
  local extra=(); [ -n "${EC_CLOUD_SSH_OPTS:-}" ] && read -r -a extra <<<"$EC_CLOUD_SSH_OPTS"
  ssh "${opts[@]}" ${extra[@]+"${extra[@]}"} -o ExitOnForwardFailure=yes -o BatchMode=yes \
    -M -S "$SOCK" -f -N -L "127.0.0.1:$LOCAL_PORT:127.0.0.1:$FRAMES_PORT" "$TARGET" 2>&1 | quiet_banner
  tunnel_up
}
tunnel_open() {
  if tunnel_up; then return 0; fi
  rm -f "$SOCK"
  if lsof -nP -iTCP:"$LOCAL_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
    die "port $LOCAL_PORT on this Mac is taken by another program (not our tunnel). Use --local-port N."
  fi
  tunnel_ssh || die "the SSH tunnel did not start."
}
tunnel_close() {
  local pid; pid="$(ssh -S "$SOCK" -O check "$TARGET" 2>&1 | sed -n 's/.*pid=\([0-9]*\).*/\1/p')"
  if [ -n "$pid" ]; then
    ssh -S "$SOCK" -O exit "$TARGET" >/dev/null 2>&1 || true
    sleep 0.3; kill "$pid" 2>/dev/null || true   # a master hung on a dead network
  fi
  rm -f "$SOCK"
}

# ---------- tunnel watchdog (CR11) ----------
# A loop in its own session (survives the terminal or agent that ran connect), started by
# connect/tunnel, stopped by disconnect. Every 2 s it checks the ControlMaster; when it is gone
# (network blip, sleep, `ssh -O exit`) it reopens it, retrying quickly, then more slowly when the
# machine stays unreachable (an interruptible instance may be paused: outbid). It never rebuilds
# or restarts the server. It exits when the state file goes or names another machine, so it
# can't fight a deliberate disconnect. Log: $WD_LOG; one-line state for `status`: $WD_STATE.
wd_running() { local p; p="$(cat "$WD_PID" 2>/dev/null)" && [ -n "$p" ] && kill -0 "$p" 2>/dev/null && ps -o command= -p "$p" | grep -q 'cloud.sh _watch'; }
wd_stop() {
  local p; p="$(cat "$WD_PID" 2>/dev/null)" || true
  if wd_running; then kill "$p" 2>/dev/null || true; for _ in 1 2 3 4 5 6 7 8 9 10; do kill -0 "$p" 2>/dev/null || break; sleep 0.2; done; fi
  rm -f "$WD_PID"
}
wd_start() {
  wd_stop
  # perl's setsid: a new session, so closing the terminal (or the agent's shell) doesn't stop it.
  EC_CLOUD_SOCK="$SOCK" EFFECTCRAFT_CONFIG_DIR="$CONFIG_DIR" perl -e 'use POSIX "setsid"; fork and exit; setsid; exec @ARGV' \
    "$REPO/scripts/fork/cloud.sh" _watch </dev/null >>"$WD_LOG" 2>&1
  for _ in 1 2 3 4 5 6 7 8 9 10; do wd_running && return 0; sleep 0.2; done
  say "   ⚠️  the tunnel watchdog did not start (log: $WD_LOG)"
}
wd_log() { printf '%s %s\n' "$(date '+%F %T')" "$*"; }
cmd_watch() {
  load_state
  echo $$ >"$WD_PID"
  trap 'wd_log "stopped"; rm -f "$WD_STATE"; exit 0' TERM INT
  local me="$TARGET:$SSH_PORT" last now gap down_since="" tries=0 drops=0 wait=2 err="" note
  wd_log "watching the tunnel 127.0.0.1:$LOCAL_PORT → $TARGET -p $SSH_PORT:$FRAMES_PORT (pid $$)"
  # Keep the log small.
  [ "$(wc -c <"$WD_LOG" 2>/dev/null || echo 0)" -gt 1000000 ] && { tail -n 2000 "$WD_LOG" >"$WD_LOG.tmp" && mv "$WD_LOG.tmp" "$WD_LOG"; } || true
  last=$(date +%s)
  while :; do
    [ -f "$STATE" ] && [ "$(state_get target):$(state_get ssh_port)" = "$me" ] || { wd_log "connection state gone or changed: stopping"; rm -f "$WD_STATE" "$WD_PID"; exit 0; }
    now=$(date +%s); gap=$((now - last)); last=$now
    if [ "$gap" -gt 30 ] && [ -z "$down_since" ] && tunnel_up; then
      # The Mac slept (or this loop was stopped): the connection under the tunnel is most
      # likely dead, and ssh would only notice in 15 s. Reopen now.
      wd_log "Mac awake after ${gap} s: reopening the tunnel"
      tunnel_close
    fi
    if tunnel_up; then
      if [ -n "$down_since" ]; then
        wd_log "✅ tunnel back after $((now - down_since)) s ($tries attempts)"
        down_since=""; tries=0; wait=2
      fi
      printf 'up since=%s drops=%s\n' "${up_since:=$now}" "$drops" >"$WD_STATE"
    else
      if [ -z "$down_since" ]; then
        down_since=$now; drops=$((drops + 1)); up_since=""
        wd_log "⚠️  tunnel down (drop $drops): reopening"
      fi
      rm -f "$SOCK"
      tries=$((tries + 1))
      if lsof -nP -iTCP:"$LOCAL_PORT" -sTCP:LISTEN >/dev/null 2>&1; then
        err="port $LOCAL_PORT is taken by another program"
      else
        err="$(tunnel_ssh 2>&1 | tr '\n' ' ' | cut -c1-200)" && err=""
      fi
      if [ -z "$err" ] && tunnel_up; then continue; fi
      local down=$((now - down_since))
      if ! scutil -r "${TARGET#*@}" 2>/dev/null | grep -q '^Reachable'; then note="this Mac is offline"
      elif [ "$down" -ge 20 ] && printf '%s' "$err" | grep -qi 'refused'; then note="the machine refuses ssh: the instance is most likely paused (outbid, if interruptible) or stopped; check the vast.ai console"
      elif [ "$down" -ge 60 ]; then note="the machine has not answered for $((down / 60)) min: an interruptible instance may be paused (outbid) or stopped; check the vast.ai console"
      else note="retrying"; fi
      # Quick retries for a blip, then slower: an unreachable machine is no reason to spin.
      if [ "$down" -lt 120 ]; then wait=2; elif [ "$down" -lt 600 ]; then wait=15; else wait=60; fi
      if [ "$tries" -le 3 ] || [ $((tries % 10)) -eq 0 ] || [ "$wait" -gt 2 ]; then wd_log "   attempt $tries failed: ${err:-no tunnel} ($note)"; fi
      printf 'down since=%s drops=%s tries=%s note=%s\n' "$down_since" "$drops" "$tries" "$note" >"$WD_STATE"
      sleep "$wait" & wait $!; continue
    fi
    # (sleep in the background: a TERM from disconnect runs the trap at once, not after the sleep)
    sleep 2 & wait $!
  done
}

# ---------- remote scripts ----------
REMOTE_PREFLIGHT='
set -u
echo "gpu=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1)"
echo "vram_free_mb=$(nvidia-smi --query-gpu=memory.free --format=csv,noheader,nounits 2>/dev/null | head -1)"
echo "vram_total_mb=$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits 2>/dev/null | head -1)"
echo "disk_free_gb=$(df -BG --output=avail /workspace 2>/dev/null | tail -1 | tr -dc 0-9)"
echo "have_repo=$([ -d /workspace/effectcraft/.git ] && echo 1 || echo 0)"
echo "have_cargo=$(command -v cargo >/dev/null 2>&1 && echo 1 || { [ -x $HOME/.cargo/bin/cargo ] && echo 1 || echo 0; })"
echo "built=$(cat /workspace/effectcraft/target/local/.built-commit 2>/dev/null)"
echo "head=$(git -C /workspace/effectcraft rev-parse HEAD 2>/dev/null)"
'

# $1 = wanted commit. Fast-forwards the checkout (never discards anything) and starts a background build.
REMOTE_UPDATE='
set -eu
want="$1"; cd /workspace/effectcraft
export PATH="$HOME/.cargo/bin:$PATH"
if [ "$(git rev-parse HEAD)" != "$want" ]; then
  git cat-file -e "$want^{commit}" 2>/dev/null || { echo "ERR commit $want did not arrive"; exit 3; }
  git merge -q --ff-only "$want" || { echo "ERR cannot fast-forward /workspace/effectcraft to $want (local changes or another branch there)"; exit 3; }
fi
pgrep -f "cloud-build.state" >/dev/null && { echo "BUILDING (already running)"; exit 0; }
nohup bash -c "echo running >/workspace/cloud-build.state; if nice -n 15 \${EC_BUILD_CPUS:+taskset -c \$EC_BUILD_CPUS} cargo build --profile local -p effectcraft-cli -j \$(( \$(nproc) / 2 > 8 ? \$(nproc) / 2 : 8 )) >/workspace/cloud-build.log 2>&1; then echo $want >target/local/.built-commit; echo ok >/workspace/cloud-build.state; else echo failed >/workspace/cloud-build.state; fi" >/dev/null 2>&1 &
echo "BUILDING"
'

# args: frames_port commit idle_min token
REMOTE_SERVE='
set -u
fp="$1"; commit="$2"; idle="$3"; token="$4"
cli=/workspace/effectcraft/target/local/effectcraft-cli
pidf=/workspace/cloud-serve-$fp.pid; stampf=/workspace/cloud-serve-$fp.commit
alive() { [ -f "$pidf" ] && p=$(cat "$pidf") && [ -n "$p" ] && tr "\0" " " </proc/$p/cmdline 2>/dev/null | grep -q -- "serve --gpu --frames $fp"; }
if alive; then
  p=$(cat "$pidf")
  if [ "$(cat "$stampf" 2>/dev/null)" = "$commit" ] && tr "\0" "\n" </proc/$p/environ 2>/dev/null | grep -qx "EFFECTCRAFT_FRAME_TOKEN=$token"; then
    echo "SERVE already running (pid $p, commit ${commit:0:8})"; skip=1
  else
    echo "SERVE restarting (other commit or token)"; kill "$p"; sleep 2; skip=0
  fi
else skip=0; fi
if [ "$skip" = 0 ]; then
  cd /workspace
  [ -f "$HOME/.config/effectcraft/cloud.env" ] && . "$HOME/.config/effectcraft/cloud.env"
  export EFFECTCRAFT_FONT_DIRS EFFECTCRAFT_CJK_LOCALE
  # CR8: the RAM the rental allots (machine-ram-gb, decimal GB, the vast portal figure) caps every cache size.
  for rg in /home/user/.config/effectcraft/machine-ram-gb "$HOME/.config/effectcraft/machine-ram-gb"; do
    [ -f "$rg" ] && export EFFECTCRAFT_RAM_GB="$(tr -dc 0-9. <"$rg")" && break
  done
  # CR8: the server frame cache sized by ec-tune (machine-tune.json "serve"); without it serve takes 85 % of the RAM
  # (the smaller of EFFECTCRAFT_RAM_GB and the container limit) less 8 GB.
  for mt in /home/user/.config/effectcraft/machine-tune.json "$HOME/.config/effectcraft/machine-tune.json"; do
    [ -f "$mt" ] || continue
    eval "$(python3 -c "import json,shlex,sys; s=json.load(open(sys.argv[1])).get(\"serve\") or {}; [print(f\"export {k}={shlex.quote(str(s[j]))}\") for k,j in ((\"EFFECTCRAFT_SERVER_FRAMES_MB\",\"framesMb\"),(\"EFFECTCRAFT_SERVER_FRAMES_DISK_GB\",\"framesDiskGb\"),(\"EFFECTCRAFT_SERVER_FRAMES_DIR\",\"framesDir\")) if j in s]" "$mt" 2>/dev/null)"
    break
  done
  EFFECTCRAFT_FRAME_TOKEN="$token" EFFECTCRAFT_FRAME_GPU_SLOTS=4 WGPU_BACKEND=vulkan RUST_LOG=info \
    setsid nohup "$cli" serve --gpu --frames "$fp" >/workspace/cloud-serve-$fp.log 2>&1 </dev/null &
  echo $! >"$pidf"; echo "$commit" >"$stampf"
  # A fresh machine builds its GPU kernels first (17 s on an A40): wait up to 90 s while it runs.
  for i in $(seq 1 90); do sleep 1; ss -ltn 2>/dev/null | grep -q ":$fp " && break; kill -0 "$(cat "$pidf")" 2>/dev/null || break; done
  ss -ltn 2>/dev/null | grep -q ":$fp " && echo "SERVE started (pid $(cat "$pidf"))" || { echo "ERR serve did not listen on $fp:"; tail -5 /workspace/cloud-serve-$fp.log; exit 4; }
fi
# optional idle watchdog: stops the serve process (never the instance) after N minutes without a connected app
wd=/workspace/cloud-idle-$fp.pid
[ -f "$wd" ] && kill "$(cat "$wd")" 2>/dev/null || true; rm -f "$wd"
if [ "$idle" -gt 0 ] 2>/dev/null; then
  setsid nohup bash -c "last=\$(date +%s); while kill -0 \$(cat $pidf) 2>/dev/null; do
      if [ \$(ss -Htn state established \"( sport = :$fp )\" 2>/dev/null | wc -l) -gt 0 ]; then last=\$(date +%s); fi
      if [ \$((\$(date +%s)-last)) -gt $((idle*60)) ]; then echo \"\$(date -u +%T) idle $idle min: stopping serve\" >>/workspace/cloud-serve-$fp.log; kill \$(cat $pidf); break; fi
      sleep 20; done; rm -f $wd" >/dev/null 2>&1 </dev/null &
  echo $! >"$wd"; echo "IDLE watchdog: stops serve after $idle min without the app"
fi
'

# args: frames_port
REMOTE_STOP='
fp="$1"; pidf=/workspace/cloud-serve-$fp.pid; wd=/workspace/cloud-idle-$fp.pid
[ -f "$wd" ] && kill "$(cat "$wd")" 2>/dev/null; rm -f "$wd"
if [ -f "$pidf" ] && p=$(cat "$pidf") && [ -r /proc/$p/cmdline ] && tr "\0" " " </proc/$p/cmdline 2>/dev/null | grep -q -- "serve --gpu --frames $fp"; then
  kill "$p"; sleep 1; echo "serve stopped (pid $p)"; rm -f "$pidf" /workspace/cloud-serve-$fp.commit
else echo "no serve of ours running on $fp"; rm -f "$pidf"; fi
'

REMOTE_STATUS='
fp="$1"; pidf=/workspace/cloud-serve-$fp.pid
echo "gpu=$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1)"
echo "vram_free_mb=$(nvidia-smi --query-gpu=memory.free --format=csv,noheader,nounits 2>/dev/null | head -1)"
echo "vram_total_mb=$(nvidia-smi --query-gpu=memory.total --format=csv,noheader,nounits 2>/dev/null | head -1)"
echo "disk_free_gb=$(df -BG --output=avail /workspace 2>/dev/null | tail -1 | tr -dc 0-9)"
echo "built=$(cat /workspace/effectcraft/target/local/.built-commit 2>/dev/null)"
if [ -f "$pidf" ] && p=$(cat "$pidf") && tr "\0" " " </proc/$p/cmdline 2>/dev/null | grep -q -- "serve --gpu --frames $fp"; then
  echo "serve=running pid $p since $(ps -o lstart= -p $p | sed "s/  */ /g"), commit $(cat /workspace/cloud-serve-$fp.commit 2>/dev/null)"
  echo "clients=$(ss -Htn state established "( sport = :$fp )" 2>/dev/null | wc -l)"
else echo "serve=not running"; fi
'

parse_kv() { sed -n "s/^$1=//p" | head -1; }

# ---------- connect ----------
# Big files with a Google Drive file link in the project's sidecar start downloading on the machine
# right away, while it is set up and builds (curl, no effectcraft-cli needed). Each lands at its Mac
# path with the Mac's size and mtime, so the mirror (step 3) finds it up to date. Folder links and
# anything that fails are left to the mirror.
drive_pull_early() { # drive_pull_early PROJECT
  local sidecar="${1%.ecproj}.cloud.json" jobs="" path link id size mtime
  [ -f "$sidecar" ] || return 0
  while IFS=$'\t' read -r path link; do
    id="$(printf '%s' "$link" | sed -n 's#.*/file/d/\([A-Za-z0-9_-]*\).*#\1#p')"
    [ -n "$id" ] && [ -f "$path" ] || continue
    read -r size mtime < <(stat -f '%z %m' "$path") || continue
    jobs+="$id"$'\t'"$size"$'\t'"$mtime"$'\t'"$path"$'\n'
  done < <(jq -r '.drive // {} | to_entries[] | [.key, .value] | @tsv' "$sidecar" 2>/dev/null)
  [ -n "$jobs" ] || return 0
  say "   ☁️  pulling $(printf '%s' "$jobs" | grep -c .) big file(s) from Google Drive on the machine meanwhile"
  rbash "$(printf '%s' "$jobs" | base64 | tr -d '\n')" <<'EOS' >/dev/null
rm -rf /tmp/ec-drive-early; mkdir -p /tmp/ec-drive-early; n=0
printf '%s' "$1" | base64 -d | while IFS=$'\t' read -r id size mtime path; do
  n=$((n + 1)); st="/tmp/ec-drive-early/$n"
  if [ "$(stat -c%s "$path" 2>/dev/null)" = "$size" ]; then echo DONE > "$st"; continue; fi
  echo RUN > "$st"; mkdir -p "$(dirname "$path")"
  setsid nohup bash -c 'curl -sSL --retry 3 -o "$4.part" "https://drive.usercontent.google.com/download?id=$1&export=download&confirm=t" && [ "$(stat -c%s "$4.part")" = "$2" ] && touch -d "@$3" "$4.part" && mv "$4.part" "$4" && echo DONE > "$5" || { rm -f "$4.part"; echo FAIL > "$5"; }' _ "$id" "$size" "$mtime" "$path" "$st" >/dev/null 2>&1 </dev/null &
done
EOS
}
drive_pull_wait() { # waits for drive_pull_early's downloads (up to 30 min), then the mirror takes over
  local waited=0 st
  while st="$(rssh 'cat /tmp/ec-drive-early/* 2>/dev/null')"; printf '%s' "$st" | grep -q RUN; do
    [ "$waited" -ge 1800 ] && { say "   ⚠️  Drive downloads still running after 30 min: the mirror takes over"; return 0; }
    sleep 10; waited=$((waited + 10))
  done
  [ -z "$st" ] || say "   ☁️  Drive: $(printf '%s' "$st" | grep -c DONE) arrived$(printf '%s' "$st" | grep -q FAIL && echo ", $(printf '%s' "$st" | grep -c FAIL) failed (the mirror retries)")"
}

cmd_connect() {
  TARGET=""; SSH_PORT=22; FRAMES_PORT=9878; LOCAL_PORT=""; APP_PORT="${EFFECTCRAFT_CONTROL_PORT:-9877}"; IDLE_MIN=0
  local project="" do_mirror=1
  while [ $# -gt 0 ]; do
    case "$1" in
      -p|--ssh-port) SSH_PORT="$2"; shift 2 ;;
      --project) project="$2"; shift 2 ;;
      --frames-port) FRAMES_PORT="$2"; shift 2 ;;
      --local-port) LOCAL_PORT="$2"; shift 2 ;;
      --app-port) APP_PORT="$2"; shift 2 ;;
      --idle) IDLE_MIN="$2"; shift 2 ;;
      --no-mirror) do_mirror=0; shift ;;
      -h|--help) usage; exit 0 ;;
      -*) die "unknown option $1 (cloud.sh --help)" ;;
      *) [ -z "$TARGET" ] && TARGET="$1" || die "unexpected argument $1"; shift ;;
    esac
  done
  [ -n "$TARGET" ] || { usage; exit 2; }
  LOCAL_PORT="${LOCAL_PORT:-$FRAMES_PORT}"
  case "$SSH_PORT$FRAMES_PORT$LOCAL_PORT$APP_PORT$IDLE_MIN" in *[!0-9]*) die "ports and --idle must be numbers" ;; esac
  [ -f "$SSH_KEY" ] || die "ssh key $SSH_KEY not found (EC_CLOUD_KEY=...)"

  say "🔎 1/5 Checking the machine $TARGET (port $SSH_PORT)"
  local pre; pre="$(rbash <<<"$REMOTE_PREFLIGHT")" || true
  [ -n "$(printf '%s' "$pre" | parse_kv gpu)" ] || die "cannot reach $TARGET or no GPU there: $pre"
  say "   GPU: $(printf '%s' "$pre" | parse_kv gpu), free VRAM $(printf '%s' "$pre" | parse_kv vram_free_mb) / $(printf '%s' "$pre" | parse_kv vram_total_mb) MB, free disk $(printf '%s' "$pre" | parse_kv disk_free_gb) GB"
  [ "$do_mirror" = 1 ] && [ -n "$project" ] && drive_pull_early "$project"
  # A machine rented without the EffectCraft template (or whose On-start didn't run): set it up
  # here with the same script the template embeds, then check again.
  local provision="$SKILL_SCRIPTS/template/ec-provision.sh"
  if [ "$(printf '%s' "$pre" | parse_kv have_repo)" != 1 ] && [ -f "$provision" ]; then
    say "🧰 The machine isn't set up for EffectCraft yet: running the template's setup (ec-provision.sh); about 3 min on 64+ cores"
    rbash "$(base64 <"$provision" | tr -d '\n')" <<'EOS' >/dev/null
printf '%s' "$1" | base64 -d > /root/ec-provision.sh
pgrep -f ec-provision.sh >/dev/null || setsid nohup bash /root/ec-provision.sh >/dev/null 2>&1 </dev/null &
EOS
    local waited=0
    until [ "$(rssh 'cat /tmp/ec-ready 2>/dev/null' | head -1)" != "" ]; do
      [ "$waited" -ge 1800 ] && die "the setup didn't finish in 30 min (on the machine: tail /tmp/ec-setup.log)"
      sleep 20; waited=$((waited + 20))
    done
    say "   setup: $(rssh 'head -1 /tmp/ec-ready')"
    pre="$(rbash <<<"$REMOTE_PREFLIGHT")" || true
  fi
  [ "$(printf '%s' "$pre" | parse_kv have_repo)" = 1 ] || die "no $REMOTE_DIR on the machine: it is not set up for EffectCraft yet (see the effectcraft-remote-gpu skill: scripts/template/ec-provision.sh)."
  [ "$(printf '%s' "$pre" | parse_kv have_cargo)" = 1 ] || die "no cargo on the machine, so effectcraft-cli cannot be built there."
  local free; free="$(printf '%s' "$pre" | parse_kv disk_free_gb)"
  [ "${free:-0}" -ge 6 ] 2>/dev/null || say "⚠️  only ${free:-?} GB free on /workspace: the media may not fit"
  if [ -x "$SKILL_SCRIPTS/check.sh" ]; then # the template's own check (non-fatal: it tests NVENC for the browser stream, not needed here)
    say "   machine check (check.sh):"
    rbash <"$SKILL_SCRIPTS/check.sh" | sed 's/^/     /' | head -12 || true
  fi

  local mac_commit; mac_commit="$(git -C "$REPO" rev-parse main)"
  local built; built="$(printf '%s' "$pre" | parse_kv built)"
  if [ "$built" = "$mac_commit" ]; then
    say "✅ effectcraft-cli on the machine is already at ${mac_commit:0:8} (this Mac's main)"
  else
    [ "${free:-0}" -ge 12 ] 2>/dev/null || die "only ${free:-?} GB free on /workspace; a build of effectcraft-cli needs about 12 GB. Free space on the machine first (old target/ folders, /workspace/*.log, old copies like cr*-target or cr*-src), then run connect again."
    say "🔨 2/5 Building effectcraft-cli on the machine at ${mac_commit:0:8} (was ${built:0:8}); nothing is uploaded"
    # only the missing commits travel (git over ssh, a few MB at most); the machine never needs GitHub
    local sshcmd="ssh"; while IFS= read -r l; do sshcmd+=" '$l'"; done < <(ssh_opts)
    GIT_SSH_COMMAND="$sshcmd" git -C "$REPO" push -q -f "ssh://$TARGET$REMOTE_DIR" "$mac_commit:refs/cloud/mac-main" 2>&1 | quiet_banner \
      || die "could not send the source commits to the machine"
    local r; r="$(rbash "$mac_commit" <<<"$REMOTE_UPDATE")" || true
    case "$r" in *ERR*) die "$(printf '%s' "$r" | grep ERR)" ;; *BUILDING*) ;; *) die "could not start the build: $r" ;; esac
    local t0=$SECONDS st
    while :; do
      sleep 20
      st="$(rssh "tail -c 600 /workspace/cloud-build.log | tr '\r' '\n' | tail -2; echo STAMP=\$(cat $REMOTE_DIR/target/local/.built-commit 2>/dev/null); echo STATE=\$(cat /workspace/cloud-build.state 2>/dev/null)")" || true
      say "   … $((SECONDS - t0)) s: $(printf '%s' "$st" | grep -v -E 'STAMP=|STATE=' | tail -1 | cut -c1-110)"
      printf '%s' "$st" | grep -q "STAMP=$mac_commit" && break
      if printf '%s' "$st" | grep -q 'STATE=failed'; then
        printf '%s\n' "❌ the build on the machine failed; last lines of /workspace/cloud-build.log there:" >&2
        rssh 'tail -15 /workspace/cloud-build.log' >&2; exit 6
      fi
      [ $((SECONDS - t0)) -lt 3600 ] || die "the build takes over an hour; check /workspace/cloud-build.log on the machine"
    done
    say "✅ built in $((SECONDS - t0)) s"
  fi

  ensure_token
  ensure_library_token
  if [ "$do_mirror" = 1 ]; then
    say "📦 3/5 Mirroring media + fonts (up to 100 MB from this Mac once; bigger files from Google Drive links; Motion Library cards straight from R2)"
    [ -n "$project" ] && drive_pull_wait
    local cli; cli="$(mac_cli)"
    local mopts=("$TARGET" --ssh-port "$SSH_PORT" --identity "$SSH_KEY")
    if [ -n "$project" ]; then mopts+=(--project "$project")
    elif app_up; then mopts+=(--bridge "$APP_PORT")
    else say "   ⚠️  no --project and the Mac app is not listening on $APP_PORT: skipping the mirror (re-run with --project F.ecproj)"; mopts=(); fi
    if [ ${#mopts[@]} -gt 0 ]; then
      if ! "$cli" mirror "${mopts[@]}" 2>&1 | quiet_banner; then
        cat >&2 <<'EOF'

❌ The mirror stopped: files or fonts are missing on the machine (listed above).
   Files over 100 MB are never uploaded from this Mac. For each one listed:
   1. upload it to Google Drive and copy its share link,
   2. paste the link into the project's sidecar "<project>.cloud.json" ("driveLinks": {"file name": "link"}),
      or in the app: command cloud.driveLinks {"set": {"file name": "link"}},
   3. run this same connect command again (finished files are not sent twice).
EOF
        exit 5
      fi
    fi
  else say "📦 3/5 Mirror skipped (--no-mirror)"; fi

  say "🚀 4/5 Frame server on the machine (port $FRAMES_PORT)"
  local token; token="$(cat "$TOKEN_FILE")"
  out="$(mktemp -t ec-cloud-serve)"; trap 'rm -f "$out"' EXIT
  rbash "$FRAMES_PORT" "$mac_commit" "$IDLE_MIN" "$token" <<<"$REMOTE_SERVE" >"$out" 2>&1 || true
  if grep -q '^ERR' "$out"; then cat "$out"; die "the frame server did not start"; fi
  sed 's/^/   /' "$out"

  say "🔌 5/5 SSH tunnel 127.0.0.1:$LOCAL_PORT → machine:$FRAMES_PORT and the Mac app"
  tunnel_open
  state_write
  wd_start
  say "   tunnel up, watchdog pid $(cat "$WD_PID" 2>/dev/null) reopens it after a drop (log: $WD_LOG)"
  if app_up; then
    app_cmd prefs.set "$(jq -cn --arg a "127.0.0.1:$LOCAL_PORT" '{values:{"previews.cloudAddress":$a,"previews.previewOn":"cloud"}}')" >/dev/null || true
    local back; back="$(app_cmd prefs.get '{"key":"previews.previewOn"}' || true)"
    printf '%s' "$back" | grep -q cloud && say "   app (port $APP_PORT): Preview On = Cloud, address 127.0.0.1:$LOCAL_PORT" || say "   ⚠️  could not confirm the app setting: $back"
  else
    say "   ⚠️  the Mac app is not listening on control port $APP_PORT. Open Settings ▸ Previews ▸ Cloud Preview: Preview On = Cloud, Cloud Address = 127.0.0.1:$LOCAL_PORT"
  fi
  say "🎉 Connected. Check with: scripts/fork/cloud.sh status   Stop with: scripts/fork/cloud.sh disconnect"
  say "   Destroying the instance stays manual (vast.ai console)."
}

# ---------- status ----------
wd_status() {
  if ! wd_running; then say "🐕 watchdog: not running (scripts/fork/cloud.sh tunnel starts it and reopens the tunnel)"; return; fi
  local w; w="$(cat "$WD_STATE" 2>/dev/null || true)"
  local since drops tries note; since="$(printf '%s' "$w" | sed -n 's/.*since=\([0-9]*\).*/\1/p')"
  drops="$(printf '%s' "$w" | sed -n 's/.*drops=\([0-9]*\).*/\1/p')"; tries="$(printf '%s' "$w" | sed -n 's/.*tries=\([0-9]*\).*/\1/p')"
  note="$(printf '%s' "$w" | sed -n 's/.*note=//p')"
  case "$w" in
    down*) say "🐕 watchdog pid $(cat "$WD_PID"): tunnel down for $(( $(date +%s) - since )) s, $tries attempts: $note" ;;
    *) say "🐕 watchdog pid $(cat "$WD_PID"): ${drops:-0} drops reopened$( [ -n "$since" ] && echo ", up $(( ($(date +%s) - since) / 60 )) min")" ;;
  esac
  local last; last="$(grep -E 'tunnel down|tunnel back|awake' "$WD_LOG" 2>/dev/null | tail -2 | tr '\n' ';' | sed 's/;$//; s/;/ · /')"
  [ -n "$last" ] && say "   last: $last (log: $WD_LOG)"
  return 0
}
cmd_status() {
  load_state
  say "🔗 connection: $TARGET -p $SSH_PORT (saved $(state_get connected_at))"
  if tunnel_up; then say "✅ tunnel up: 127.0.0.1:$LOCAL_PORT → machine:$FRAMES_PORT"; else say "❌ tunnel down"; fi
  wd_status
  local s; s="$(rbash "$FRAMES_PORT" <<<"$REMOTE_STATUS")" || true
  if [ -z "$(printf '%s' "$s" | parse_kv gpu)" ]; then say "❌ machine not reachable over ssh"; else
    local mac_commit; mac_commit="$(git -C "$REPO" rev-parse main)"
    local b; b="$(printf '%s' "$s" | parse_kv built)"
    say "🖥  GPU: $(printf '%s' "$s" | parse_kv gpu), free VRAM $(printf '%s' "$s" | parse_kv vram_free_mb) / $(printf '%s' "$s" | parse_kv vram_total_mb) MB, free disk $(printf '%s' "$s" | parse_kv disk_free_gb) GB"
    say "🧱 cli on the machine: ${b:0:8}$([ "$b" = "$mac_commit" ] && echo " (= this Mac's main)" || echo " (Mac main is ${mac_commit:0:8}: connect again to rebuild)")"
    say "🚀 server: $(printf '%s' "$s" | parse_kv serve), clients connected: $(printf '%s' "$s" | parse_kv clients)"
  fi
  APP_PORT="${APP_PORT:-9877}"
  if app_up; then
    local pend pv ins; pend="$(app_cmd cloud.pendingUploads '{}' || true)"; pv="$(app_cmd prefs.get '{"key":"previews.previewOn"}' || true)"
    ins="$(app_call ui.inspect '{}' || true)"
    say "🍎 app: Preview On = $(printf '%s' "$pv" | jq -r '.result | if type=="object" then .value else . end' 2>/dev/null || echo '?'), pending uploads: $(printf '%s' "$pend" | jq -r '.result.count // "?"' 2>/dev/null || echo '?') ($(printf '%s' "$pend" | jq -r '(.result.bytes // 0) / 1048576 | floor' 2>/dev/null || echo '?') MB)"
    say "   link: $(printf '%s' "$ins" | jq -r '.result.cloud | "connected=\(.connected) ready=\(.ready) rtt=\(.rttMs // "-") ms frames=\(.frames) gpu=\(.gpu // "-")"' 2>/dev/null || echo 'unknown')"
    say "   📶 internet this session: $(printf '%s' "$ins" | jq -r '.result.cloud | "\((.bytesDown // 0) / 1048576 | floor) MB down, \((.bytesUp // 0) / 1048576 | floor) MB up"' 2>/dev/null || echo 'unknown')"
    say "   🗄  server frame cache: $(printf '%s' "$ins" | jq -r '.result.cloud.serverCache | if . == null then "no report yet (Cache Frames When Idle off, or not idle yet)" else "\(.done)/\(.total) frames of the comp cached, \(.ramMb) of \(.ramBudgetMb) MB RAM, \(.diskMb) MB disk" end' 2>/dev/null || echo 'unknown')"
  else say "🍎 app: not listening on control port $APP_PORT"; fi
}

# ---------- tunnel only ----------
cmd_tunnel() {
  load_state
  APP_PORT="${APP_PORT:-9877}"
  wd_stop
  tunnel_open
  wd_start
  say "✅ tunnel up: 127.0.0.1:$LOCAL_PORT → $TARGET:$FRAMES_PORT; watchdog pid $(cat "$WD_PID" 2>/dev/null) (log: $WD_LOG)"
}

# ---------- disconnect ----------
cmd_disconnect() {
  load_state
  APP_PORT="${APP_PORT:-9877}"
  if app_up; then
    app_cmd prefs.set '{"values":{"previews.previewOn":"local"}}' >/dev/null || true
    say "🍎 app: Preview On = This Mac"
  else say "🍎 app not reachable on $APP_PORT (set Preview On = This Mac yourself if it is open)"; fi
  wd_stop; tunnel_close; say "🔌 tunnel closed (watchdog stopped)"
  rbash "$FRAMES_PORT" <<<"$REMOTE_STOP" | sed 's/^/🚀 /' || say "⚠️  could not reach the machine to stop the server (it may be gone already)"
  rm -f "$STATE"
  say "✅ Disconnected. The rented instance is still running and billing: destroy it in the vast.ai console when you are done."
}

case "${1:-}" in
  connect) shift; cmd_connect "$@" ;;
  status) shift; cmd_status "$@" ;;
  disconnect) shift; cmd_disconnect "$@" ;;
  tunnel) shift; cmd_tunnel "$@" ;;
  _watch) shift; cmd_watch; exit 0 ;;
  -h|--help|help|"") usage ;;
  *) usage; exit 2 ;;
esac
