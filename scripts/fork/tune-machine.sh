#!/usr/bin/env bash
# tune-machine.sh: size EffectCraft's MACHINE settings for THIS computer (RAM, free disk, GPU) and write
# only those into prefs.json. Personal settings (shortcuts, workspaces, UI, editing prefs) stay as the Mac sent them.
# Runs ON the server (as root or user). push-private.sh sends it and runs it after the configs; ec-launch runs it
# before every start (installed as /usr/local/bin/ec-tune), so a new machine is never left on the Mac's values.
#
#   tune-machine.sh [--dry-run] [--quiet] [--force] [--conf DIR] [--project F.ecproj]...
#     --dry-run   print the table and what would change; write nothing
#     --quiet     one summary line (for ec-launch)
#     --force     also replace values changed by hand since the last tune (push-private.sh: the Mac's prefs just arrived)
#     --conf DIR  settings folder (default /home/user/.config/effectcraft)
#     --project   size the preview caches for this project's longest comp (default: the recent projects in prefs.json)
# Rules and the measurements behind them: plan/cloud-render-baseline.md ▸ "Cache settings per machine".
set -u
exec python3 - "$@" <<'PY'
import argparse, glob, json, math, os, shutil, subprocess, sys, time

ap = argparse.ArgumentParser(prog="tune-machine.sh")
ap.add_argument("--dry-run", action="store_true")
ap.add_argument("--quiet", action="store_true")
ap.add_argument("--force", action="store_true")
ap.add_argument("--conf", default="/home/user/.config/effectcraft")
ap.add_argument("--project", action="append", default=[])
a = ap.parse_args()
GIB = 1 << 30

# ---- Rules (see the baseline doc; change them there and here together) ----
CACHE_SHARE = 0.85        # Elias: all RAM caches together use up to 85 % of RAM
RESERVE_MIN_GB = 8        # ... but always leave >= 8 GB for the app itself, the GPU driver, KDE, Selkies
LAYER_RULE = (0.03, 2, 8)  # layer cache = 3 % of RAM, 2-8 GB. Sweep (3090): 2, 8, 20, 128 GB re-render equally fast; its
                           # GPU part is capped at 1.5 GB anyway. 8 GB = headroom for CPU-heavy layers the test didn't have.
MEDIA_RULE = (0.03, 2, 8)  # footage cache = 3 % of RAM, 2-8 GB. Sweep: 2, 8, 20, 64 GB equal (ProRes decode overlaps the GPU
                           # render); 8 GB keeps the 32-frame decode-ahead full for a few 1080p clips at once.
PREVIEW_HEADROOM = 1.10   # preview cache = the longest comp at Full res x 1.1, if RAM allows
DISK_MARGIN = (15, 0.20)  # leave max(15 GB, 20 % of the disk) free: builds of main (5-15 GB a target dir), logs, autosaves,
                          # uploads. 17 Oct: the 3090's 60 GB disk hit 0 bytes free with a 6 GB cache + other sessions' builds.
DISK_FRAME_MB = 2.0       # an LZ4 1080p frame on disk (measured 1.92 MB avg on 10-10 Edit)
DISK_NEED_FACTOR = 2.0    # frames + processed layers kept on disk ~ 2x the frames
DISK_MIN_GB = 10          # below this the disk cache is off: a few minutes of frames isn't worth a full-disk risk, and the RAM
                          # preview cache (much bigger on a cloud box) already holds them

def sh(cmd):
    try:
        return subprocess.run(cmd, shell=True, capture_output=True, text=True, timeout=20).stdout.strip()
    except Exception:
        return ""

# ---- Measure this machine ----
# RAM = what this container may use: the host's MemTotal, limited by the cgroup (vast rents a slice of a bigger
# host: the 3090 shows 504 GB in /proc/meminfo but memory.max = 342 GiB). EC_TUNE_SYSROOT: a fake / for tests.
ROOT = os.environ.get("EC_TUNE_SYSROOT", "")

def read(path):
    try:
        return open(ROOT + path).read().strip()
    except OSError:
        return ""

def host_ram():
    for line in read("/proc/meminfo").splitlines():
        if line.startswith("MemTotal:"):
            return int(line.split()[1]) * 1024
    return 0 if ROOT else os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES")

def cgroup_limit(host):
    """The container's memory limit in bytes, or None: cgroup v2 memory.max ("max" = none), else v1
    memory.limit_in_bytes (unlimited = a huge sentinel, about 2^63)."""
    for path in ("/sys/fs/cgroup/memory.max", "/sys/fs/cgroup/memory/memory.limit_in_bytes"):
        v = read(path)
        if v.isdigit() and 0 < int(v) < (1 << 60) and (host == 0 or int(v) < host):
            return int(v)
    return None

host = host_ram()
limit = cgroup_limit(host)
ram = limit if limit else host
# The RAM the rental actually allots (vast's portal: 256.8 GB on the 3090, while its cgroup says 342 GiB): EFFECTCRAFT_RAM_GB, else
# the number in <conf>/machine-ram-gb. Decimal GB. Every cache is sized from the smaller of this and the cgroup/host value.
def allotment():
    v = os.environ.get("EFFECTCRAFT_RAM_GB", "").strip()
    if not v:
        try:
            v = open(os.path.join(a.conf, "machine-ram-gb")).read().strip()
        except OSError:
            v = ""
    try:
        return int(float(v) * 1e9) if float(v) > 0 else None
    except ValueError:
        return None
allot = allotment()
if allot and allot < ram:
    ram = allot
cores = os.cpu_count() or 1
g = sh("nvidia-smi --query-gpu=name,memory.total --format=csv,noheader,nounits").splitlines()
gpu_name, vram = (g[0].split(",")[0].strip(), int(g[0].split(",")[1]) * (1 << 20)) if g and "," in g[0] else ("none", 0)

pf = os.path.join(a.conf, "prefs.json")
if not os.path.exists(pf):
    print(f"tune-machine: no {pf} yet (push-private.sh sends it); nothing to tune"); sys.exit(0)
prefs = json.load(open(pf))
home = os.path.dirname(os.path.dirname(os.path.abspath(a.conf)))  # /home/user/.config/effectcraft -> /home/user
default_cache = os.path.join(home, ".cache", "effectcraft", "disk-cache")

def du(path):
    try:
        return int(sh(f"du -sb {json.dumps(path)} 2>/dev/null | cut -f1") or 0)
    except ValueError:
        return 0

def fs_of(path):
    p = path
    while not os.path.exists(p):
        p = os.path.dirname(p) or "/"
    st = os.statvfs(p)
    return os.stat(p).st_dev, st.f_bavail * st.f_frsize, st.f_blocks * st.f_frsize

# Disk cache folder: the default, unless another mount (e.g. a /workspace volume) has more room. Never /dev/shm (that is RAM).
cands = [default_cache] + [os.path.join(m, "effectcraft-disk-cache") for m in ("/workspace", "/data", "/mnt") if os.path.isdir(m)]
best = None
for c in cands:
    dev, avail, size = fs_of(c)
    usable = avail + du(c)          # what the cache holds already is reusable
    if best is None or (dev != best[1] and usable > best[2] * 1.5):
        best = (c, dev, usable, size)
cache_dir, _, disk_usable, disk_size = best

# ---- The longest comp (preview + disk need) ----
projs = [p for p in (a.project or prefs.get("recentProjects", [])) if p.endswith(".ecproj") and os.path.exists(p)][:5]
longest = None  # (bytes at Full, frames, name, project)
for p in projs:
    try:
        d = json.load(open(p))
    except Exception:
        continue
    for it in d.get("items", {}).values():
        k = it.get("kind", {})
        if k.get("type") != "Comp":
            continue
        fr = k.get("frame_rate") or {}
        fps = fr.get("num", 30) / max(fr.get("den", 1), 1)
        frames = int(k.get("duration", 0) / 254_016_000_000 * fps)
        b = frames * k.get("width", 1920) * k.get("height", 1080) * 4
        if longest is None or b > longest[0]:
            longest = (b, frames, it.get("name", "?"), os.path.basename(p))
if longest is None:
    longest = (int(10 * 60 * 29.97) * 1920 * 1080 * 4, int(10 * 60 * 29.97), "(no project found: assumed 10 min 1080p)", "")

xvfb = sys.platform.startswith("linux") and bool(sh("pgrep -x Xvfb"))

# ---- Compute ----
def rule(r):
    share, lo, hi = r
    return min(max(ram * share, lo * GIB), hi * GIB)

reserve_gb = max(math.ceil(ram * (1 - CACHE_SHARE) / GIB), RESERVE_MIN_GB)
cache_total = max(ram - reserve_gb * GIB, 3 * GIB)
# The app caps its caches at MemTotal - reserve and reads the host's MemTotal (not the cgroup), so in a container
# the reserve also covers the RAM the container can't have: the app's own cap then stays inside the limit too.
if ram < host:
    reserve_gb = max(math.ceil((host - cache_total) / GIB), reserve_gb)
layer = rule(LAYER_RULE)
media = rule(MEDIA_RULE)
if layer + media > cache_total * 0.5:                       # small machines: keep half for preview frames
    k = cache_total * 0.5 / (layer + media); layer *= k; media *= k
preview_need = longest[0] * PREVIEW_HEADROOM
preview = max(min(preview_need, cache_total - layer - media), 1 * GIB)

disk_margin = max(DISK_MARGIN[0] * GIB, disk_size * DISK_MARGIN[1])
disk_need = longest[1] * DISK_FRAME_MB * (1 << 20) * DISK_NEED_FACTOR
disk_gb = int(min(max(disk_usable - disk_margin, 0), disk_need) / GIB)
disk_on = disk_gb >= DISK_MIN_GB

mb = lambda b: int(b / (1 << 20))
new = {
    "memory.ramReservedGb": reserve_gb,
    "memory.layerCacheMb": mb(layer),
    "memory.mediaCacheMb": mb(media),
    "memory.previewCacheMb": mb(preview),
    "memory.reduceCacheWhenLow": True,
    "disk.diskCacheEnabled": disk_on,
    "disk.diskCacheMaxGb": max(disk_gb, 1),
    "disk.diskCacheFolder": "" if cache_dir == default_cache else cache_dir,
    "disk.mediaCacheFolder": "",          # Mac paths don't exist here: the default (~/.cache/effectcraft/Media Cache)
    "disk.conformedMediaFolder": "",
    "audio.outputDevice": "",             # the Mac's device names don't exist here: the system default (PipeWire)
    "previews.hardwareProResDecoding": sys.platform == "darwin",  # no hardware ProRes decoder on Linux
    # Zero-copy display shares the window's GPU device. On an Xvfb desktop (Selkies; no DRI3) it is slower
    # (17 vs 29.7 fps uncached on the 3090), keeps cached frames in at most 1 GiB of VRAM instead of the RAM
    # preview cache, and a GPU out-of-memory takes the window down with it. Off = frames are read back into RAM.
    "previews.zeroCopyDisplay": not xvfb,
}

def get(d, key):
    for p in key.split("."):
        d = d.get(p) if isinstance(d, dict) else None
    return d

def put(d, key, v):
    ps = key.split(".")
    for p in ps[:-1]:
        d = d.setdefault(p, {})
    d[ps[-1]] = v

def show(key, v):
    if v is None: return "-"
    if key.endswith("Mb"): return f"{v / 1024:.1f} GB"
    if key.endswith("Gb"): return f"{v} GB"
    if v == "": return "(default)"
    return str(v)

# A value changed by hand since the last tune (in the app's Settings on this machine) is kept, unless --force.
try:
    prev = json.load(open(os.path.join(a.conf, "machine-tune.json")))
except Exception:
    prev = {}
last, hand = prev.get("set", {}), set(prev.get("hand", []))
kept = set()
if not a.force:
    for k in new:
        if k in hand or (k in last and get(prefs, k) != last[k]):
            new[k] = get(prefs, k); kept.add(k)
changes = [(k, get(prefs, k), v) for k, v in new.items() if get(prefs, k) != v]
gb = lambda b: f"{b / GIB:.0f} GB"
head = (f"this machine: {gb(ram)} RAM{' (allotted: EFFECTCRAFT_RAM_GB / machine-ram-gb)' if allot and ram == allot else ''}{f' (container limit; host has {gb(host)})' if limit and ram == limit else ''}, {cores} CPUs, {gpu_name} {gb(vram)} VRAM, disk {gb(disk_size)} "
        f"({gb(disk_usable)} usable for the cache at {cache_dir})")
if a.quiet:
    print(f"tune-machine: {len(changes)} machine settings {'would change' if a.dry_run else 'set'}; caches "
          f"{show('x.Mb', new['memory.layerCacheMb'])} layer / {show('x.Mb', new['memory.mediaCacheMb'])} footage / "
          f"{show('x.Mb', new['memory.previewCacheMb'])} preview, reserve {new['memory.ramReservedGb']} GB, disk "
          f"{str(new['disk.diskCacheMaxGb']) + ' GB' if new['disk.diskCacheEnabled'] else 'off'}{', kept by hand: ' + ', '.join(sorted(kept)) if kept else ''}")
else:
    print(head)
    print(f"longest comp: {longest[2]} {('in ' + longest[3]) if longest[3] else ''}: {longest[1]} frames = {gb(longest[0])} at Full in RAM, "
          f"~{gb(longest[1] * DISK_FRAME_MB * (1 << 20))} on disk")
    print(f"RAM for caches: {gb(cache_total)} of {gb(ram)} ({100 * cache_total / ram:.0f} %); disk margin kept free: {gb(disk_margin)}")
    print(f"{'setting':<34}{'now (Mac sent)':>16}{'this machine':>16}")
    for k, v in new.items():
        old = get(prefs, k)
        print(f"{k:<34}{show(k, old):>16}{show(k, v):>16}{'  kept: changed by hand' if k in kept else '' if old == v else '  *'}")
    if preview < preview_need:
        print(f"note: the longest comp needs {gb(preview_need)} of preview cache; RAM allows {gb(preview)} (Half res needs a quarter)")
    if not disk_on:
        print(f"note: disk cache OFF: {gb(disk_usable)} usable minus {gb(disk_margin)} margin leaves under {DISK_MIN_GB} GB. Fine with a big "
              f"RAM preview cache; for frames that survive restarts rent >= {gb(disk_need + disk_margin + 40 * GIB)} of disk.")
        old_cache = du(cache_dir)
        if old_cache > GIB:
            print(f"note: {gb(old_cache)} of old disk cache in {cache_dir}: Settings > Media & Disk Cache > Empty Disk Cache frees it")
if a.dry_run:
    if not a.quiet: print(f"dry run: {len(changes)} changes, nothing written")
    sys.exit(0)
if changes:
    shutil.copy2(pf, pf + ".before-tune")
    for k, _, v in changes:
        put(prefs, k, v)
    tmp = pf + ".tmp"
    json.dump(prefs, open(tmp, "w"), indent=2)
    os.replace(tmp, pf)
    try:
        st = os.stat(a.conf); os.chown(pf, st.st_uid, st.st_gid)
    except Exception:
        pass
mt = os.path.join(a.conf, "machine-tune.json")
# The cloud frame server's frame cache (CR8; cloud.sh exports it): what the app's caches leave of the cache total (no double
# count with the browser desktop app's preview cache; serve's own 1 GiB layer cache counted), at least 2 GiB, and the disk
# cache's room in a folder of its own.
serve_frames = max(cache_total - layer - media - preview - GIB, 2 * GIB)
serve = {"framesMb": mb(serve_frames), "framesDiskGb": disk_gb if disk_on else 0, "framesDir": os.path.join(os.path.dirname(cache_dir), "server-frames")}
json.dump({"at": time.strftime("%F %T"), "ram": ram, "hostRam": host, "cgroupLimit": limit, "cores": cores, "gpu": gpu_name, "vram": vram, "diskSize": disk_size,
           "diskUsable": disk_usable, "cacheDir": cache_dir, "longestComp": longest[1:], "set": new, "hand": sorted(kept), "serve": serve},
          open(mt, "w"), indent=1)
try:
    st = os.stat(a.conf); os.chown(mt, st.st_uid, st.st_gid)
except Exception:
    pass
if not a.quiet:
    print(f"wrote {len(changes)} machine settings to {pf} (previous: prefs.json.before-tune); restart EffectCraft to apply" if changes
          else "nothing to change")
PY
