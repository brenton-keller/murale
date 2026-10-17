# Troubleshooting

Most problems show up in murale's logs, so this page starts with where they are.

## Where the logs are

In the default mode, murale writes logs to files, not to the terminal. `<ns>` below is `default` when `--output` is not given, and otherwise `out-` followed by the output name in hex. For `--output DP-4` that is `out-44502d34`.

The supervisor's log is kept on disk so that it survives a reboot:

```
${XDG_STATE_HOME:-$HOME/.local/state}/murale/<ns>/supervisor.log
```

If that path cannot be opened, murale prints `warning: persistent log path failed, using tmpfs: <dir>` to stderr and writes `supervisor.log` to the runtime directory below instead.

Renderer logs are written to the runtime directory, which is a tmpfs and does not survive a reboot:

```
$XDG_RUNTIME_DIR/murale/<ns>/logs/
```

It contains `gen-<N>-<supervisor pid>.log`, one plain-text log per renderer, and `failed/`, where the log of a renderer that crashed or failed a handoff is moved.

With `--no-recycle`, everything goes to stderr instead.

murale logs at `info` level by default. Set `RUST_LOG` to change that, for example `RUST_LOG=murale=debug`.

Every handoff attempt ends with one `handoff summary` line, so `grep 'handoff summary' supervisor.log` lists the recycling history:

```
handoff summary: gen=8 result=ok path="/home/user/Videos/wallpaper.mp4" consecutive=0 renderer_age=1m54s mem=591MB (rss=591 swap=0) log=2KB validated=true
```

That line is from a test run, with the video path replaced. On an `ok` line, `gen` is the new renderer's generation, while `renderer_age`, `mem` and `log` describe the renderer being retired. `mem` is resident plus swapped memory.

## Common problems

### murale exits immediately and prints nothing

In the default mode, startup errors go to `supervisor.log` (see [Where the logs are](#where-the-logs-are)), not the terminal. Three errors you may see there:

- `another murale supervisor is running`: another murale is already running for the same output.
- `XDG_RUNTIME_DIR must be set`: murale was started without that variable.
- `initial display-ready`: the first renderer exited, or stopped responding, before it drew its first frame.

If the renderer exits unexpectedly later, the supervisor starts a new one, waiting 2, 4 and then 8 seconds before each attempt. After three failed attempts the supervisor exits.

### I can't find the renderer in the process list

The renderer runs under the name `murale-render`, and the supervisor under `murale`. `pgrep -a murale-render` lists the renderers.

### The wallpaper is on the wrong monitor

Run `murale --list-outputs` and check the name you passed to `--output`. If the name does not match, murale keeps waiting for it, and `supervisor.log` has a line starting `waiting for output (renderer alive, available:` that lists the names murale saw.

### Handoffs stop happening

Run `grep 'handoff summary' supervisor.log`. Each line's `result` shows whether that attempt succeeded, failed (`fail`), was blocked by a resource check (`gate_skip`) or was put off because the output was disconnected (`deferred`), and `reason` says why. After a failure, a `next attempt in` line says when the supervisor will try again. An `ERROR` line starting `recycle stale:` means the current renderer has gone more than three intervals of on-screen time without a successful handoff. `INFO` lines starting with `resource gate:` are checks that passed and show how much headroom there was. Before starting the new renderer, the supervisor checks that the system has at least 50 MiB of available memory, at least 50 MiB free in the runtime directory, at least 3 GiB of free video memory when `/dev/nvidia0` exists, and no kernel out-of-memory messages in the last 5 minutes. The wait before a retry starts at 10 minutes and doubles with each further failure in a row, but is never longer than the recycle interval.

### My mpv config is ignored

Pass `config=yes` in `--mpv-options`. See [Flags](../README.md#flags).

## mpv version

libmpv's OpenGL renderer had a file descriptor leak involving fences ([mpv#17217](https://github.com/mpv-player/mpv/issues/17217)). It affects programs that render through libmpv's OpenGL render API, which murale does. The fix is [mpv PR #17303](https://github.com/mpv-player/mpv/pull/17303), merged after mpv 0.41.0 was released. mpv 0.41.0 and earlier do not have it, so use a newer release or a git build.

On CachyOS, `mpv-git` is in the repositories. On Arch, it is in the AUR. To go back to the packaged release, install `mpv` again.

## Known limitations

### Workspace switching on more than one monitor

Hyprland's `workspace>>` event reports the focused workspace, whichever monitor it is on, and every murale instance switches on it. With `--workspace`, every 30 seconds the renderer also checks which workspace is active and whether mpv is playing that workspace's video. That mismatch check works differently: with `--output`, it asks Hyprland for the workspace shown on that output. On two monitors, when focus moves to a workspace on the other monitor, an instance loads that workspace's video, and after two checks it logs a `[ws] mismatch` warning, repeated every 5 minutes while focus stays there.

### Paths with `..` or `~user/`

murale expands only a leading `~/`. A path starting with `~user/`, or a bare `~`, is treated as a relative path and made absolute against the directory murale was started from, so mpv cannot open it.

murale keeps `..` in a path as written, but mpv resolves it when it loads the file. The path murale expects then never matches the path mpv reports, so murale reloads that video, restarting it, on every workspace event that selects it and on every startup, handoff or reconnect check, and the mismatch check logs a warning. Paths without `..` are not affected.

### Old video for up to about 10 seconds after a handoff

If you switch workspaces while a handoff is in progress, the new renderer shows the video from before the switch until the supervisor has finished checking it, about 10 seconds after it takes over the screen.

### Repeated warning while the monitor sleeps

In the default mode, while a monitor named by `--output` is asleep or disconnected, the renderer log gets a `still waiting for target output` warning every 10 minutes. This is expected and does not need any action.

## Tested on

The measurements in the README were taken on:

- murale 1.3.0: the release build (commit c0c4150) for run 1 and a development build (commit 21b5f33) for run 2
- AMD Ryzen 9 5950X
- NVIDIA GeForce RTX 5080, driver 580.142
- Samsung Odyssey G95NC at 7680x2160, 240 Hz
- Hyprland 0.54.3
- mpv-git 0.41.0_703_gf5d4d9b029
- A 3840x2160 HEVC video at 240 fps
