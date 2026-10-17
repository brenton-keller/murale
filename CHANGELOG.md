# Changelog

## 1.3.0 (2026-10-16)

### Added

- `--list-outputs` prints the names of the connected outputs, one per line, and exits.
- Recycling counts only on-screen time, meaning the time the output has been connected since the current renderer started. While the output is disconnected, for example when the monitor sleeps, no handoff starts. After the output reconnects, the supervisor waits 60 seconds before a handoff.
- After a failed handoff or a failed resource check, the supervisor waits before trying again. The wait starts at 10 minutes and doubles with each further failure in a row, up to the recycle interval. A successful handoff resets it.
- `supervisor.log` is now kept at `${XDG_STATE_HOME:-$HOME/.local/state}/murale/<ns>/supervisor.log`, so it survives a reboot. At startup a log larger than 2 MiB is renamed to `supervisor.log.1`, and while murale runs the log is cut to its last 1 MiB once it passes 2 MiB. If the file cannot be opened, murale prints a warning to stderr and writes the log to the runtime directory as before.
- When a renderer log is moved to `failed/`, its last 1 MiB is also copied to a `failed/` directory next to the persistent `supervisor.log`.
- Every handoff attempt logs one `handoff summary` line with the result (`ok`, `fail`, `gate_skip` or `deferred`), the number of failures in a row, the renderer's age and memory use, and its log size.
- A failed handoff is logged at `WARN`, and from the third failure in a row at `ERROR`. A `recycle stale` line is logged at `ERROR` when the current renderer's on-screen time passes three times the recycle interval. The supervisor logs `next attempt in <age>` when it schedules a retry.
- A resource check that passes logs the free video memory, available memory and free runtime-directory space it measured.
- The `handoff complete` line includes the path of the video the new renderer is playing.
- The supervisor logs a warning when `input-ipc-server` is passed in the default mode, where it is ignored.
- The renderer logs each Hyprland `workspace>>` event with what it did about it (`action=loaded`, `load-failed`, `ignored-disconnected`, `same-video`, `deferred-hold`, `skipped-mpv-idle` or `skipped-no-handle`).
- With `--workspace`, the renderer checks every 30 seconds whether mpv is playing the video for the active workspace, and logs a `[ws] mismatch` warning when the same mismatch is seen on two checks in a row, repeated every 5 minutes while it lasts.
- The renderer logs the surface size when the surface is created, configured or changes scale. With `--output`, it warns when the buffer size differs from the output's current mode on two checks in a row.
- Workspace re-checks log a line starting with `startup:`, `unlock:` or `reconnect:` that names the active workspace and says whether a different video was loaded.
- If the thread that listens for workspace events stops, the renderer logs a warning and starts a new one.

### Changed

- If `--output` names an output that is not connected, for example because the monitor is asleep when murale starts, murale now waits for that output to appear instead of letting the compositor choose one. This applies both in the default mode and with `--no-recycle`. The renderer reports to the supervisor every 30 seconds while it waits, and the supervisor keeps waiting as long as those reports arrive. A renderer that stops reporting is treated as failed. The supervisor logs `waiting for output (renderer alive, available: [<names>])` when the wait starts and every 10 minutes after; the renderer logs a `still waiting for target output` warning every 10 minutes.
- If the renderer crashes while its output is disconnected, the replacement the supervisor starts waits for the output in the same way, and the supervisor keeps running.
- murale checks the active workspace when it starts, after each handoff, and when a monitor named by `--output` reconnects (straight away and again 1 second later). If the file playing is not that workspace's video, or the default video for a workspace without a mapping, it loads it.
- Workspace events that arrive while the output is disconnected are ignored.
- A leading `~/` in `<VIDEO>` and `--workspace` paths is expanded, and relative paths are made absolute, when murale starts.
- The renderer process is named `murale-render`. It was `exe` before.
- Renderer logs no longer contain terminal color codes.
- The `--output` help text points to `--list-outputs`.

### Fixed

- Handoffs could fail with `B path mismatch` when `--workspace` was in use, leaving the old renderer running past its interval.
- Handoffs could fail with `resource pressure: insufficient VRAM` because the video memory check also ran while both renderers were running. The check now runs only before the new renderer starts.
- After a handoff, workspaces without a mapping played the video the old renderer had been playing instead of the `<VIDEO>` default.
- After murale started, or a handoff finished, on a workspace with a mapping, switching to a workspace that uses the default video could leave the previous video playing.
- After a monitor woke from sleep, the wallpaper could be shown zoomed in by a factor of 2.
- After a monitor woke from sleep, murale could play the video for a workspace Hyprland reported during wake-up instead of the workspace you were on.
- Without `HYPRLAND_INSTANCE_SIGNATURE`, the "workspace switching disabled" warning was logged on every pass of the main loop. It is now logged once.
- Log pruning could remove the log of the renderer that was running.
- Two murale instances on different outputs collided on the same lock file, so the second one could not start. Each output now has its own runtime directory, lock and logs ([#4](https://github.com/brenton-keller/murale/issues/4)).

## 1.2.0 (2026-08-31)

- murale now supervises its own renderer. By default it runs a supervisor that replaces the renderer process every 2 hours with a handoff to a new one, which takes over the screen at the same playback position. This replaces the separate `murale-supervisor` script from 1.1.0.
- `--no-recycle` runs a single process without the supervisor.
- `MURALE_REFRESH_INTERVAL` sets the recycle interval in seconds.
- `profile=NAME` in `--mpv-options` works with user-defined profiles ([#2](https://github.com/brenton-keller/murale/issues/2)).
