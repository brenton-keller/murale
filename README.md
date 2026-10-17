# murale

murale plays a video as the desktop wallpaper on Wayland compositors that support `wlr-layer-shell`. It is written in Rust and uses libmpv to decode and render the video.

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

![murale demo](docs/demo.gif)

## Why murale exists

[mpvpaper](https://github.com/GhostNaN/mpvpaper) is an existing video wallpaper player for Wayland. Its issue tracker has open reports of memory growth during long playback ([#101](https://github.com/GhostNaN/mpvpaper/issues/101), and [#117](https://github.com/GhostNaN/mpvpaper/issues/117), which reports 28 GB after about 12 hours) and of a segfault when monitors disconnect ([#97](https://github.com/GhostNaN/mpvpaper/issues/97)).

On NVIDIA (driver 580.142), murale's rendering process also grows in memory, but only while it is presenting frames. Starting a new renderer process brings memory back down, so murale replaces the renderer on a timer. The measurements are under [Recycling](#recycling).

## Install

Build from source with a Rust toolchain:

```bash
git clone https://github.com/brenton-keller/murale.git
cd murale
cargo build --release
```

The binary is `target/release/murale`. Copy it somewhere on your `PATH` if you want to call it as `murale`.

The build links against libmpv, libwayland-client and libwayland-egl, and finds the Wayland libraries through pkg-config. At runtime murale also loads `libEGL.so.1`, which your GPU driver or libglvnd provides.

```bash
sudo pacman -S mpv wayland pkgconf                          # Arch Linux
sudo apt install libmpv-dev libwayland-dev pkgconf          # Debian and Ubuntu
sudo dnf install mpv-devel wayland-devel pkgconf-pkg-config # Fedora
```

mpv 0.41.0 and earlier have a file descriptor leak in libmpv's OpenGL renderer, fixed by [mpv PR #17303](https://github.com/mpv-player/mpv/pull/17303) after 0.41.0 was released. Use a newer mpv release or a git build, such as `mpv-git` from the CachyOS repositories or the AUR. See [mpv version](docs/troubleshooting.md#mpv-version).

Your compositor must support `wlr-layer-shell`; murale exits with an error if it does not. According to the [protocol's support table](https://wayland.app/protocols/wlr-layer-shell-unstable-v1), Hyprland, Sway, river, Wayfire and KWin support it and GNOME's Mutter and Weston do not. murale has only been tested on Hyprland.

## Usage

```bash
murale --list-outputs
murale ~/Videos/wallpaper.mp4 --output DP-1
```

With no `--output`, the compositor picks which monitor the wallpaper goes on. To start murale with Hyprland, add an `exec-once` line to your Hyprland config:

```ini
exec-once = murale ~/Videos/default.mp4 --output DP-1 --workspace 1=~/Videos/forest.mp4 --workspace 2=~/Videos/ocean.mp4
```

If murale is not on Hyprland's `PATH`, use the full path to the binary. In the default mode murale writes its logs to files, not the terminal; see [Where the logs are](docs/troubleshooting.md#where-the-logs-are).

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

There is no `--version` flag. If no connected output has the name given to `--output`, murale logs the names it can see and waits for that output to appear. This also covers a monitor that is asleep when murale starts.

`--mpv-options` takes `key=value` pairs separated by commas, for example `"hwdec=vaapi,mute=yes"`. Spaces are not separators, and an option value cannot itself contain a comma. murale sets `hwdec=auto`, `loop-file=inf` and `panscan=1.0` (scale to fill the screen, cropping the edges) before applying yours. libmpv does not read mpv's config files by default, so `~/.config/mpv/mpv.conf` is ignored unless you pass `config=yes`. mpv's JSON IPC (`input-ipc-server`) is available only when murale runs with `--no-recycle`.

## Per-workspace videos

On Hyprland, murale can switch videos when the active workspace changes. Each `--workspace N=PATH` maps a workspace to a video, and workspaces without a mapping play the default video. Switching uses mpv's `loadfile`, so the new video starts from the beginning. Your shell does not expand `~` after `=`, so murale does it. Workspace switching needs `HYPRLAND_INSTANCE_SIGNATURE` in murale's environment, which processes started by Hyprland inherit, and `hyprctl` on its `PATH`.

## Multi-monitor

Run one murale per output, each with its own `--output`:

```ini
exec-once = murale ~/Videos/left.mp4 --output DP-1
exec-once = murale ~/Videos/right.mp4 --output DP-2
```

When `--output` is set and that output disconnects, murale removes its surface and pauses, and resumes when an output with the same name connects again. Instances started without `--output` all share the namespace `default`, so only one of them can run.

## Recycling

By default `murale` starts a supervisor process, which starts the renderer process that does the drawing. After every 2 hours of on-screen time, the supervisor starts a second renderer with the video the current one is playing, switches the screen to it on a frame boundary, checks it for about 10 seconds, and then stops the old renderer. If the check fails, the old renderer keeps running. With the default 2 hour interval, a renderer reached 967 MB after 2 hours of continuous rendering, and its replacement started at 577 MB.

| Run | Renderer | Start | After 2 hours |
|---|---|---|---|
| 1 | 1 | 586 MB | 967 MB |
| 2 | 1 | 577 MB | 916 MB |
| 2 | 2 | 577 MB | 982 MB |
| 2 | 3 | 583 MB | 971 MB |

These are four renderers from two test runs on one machine (NVIDIA RTX 5080, driver 580.142, Hyprland 0.54.3), playing a 3840x2160 HEVC video at 240 fps on a 7680x2160 output at 240 Hz. Memory is resident plus swapped memory. Memory does not grow while the monitor is asleep, and on-screen time does not count while the output is disconnected.

To change the interval, set `MURALE_REFRESH_INTERVAL` to a number of seconds:

```bash
MURALE_REFRESH_INTERVAL=3600 murale ~/Videos/wallpaper.mp4
```

`--no-recycle` runs a single renderer with no supervisor. That process instead rebuilds its mpv and EGL state in place every 2 hours. `MURALE_REFRESH_INTERVAL` does not change it.

If something goes wrong, see [docs/troubleshooting.md](docs/troubleshooting.md) for log locations, common problems, known limitations and the mpv version requirement.

## License

[MIT](LICENSE)
