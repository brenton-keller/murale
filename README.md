# murale

murale plays a video as the desktop wallpaper on Wayland compositors that support `wlr-layer-shell`. It is written in Rust and uses libmpv to decode and render the video.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

![murale demo](docs/demo.gif)

## Why murale exists

[mpvpaper](https://github.com/GhostNaN/mpvpaper) is an existing video wallpaper player for Wayland. Its issue tracker has open reports of memory growth during long playback ([#101](https://github.com/GhostNaN/mpvpaper/issues/101), and [#117](https://github.com/GhostNaN/mpvpaper/issues/117), which reports 28 GB after about 12 hours) and of a segfault when monitors disconnect ([#97](https://github.com/GhostNaN/mpvpaper/issues/97)).

On NVIDIA (driver 580.142), murale's rendering process also grows in memory, but only while it is presenting frames. One renderer ran for 32.8 hours and rose from 589.2 MB to 1,273.9 MB. All of that growth happened in the 3.7 hours the monitor was on, at 155 to 188 MB per hour of 240 fps playback, and memory stayed flat for the 29 hours the monitor was off. A heap profile of an earlier build traces the largest per-frame allocations to libwayland-client calls made by NVIDIA's EGL Wayland library. Starting a new renderer process brings memory back down: in another run, one renderer reached 918.1 MB before its handoff and its replacement started at 619.8 MB. That is why murale replaces the renderer on a timer, as described under [Recycling](#recycling).

## Install

Build from source with a Rust toolchain:

```bash
git clone https://github.com/brenton-keller/murale.git
cd murale
cargo build --release
```

The binary is `target/release/murale`. Copy it somewhere on your `PATH` if you want to call it as `murale`.

The build links against libmpv, libwayland-client and libwayland-egl, and finds the Wayland libraries through pkg-config. At runtime murale also loads `libEGL.so.1`, which your GPU driver or libglvnd provides.

Arch Linux:

```bash
sudo pacman -S mpv wayland pkgconf
```

Debian and Ubuntu:

```bash
sudo apt install libmpv-dev libwayland-dev pkgconf
```

Fedora:

```bash
sudo dnf install mpv-devel wayland-devel pkgconf-pkg-config
```

Your compositor must support `wlr-layer-shell`; murale exits with an error if it does not. According to the [protocol's support table](https://wayland.app/protocols/wlr-layer-shell-unstable-v1), Hyprland, Sway, river, Wayfire and KWin support it and GNOME's Mutter and Weston do not. murale has only been tested on Hyprland.

See [mpv version](#mpv-version) before choosing an mpv package.

## Usage

```bash
murale ~/Videos/wallpaper.mp4
```

This starts murale in the foreground. With no `--output`, the compositor picks which monitor the wallpaper goes on.

To put it on a specific monitor, find the output name and pass it:

```bash
murale --list-outputs
murale ~/Videos/wallpaper.mp4 --output DP-1
```

To start murale with Hyprland, add an `exec-once` line to your Hyprland config:

```ini
exec-once = murale ~/Videos/wallpaper.mp4 --output DP-1
```

If murale is not on Hyprland's `PATH`, use the full path to the binary.

## Flags

| Flag | Meaning |
|---|---|
| `<VIDEO>` | Video file to play. Any format mpv can open. With `--workspace`, this is the video for workspaces that have no mapping. |
| `-o`, `--output <NAME>` | Draw on the named output, for example `DP-1` or `HDMI-A-1`. The name must be 1 to 100 bytes. |
| `--list-outputs` | Print the names of the connected outputs, one per line, and exit. No video argument is needed. |
| `-w`, `--workspace <N=PATH>` | Play `PATH` while Hyprland workspace `N` is active. Repeat the flag for more workspaces. |
| `--mpv-options <OPTIONS>` | Set mpv options. See below. |
| `--stats` | Log frame timing and playback counters every 5 seconds. |
| `--scanout` | Log a `wp_presentation` scanout timestamp for every frame. Stops after 60 seconds. |
| `--no-recycle` | Run as a single process without the supervisor. See [Recycling](#recycling). |
| `-h`, `--help` | Print help. |

There is no `--version` flag.

If no connected output has the name given to `--output`, murale logs the names it can see and waits for that output to appear. This also covers a monitor that is asleep when murale starts. While it waits, the renderer reports to the supervisor every 30 seconds, and the supervisor waits as long as those reports keep coming. `supervisor.log` gets a `waiting for output (renderer alive, available: [<names>])` line when the wait starts and every 10 minutes after, and the renderer log gets a `still waiting for target output` warning every 10 minutes. If the renderer crashes while the output is away, the renderer the supervisor starts in its place waits the same way. With `--no-recycle`, murale also waits.

`--scanout` only logs anything if the compositor offers the `wp_presentation` protocol.

### mpv options

`--mpv-options` takes `key=value` pairs separated by commas:

```bash
murale video.mp4 --mpv-options "hwdec=vaapi,mute=yes"
```

murale splits the string on commas only. Spaces are not separators, so `"hwdec=vaapi mute=yes"` becomes a single option named `hwdec` with the value `vaapi mute=yes`. Because commas always split, an option value cannot itself contain a comma. A key with no `=` is set to `yes`, and the bare word `loop` is turned into `loop-file=inf`. If mpv rejects an ordinary option, murale logs a warning and continues. An invalid `config`, `config-dir` or `profile` value stops murale with an error.

murale sets these mpv options before applying yours:

| Option | Value |
|---|---|
| `vo` | `libmpv` |
| `hwdec` | `auto` |
| `video-sync` | `desync` |
| `loop-file` | `inf` |
| `keepaspect` | `yes` |
| `panscan` | `1.0` (scale to fill the screen, cropping the edges) |
| `background-color` | `#00000000` |
| `terminal` | `no` |

You can override all of them except `vo`, which murale sets back to `libmpv` after your options are applied.

libmpv does not read mpv's config files by default, so `~/.config/mpv/mpv.conf` is ignored unless you pass `config=yes`. Add `config-dir=PATH` to use a different directory. Both are applied before mpv initializes. `profile=NAME` applies an mpv profile after initialization; a profile you defined yourself also needs `config=yes`, or mpv will not find it.

## Multi-monitor

Run one murale per output, each with its own `--output`:

```ini
exec-once = murale ~/Videos/left.mp4 --output DP-1
exec-once = murale ~/Videos/right.mp4 --output DP-2
```

Each output gets its own lock file and log directories (see [Logs](#logs)). A second murale started for an output that already has one exits with the error `another murale supervisor is running`, which is written to that output's `supervisor.log` (see [Logs](#logs)). Instances started without `--output` all share the namespace `default`, so only one of them can run. `--no-recycle` instances do not take the lock.

When `--output` is set and that output disconnects, murale removes its surface and pauses. While it is disconnected, murale ignores workspace events and the supervisor does not start handoffs. When an output with the same name connects again, murale recreates the surface and resumes. Without `--output`, murale does not follow reconnects.

## Per-workspace videos

On Hyprland, murale can switch videos when the active workspace changes:

```bash
murale ~/Videos/default.mp4 \
  --workspace 1=~/Videos/forest.mp4 \
  --workspace 2=~/Videos/ocean.mp4
```

The part before `=` is matched against Hyprland's workspace name, which for numbered workspaces is the number. Workspaces without a mapping play the default video. When a workspace event arrives, murale compares the video for that workspace with the file mpv is playing and loads it only if they differ.

Your shell does not expand `~` after `=`, so murale does it: a path that starts with `~/` has `~` replaced with `$HOME`, and a relative path is made absolute against the directory murale was started from. This applies to the `<VIDEO>` argument and to every `--workspace` path.

murale listens on Hyprland's event socket and reacts to `workspace>>` events. That event reports the focused workspace on any monitor, so with one murale per monitor, every instance switches when focus moves to a mapped workspace, whichever monitor it is on.

Switching uses mpv's `loadfile`, so the new video starts from the beginning.

During a recycling handoff (see [Recycling](#recycling)), the new renderer starts with the video the old renderer was playing when the handoff began, and it ignores workspace events until the supervisor has finished checking it. If you switch workspaces during a handoff, the screen shows the video from before the switch for up to about 10 seconds after the new renderer takes over. In two logged handoffs, the supervisor finished its checks 10.0 seconds after the new renderer took over the screen. At that point the new renderer asks Hyprland for the active workspace and, if the file playing is not that workspace's video (or the default video, for a workspace with no mapping), loads it. It then starts listening for workspace events.

murale makes the same check when it starts. When a monitor named by `--output` reconnects, it makes it straight away and again 1 second later. Each check logs a line starting with `startup:`, `unlock:` or `reconnect:` that names the active workspace and says whether a different video was loaded. After a handoff the new renderer logs an `unlock:` line followed by a `startup:` line. The second one reports `already correct` unless the active workspace changed between the two checks, which were 9 ms apart in one logged handoff. This corrects the case where, after a monitor wakes, Hyprland reports the monitor's previous workspace instead of the one you are on. Each correction restarts the video from the beginning.

Workspace switching needs `HYPRLAND_INSTANCE_SIGNATURE` in murale's environment, which processes started by Hyprland inherit, and `hyprctl` on its `PATH`. If the variable is missing, murale logs one warning at startup, does not run any of these checks, and plays only the default video.

## Recycling

By default `murale` starts a supervisor process, which starts the renderer process that does the drawing. After every 2 hours of on-screen time, the supervisor starts a second renderer with the video the current one is playing, seeks it about 0.8 seconds ahead of the current position and holds it there. When the old renderer reaches that position, it signals the new one from its frame callback, and the new one shows its frame and starts playing.

The supervisor then watches the new renderer for 10 seconds, checking only that the process is still running. After that it checks that the new renderer is playing the file it was started with. It compares against that starting file and not against whatever the old renderer is playing now, so a workspace switch handled by the old renderer during the handoff does not fail the check. If the check passes, the supervisor lets the new renderer start following workspace changes and stops the old renderer.

In two logged handoffs, the time from starting the new renderer to stopping the old one was 12.4 and 12.3 seconds. Both renderers are in memory for that time.

To change the interval, set `MURALE_REFRESH_INTERVAL` to a number of seconds. A value that is not a whole number is ignored and the 2 hour default is used.

```bash
MURALE_REFRESH_INTERVAL=3600 murale ~/Videos/wallpaper.mp4
```

On-screen time is the time murale's output has been connected since the current renderer started. When the output disconnects, for example because the monitor went to sleep, the supervisor stops counting and does not start a handoff until the output is back. When the output comes back, the supervisor waits 60 seconds and then hands off if the interval has run out.

Before starting the new renderer, the supervisor checks that the system has at least 50 MiB of available memory, at least 50 MiB free in the runtime directory, at least 3 GiB of free video memory when `/dev/nvidia0` exists, and no kernel out-of-memory messages in the last 5 minutes. When the checks pass it logs the measured values:

```
resource gate: vram_free=6060MiB mem_avail=33831960kB tmpfs_free=6354MiB
```

These checks run once per handoff, before the new renderer starts. Nothing checks free memory or video memory while both renderers are running, so the 3 GiB threshold is the only headroom kept for the second renderer.

If a check fails, or the handoff itself fails, the current renderer keeps running and the supervisor waits before trying again. The wait starts at 10 minutes and doubles with each further failure in a row, but is never longer than the recycle interval: with the default 2 hour interval it is 10, 20, 40, 80 and then 120 minutes. A successful handoff resets it, and a handoff put off because the output was disconnected does not count as a failure. The supervisor logs `next attempt in <age>` when it schedules the retry.

If the renderer exits unexpectedly, the supervisor starts a new one, waiting 2, 4 and then 8 seconds before each attempt. After three failed attempts the supervisor exits. A renderer started this way begins with the default video and, with `--workspace`, then makes the startup workspace check.

Stopping the supervisor with SIGTERM or SIGINT stops the renderer. If the supervisor dies without cleaning up, the renderer notices that its control channel has closed and exits.

`--no-recycle` runs a single renderer with no supervisor. That process instead rebuilds its mpv and EGL state in place every 2 hours, and sending it SIGUSR1 triggers a rebuild immediately. `MURALE_REFRESH_INTERVAL` has no effect in this mode.

### Memory use

With the default 2 hour interval, a renderer reached 967 MB after 2 hours of continuous rendering, and its replacement started at 577 MB.

These figures come from two test runs on one machine (NVIDIA RTX 5080, driver 580.142, Hyprland 0.54.3), playing a 3840x2160 HEVC video at 240 fps on a 7680x2160 output at 240 Hz. Memory is resident plus swapped memory, read from the renderer's `[mem]` lines and the supervisor's `handoff summary` lines.

| Run | Renderer | Start | End | Time rendering | Ended by |
|---|---|---|---|---|---|
| 1 (2026-10-16) | 1 | 586 MB | 967 MB | 2 h 00 m | handoff |
| 1 (2026-10-16) | 2 | 577 MB | 952 MB | 1 h 53 m | monitor sleep |
| 2 (2026-10-14) | 1 | 577 MB | 916 MB | 2 h 00 m | handoff |
| 2 (2026-10-14) | 2 | 577 MB | 982 MB | 2 h 00 m | handoff |
| 2 (2026-10-14) | 3 | 583 MB | 971 MB | 2 h 00 m | handoff |

Start is the first 30-second reading after the renderer began drawing. End is the last reading before the handoff or before the monitor went to sleep. Across these renderers memory grew by about 170 to 200 MB per hour of rendering, and each new renderer started again near 580 MB.

Memory does not grow while the monitor is asleep. Run 1's second renderer stayed at 951.6 MB for the first 9.5 hours of an 11 hour sleep. After that the system reclaimed part of its memory, and at its handoff, 13 hours after it started, the supervisor logged 837 MB, of which 448 MB was in swap. That figure reflects the time asleep, not 13 hours of rendering.

## Logs

In the default mode, murale writes logs to files, not to the terminal. `<ns>` below is `default` when `--output` is not given, and otherwise `out-` followed by the output name in hex. For `--output DP-4` that is `out-44502d34`.

The supervisor's log is kept on disk so that it survives a reboot:

```
${XDG_STATE_HOME:-$HOME/.local/state}/murale/<ns>/supervisor.log
```

When murale starts and this file is larger than 2 MiB, it is renamed to `supervisor.log.1`, replacing any older `supervisor.log.1`. While murale runs, a log larger than 2 MiB is cut down to its last 1 MiB. If the file cannot be opened, murale prints `warning: persistent log path failed, using tmpfs: <dir>` to stderr and writes `supervisor.log` to the runtime directory below instead.

Renderer logs are written to the runtime directory, which is a tmpfs and does not survive a reboot:

```
$XDG_RUNTIME_DIR/murale/<ns>/logs/
```

It contains `gen-<N>-<supervisor pid>.log`, one plain-text log per renderer, and `failed/`, where the log of a renderer that crashed or failed a handoff is moved. At startup and after each successful handoff, the supervisor removes renderer logs older than 3 days, and the oldest ones once they total more than 64 MiB. The running renderer's log is never removed. Logs in `failed/` are kept for 7 days or up to 16 MiB.

When a renderer log is moved to `failed/`, its last 1 MiB is also copied to a `failed/` directory next to `supervisor.log`, so it survives a reboot. A copy that was cut short starts with the line `--- head truncated at byte <n> ---`. That directory is pruned after each copy to 7 days or 16 MiB.

With `--no-recycle`, everything goes to stderr instead.

murale logs at `info` level by default. Set `RUST_LOG` to change that, for example `RUST_LOG=murale=debug`.

### Supervisor log lines

Every handoff attempt ends with one `handoff summary` line, so `grep 'handoff summary' supervisor.log` lists the recycling history:

```
handoff summary: gen=<N> result=ok path="<file>" consecutive=<K> renderer_age=<age> mem=<MB>MB (rss=<MB> swap=<MB>) log=<KB>KB validated=true
handoff summary: gen=<N> result=fail reason="<reason>" consecutive=<K> renderer_age=<age> mem=<MB>MB (rss=<MB> swap=<MB>) log=<KB>KB
handoff summary: gen=<N> result=gate_skip reason="<reason>" consecutive=<K> renderer_age=<age> mem=<MB>MB (rss=<MB> swap=<MB>) log=<KB>KB
handoff summary: gen=<N> result=deferred reason="target output disconnected" consecutive=<K> renderer_age=<age> mem=<MB>MB (rss=<MB> swap=<MB>) log=<KB>KB
```

`ok` and `deferred` lines are logged at `INFO`, `fail` and `gate_skip` at `WARN`. On an `ok` line, `gen` is the new renderer's generation, while `renderer_age`, `mem` and `log` describe the renderer being retired. On the other lines they all describe the current renderer. `mem` is resident plus swapped memory. `consecutive` counts `fail` and `gate_skip` results in a row; an `ok` result resets it and a `deferred` result leaves it unchanged. A line from a test run, with the video path replaced:

```
handoff summary: gen=8 result=ok path="/home/user/Videos/wallpaper.mp4" consecutive=0 renderer_age=1m54s mem=591MB (rss=591 swap=0) log=2KB validated=true
```

A failed handoff is also logged as `handoff failed (consecutive=<K>, renderer_age=<age>): <reason>` at `WARN`. From the third failure in a row it is logged at `ERROR` and includes `mem=<MB>MB`. A failed resource check is logged as `resource gate: <reason> (consecutive=<K>, renderer_age=<age>)` at `WARN`.

If the current renderer's on-screen time has passed three times the recycle interval when a handoff is about to be attempted, the supervisor logs at `ERROR` before the attempt:

```
recycle stale: on_screen=<age> renderer_age=<age> (3× interval=<age>), mem=<MB>MB (rss=<MB> swap=<MB>), consecutive=<K>
```

The supervisor also logs `output disconnected` and `output reconnected (off for <h>h <m>m)`, and `persisted failed log: <name> (<KB>KB)` when it copies a failed renderer log.

### Renderer log lines

At the default level a renderer logs startup and playback events, plus a memory line every 30 seconds:

```
[mem] periodic rss=319.1MB swap=205.9MB total=524.9MB elapsed=2370s
```

Each time the surface is created, configured or changes scale, the renderer logs its size:

```
surface: created output=DP-4 (mode=7680x2160)
surface: configure logical=7680x2160 scale=1 buffer=7680x2160 output=DP-4 (mode=7680x2160)
```

With `--output`, the renderer compares the buffer size with the output's current mode every 30 seconds. If they differ on two checks in a row, it logs `scale mismatch: buffer=<W>x<H> (logical=<W>x<H> scale=<S>) != mode=<W>x<H>` at `WARN` once, and `scale mismatch cleared: buffer=<W>x<H> matches mode` when they match again.

With `--workspace`, each `workspace>>` event from Hyprland is logged with what murale did about it:

| `action=` | Level | Meaning |
|---|---|---|
| `loaded` | `INFO` | murale loaded the workspace's video. |
| `load-failed` | `WARN` | mpv did not accept the load. |
| `ignored-disconnected` | `INFO` | The output was disconnected, so the event was ignored. |
| `same-video` | `DEBUG` | The video was already playing. |
| `deferred-hold` | `DEBUG` | The renderer was being held during a handoff. |
| `skipped-mpv-idle` | `DEBUG` | mpv had no file loaded. |
| `skipped-no-handle` | `DEBUG` | mpv was not running. |

For example:

```
[ws] event="workspace>>3" action=loaded video="<file>" believed="<file>"
```

`believed` is the file mpv reported as playing when the event arrived. Other Hyprland events are logged at `TRACE`.

With `--workspace`, every 30 seconds the renderer also checks which workspace is active and whether mpv is playing that workspace's video. With `--output`, it asks Hyprland for the workspace shown on that output; without it, for the focused workspace. If the same mismatch is seen on two checks in a row, it logs at `WARN`, and again every 5 minutes while the mismatch lasts:

```
[ws] mismatch: output=<name> active_ws=<N> expected="<file>" playing="<file>" since=<duration>
```

`playing="<none>"` means mpv had no file loaded. When the mismatch ends, the renderer logs `[ws] mismatch cleared (duration=<duration>)`. On more than one monitor this check can disagree with workspace switching; see [Known limitations](#known-limitations).

If the thread that listens for workspace events stops, the renderer logs `[ws] listener exited — respawning` within 30 seconds and starts a new one.

### Stats output

With `--stats`, the renderer logs a line in this format every 5 seconds, followed by a memory line and a line of mpv video properties:

```
[stats] fps=<f> delayed=<n> vo_drops=<n> dec_drops=<n> vsync_ratio=<f> | interval avg=<ms>ms min=<ms>ms max=<ms>ms jitter=<ms>ms
```

It also logs a memory line every 1000 frames.

`interval` and `jitter` measure the time between successive frames that murale renders. `jitter` is the standard deviation of those intervals. On a 240 Hz display, a steady 4.17 ms average means one frame per refresh.

`fps` is mpv's estimate of the video's frame rate after filtering.

`vo_drops` counts frames that mpv decoded but did not render because they were already late. This is mpv's default `framedrop=vo` behaviour, which murale does not change.

`dec_drops` counts frames the decoder dropped.

`delayed` and `vsync_ratio` are counters from mpv's display-sync modes. murale uses `video-sync=desync`, so these two fields do not describe murale's frame pacing.

## IPC

mpv's JSON IPC is available only when murale runs with `--no-recycle`. It does not work in the default mode. In the default mode the supervisor removes `input-ipc-server` from the options it passes to each renderer, because two renderers run at once during a handoff and cannot share one socket path. It logs this at `WARN` in `supervisor.log`:

```
input-ipc-server is not supported in recycling mode; ignored (use --no-recycle for mpv IPC)
```

```bash
murale --no-recycle ~/Videos/wallpaper.mp4 --mpv-options "input-ipc-server=/tmp/murale.sock"

echo '{"command": ["loadfile", "/path/to/other.mp4"]}' | socat - /tmp/murale.sock
echo '{"command": ["set_property", "pause", true]}' | socat - /tmp/murale.sock
echo '{"command": ["get_property", "time-pos"]}' | socat - /tmp/murale.sock
```

## Known limitations

### Workspace switching on more than one monitor

Hyprland's `workspace>>` event reports the focused workspace, whichever monitor it is on, and every murale instance switches on it. The 30-second mismatch check works differently: with `--output`, it asks Hyprland for the workspace shown on that output. On two monitors, when focus moves to a workspace on the other monitor, an instance loads that workspace's video, and after two checks it logs a `[ws] mismatch` warning, repeated every 5 minutes while focus stays there.

### Paths with `..` or `~user/`

murale expands only a leading `~/`. A path starting with `~user/`, or a bare `~`, is treated as a relative path and made absolute against the directory murale was started from, so mpv cannot open it.

murale keeps `..` in a path as written, but mpv resolves it when it loads the file. The path murale expects then never matches the path mpv reports, so murale reloads that video, restarting it, on every workspace event that selects it and on every startup, handoff or reconnect check, and the mismatch check logs a warning. Paths without `..` are not affected.

### Old video for up to about 10 seconds after a handoff

If you switch workspaces while a handoff is in progress, the new renderer shows the video from before the switch until the supervisor has finished checking it, about 10 seconds after it takes over the screen. See [Per-workspace videos](#per-workspace-videos).

### Repeated warning while the monitor sleeps

In the default mode, while a monitor named by `--output` is asleep or disconnected, the renderer log gets a `still waiting for target output` warning every 10 minutes. This is expected and does not need any action.

## Troubleshooting

### murale exits immediately and prints nothing

In the default mode, startup errors go to `supervisor.log` (see [Logs](#logs)), not the terminal. Three errors you may see there:

- `another murale supervisor is running`: another murale is already running for the same output.
- `XDG_RUNTIME_DIR must be set`: murale was started without that variable.
- `initial display-ready`: the first renderer exited, or stopped responding, before it drew its first frame.

### I can't find the renderer in the process list

The renderer runs under the name `murale-render`, and the supervisor under `murale`. `pgrep -a murale-render` lists the renderers.

### The wallpaper is on the wrong monitor

Run `murale --list-outputs` and check the name you passed to `--output`. If the name does not match, murale keeps waiting for it, and `supervisor.log` has a line starting `waiting for output (renderer alive, available:` that lists the names murale saw.

### Handoffs stop happening

Run `grep 'handoff summary' supervisor.log`. Each line's `result` shows whether that attempt succeeded, failed (`fail`), was blocked by a resource check (`gate_skip`) or was put off because the output was disconnected (`deferred`), and `reason` says why. After a failure, a `next attempt in` line says when the supervisor will try again. An `ERROR` line starting `recycle stale:` means the current renderer has gone more than three intervals of on-screen time without a successful handoff. `INFO` lines starting with `resource gate:` are checks that passed and show how much headroom there was. The checks are listed under [Recycling](#recycling).

### My mpv config is ignored

Pass `config=yes` in `--mpv-options`. See [mpv options](#mpv-options).

### mpv version

libmpv's OpenGL renderer had a file descriptor leak involving fences ([mpv#17217](https://github.com/mpv-player/mpv/issues/17217)). It affects programs that render through libmpv's OpenGL render API, which murale does. The fix is [mpv PR #17303](https://github.com/mpv-player/mpv/pull/17303), merged after mpv 0.41.0 was released. mpv 0.41.0 and earlier do not have it, so use a newer release or a git build.

On CachyOS, `mpv-git` is in the repositories. On Arch, it is in the AUR. To go back to the packaged release, install `mpv` again.

## Tested on

The measurements in this README were taken on:

- murale 1.2.0 and the 1.3.0 release candidate (commit 00f73fd)
- AMD Ryzen 9 5950X
- NVIDIA GeForce RTX 5080, driver 580.142
- Samsung Odyssey G95NC at 7680x2160, 240 Hz
- Hyprland 0.54.3
- mpv-git 0.41.0_703_gf5d4d9b029
- A 3840x2160 HEVC video at 240 fps

## License

[MIT](LICENSE)
