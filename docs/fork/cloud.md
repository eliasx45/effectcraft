# One-command cloud preview (CR10)

`scripts/fork/cloud.sh` connects the Mac app to a vast.ai GPU machine **Elias rented himself**.
Fork-only (keep it out of upstream pull requests). It never rents, stops or destroys an instance:
destroying stays manual in the vast.ai console.

```bash
scripts/fork/cloud.sh connect root@<ip> -p <port>   # everything below, in one go
scripts/fork/cloud.sh status                        # tunnel, server commit, GPU, free VRAM/disk, pending uploads
scripts/fork/cloud.sh disconnect                    # app back to This Mac, tunnel closed, server process stopped
scripts/fork/cloud.sh tunnel                        # reopen the tunnel + start its watchdog only (no build, no server restart)
```

Open the project in the Mac app first (the mirror reads the project from the app on control port 9877),
or pass `--project "/path/10-10 Edit.ecproj"`.

## What `connect` does

1. **Machine check.** GPU, free VRAM, free disk, `/workspace/effectcraft` and `cargo` present, plus the
   remote-gpu skill's `check.sh` (informational).
2. **Matching `effectcraft-cli`.** The Mac's `main` commit is compared with the commit the machine's
   `target/local/effectcraft-cli` was built from (`target/local/.built-commit`). If different, only the
   missing git commits are pushed over ssh (a few MB, not GitHub, no binary), the checkout is
   fast-forwarded, and `cargo build --profile local -p effectcraft-cli` runs there in the background
   (`nice`, CPUs 100-111, `EC_BUILD_CPUS` to change). Progress is printed; log: `/workspace/cloud-build.log`.
   If the machine's checkout can't fast-forward (local changes, other branch) it stops and says so.
3. **Mirror (CR7).** `effectcraft-cli mirror`: files up to 100 MB are sent once and deduplicated by hash,
   fonts too; files over 100 MB are never sent from the Mac. If one has no Google Drive link the run
   stops and lists it. Paste the link in `<project>.cloud.json` (`"driveLinks": {"file name": "link"}`) or
   run `cloud.driveLinks {"set": {"file name": "link"}}` in the app, then run `connect` again. Ends with
   `cloud.preflight` on the machine.
   **Motion Library cards** (files under the Mac's library cache, `…/Motion Library/blobs/<sha256>/<name>`)
   are never uploaded: `effectcraft-cli library-pull` on the machine fetches each blob from the library's
   Worker (R2) with the read token, checks sha256 + size and puts it at the same path. If that fails (no
   token there, Worker down, an old `effectcraft-cli`) the mirror says so and uploads those files from the
   Mac instead (still only up to 100 MB). The JSON result carries `fromLibrary` / `libraryBytes`.
4. **Frame server.** `effectcraft-cli serve --gpu --frames <port>` under `nohup` (also serves the CR6a
   H.264 playback stream), `EFFECTCRAFT_FRAME_GPU_SLOTS=4`, the mirror's `cloud.env` (fonts, CJK locale).
   Skipped when it already runs at the right commit with the same token. Log:
   `/workspace/cloud-serve-<port>.log`.
5. **Tunnel + app.** `ssh -M -S … -L 127.0.0.1:<port>:127.0.0.1:<port>` (`ExitOnForwardFailure`,
   `ServerAliveInterval`), then the running app gets `previews.cloudAddress` and `previews.previewOn = cloud`
   through its control channel. If the app isn't running it prints what to set in
   Settings ▸ Previews ▸ Cloud Preview.

Running `connect` twice is safe: finished steps are skipped (build, mirror bytes, server, tunnel).

## Tunnel watchdog (CR11)

`connect` (and `tunnel`) start a watchdog: a small loop (`cloud.sh _watch`, its own session, so closing
the terminal or the agent that ran connect doesn't stop it) that checks the ssh ControlMaster every 2 s
and reopens it when it's gone. It never rebuilds, mirrors or restarts the server.

- **Drops it fixes:** a Wi‑Fi blip or network stall (the tunnel's ssh uses 5 s keepalives, so a dead
  link is noticed in ~15 s; reopening is retried every 2 s, each try with a 5 s connect timeout),
  Mac sleep (after a wake, a gap over 30 s, it reopens at once instead of waiting for ssh), a killed
  tunnel (`ssh -S … -O exit`).
- **Machine gone:** after 20 s of "Connection refused" (or 60 s of no answer) it says the instance is
  most likely paused (outbid, for interruptible ones) or stopped, and retries every 15 s, then every
  60 s after 10 min, so it reconnects by itself if the instance resumes. It never rents or resumes anything.
- **Deliberate disconnect:** `disconnect` stops the watchdog before closing the tunnel, and the
  watchdog exits by itself when the state file is removed or names another machine.
- **Where to see it:** `cloud.sh status` (🐕 line: drops reopened, or down for N s with the reason);
  log `…/EffectCraft/cloud-watchdog.log` (each drop, failed attempt and recovery with ssh's error).
- **The app:** its frame client retries at least every 2 s, and the Preview On chip says
  `Preview: Mac (reconnecting…)` (not "cloud offline") after a connection it had is lost. Once the
  tunnel is back, frames come from the cloud again within ~2–4 s.
- An existing connection made before CR11 has no watchdog: run `cloud.sh tunnel` once.

## Files and secrets

- Connection state (ip, ports, no secrets): `~/Library/Application Support/EffectCraft/cloud-connection.env`
  (so `status` and `disconnect` need no arguments).
- Frame token: `…/EffectCraft/cloud-frame-token` (mode 600, made on the first connect; the app reads it, CR5).
  It reaches the server as an environment variable of the serve process only (root on the machine can read
  it in /proc, which is fine on a machine you rent alone; it is passed once as an ssh argument).
- Motion Library read token: `connect` sends `…/EffectCraft/motion-library.read.json` (never the publish
  token `motion-library.json`; it refuses when both hold the same token) over ssh **stdin** into
  `~/.config/effectcraft/motion-library.json` for root (library-pull) and for `user` (the browser desktop
  app's Motion Library panel), mode 600. Not in a command line, a log or the vast template.
- A build needs about 12 GB free on /workspace: below that `connect` refuses and says what to free.
- The tunnel's control socket: `/tmp/ec-cloud-<uid>.sock` (`EC_CLOUD_SOCK`; macOS limits it to 104 characters).
  Watchdog: `cloud-watchdog.pid`, `.log`, `.state` next to the state file. `EC_CLOUD_SSH_OPTS` adds ssh
  options to the tunnel (tests use a `ProxyCommand` there to fake a stalled network).
- `EFFECTCRAFT_CONFIG_DIR` moves all of the above (tests use it); ssh key `~/.ssh/id_ed25519`
  (`EC_CLOUD_KEY`); `EC_CLI` picks the Mac's `effectcraft-cli`.

## Frames stay on the server (CR8)

Elias's home internet is metered: only the frames he looks at should cross it.

- **Cache Frames When Idle** (Composition ▸ Preview) with Preview On = Cloud sends the server *one* plan: the
  work area from the playhead, then the rest of the comp, at the viewer's resolution. The server renders it
  into **its own frame cache** at the lowest priority (2 frames at once, `EFFECTCRAFT_PRECACHE_THREADS`; paused
  while a playback stream runs). Nothing is downloaded. In local mode the setting works as before.
- The server reports what it holds as ranges about once a second; the timeline's **blue cache bar** shows the
  frames cached on the server (green is still this Mac's RAM).
- Scrubbing: every request names its frame key, so a cached frame comes back encode-only (no render). Frames
  the Mac shows stay in its own RAM cache.
- Playback (CR6b stream): full-size chunks take cached frames and keep the frames they render.
- Keys are the Mac's disk-cache keys (`CacheSeries::key` → `disk_cache::frame_key`): an edit changes the comp's
  content key, so old frames are never served for new content; they age out of the server's cache.
- Size: `EFFECTCRAFT_SERVER_FRAMES_MB` (RAM) and `EFFECTCRAFT_SERVER_FRAMES_DISK_GB` / `_DIR` (spill to disk;
  0 = off). `connect` exports them from `ec-tune`'s `machine-tune.json` (`"serve"`: what the browser desktop app's
  caches leave of 85 % of RAM, so nothing is counted twice, and the disk cache's room); without it serve takes 85 % of
  RAM less 8 GB, and no disk.
- **RAM = the rental's allotment.** vast's portal figure (256.8 GB on the 3090) is smaller than the container's cgroup
  limit (342 GiB): put it in `/home/user/.config/effectcraft/machine-ram-gb` (decimal GB; or `EFFECTCRAFT_RAM_GB`).
  ec-tune and serve size every cache from the smaller of it and the cgroup/host value.
- **Internet use:** the Preview dropdown's tooltip, `ui.inspect` (`cloud.bytesDown`, `cloud.bytesUp`,
  `cloud.serverCache`) and `cloud.sh status` show MB down/up this session (every byte on the wire).

## Options

`--frames-port N` (default 9878), `--local-port N`, `--app-port N` (default `$EFFECTCRAFT_CONTROL_PORT` or 9877),
`--project F.ecproj`, `--no-mirror`, and:

`--idle MIN` (off by default): a small watchdog on the machine stops the **serve process** (never the
instance, which keeps billing) after MIN minutes with no app connected. Re-run `connect` to restart it.

## Tested

3090, 17 Oct 2026: connect from a cold checkout (build, mirror, serve, tunnel), second connect (nothing
repeated), `status`, app copy on its own control port showing `connected / ready`, idle stop, `disconnect`.

18 Oct 2026 (CR11, watchdog): the A40 was paused (refused ssh), so the chain ran on the Mac (lane app
→ watchdog tunnel via `ssh localhost` through a pausable ProxyCommand → `effectcraft-cli serve --frames`).
Tunnel killed with `-O exit`: preview back on Cloud in 3.3 s (chip showed "reconnecting…"). 51 s network
stall: ssh noticed after ~17 s, preview back on Cloud 1.8 s after the network returned. Watchdog stopped for
40 s (a sleep): reopened 3 s after resuming. Refused port: retried, then named the paused-instance cause.
`disconnect`: watchdog stopped, nothing reopened.
