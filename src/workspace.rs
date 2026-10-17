use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::mpv::{mpv_get_path, mpv_loadfile, MpvPath, SharedMpvPtr};
use crate::SHUTDOWN;

// --- Hyprland workspace switching ---

pub(crate) fn get_active_workspace() -> Option<String> {
    let output = std::process::Command::new("hyprctl")
        .args(["activeworkspace", "-j"])
        .output()
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    json.get("name")?.as_str().map(String::from)
}

fn query_workspace_for_output(output_name: &str) -> Option<String> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let xdg = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
    let sock_path = format!("{xdg}/hypr/{sig}/.socket.sock");

    let mut stream = UnixStream::connect(&sock_path).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(200)))
        .ok()?;
    stream
        .set_write_timeout(Some(Duration::from_millis(200)))
        .ok()?;

    stream.write_all(b"j/monitors").ok()?;

    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    loop {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(_) => return None,
        }
    }

    let monitors: Vec<serde_json::Value> = serde_json::from_slice(&buf).ok()?;
    for mon in &monitors {
        if mon.get("name")?.as_str()? == output_name {
            return mon
                .get("activeWorkspace")?
                .get("name")?
                .as_str()
                .map(String::from);
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MismatchResult {
    Match,
    Mismatch {
        ws: String,
        expected: String,
        playing: String,
    },
    Skip,
}

pub(crate) fn check_workspace_mismatch(
    ws: &str,
    map: &HashMap<String, String>,
    default: &str,
    mpv_path: &MpvPath,
) -> MismatchResult {
    let playing = match mpv_path {
        MpvPath::NoHandle => return MismatchResult::Skip,
        MpvPath::Unavailable => "<none>".to_string(),
        MpvPath::Path(p) => p.clone(),
    };
    let expected = map.get(ws).map(|v| v.as_str()).unwrap_or(default);
    if playing == expected {
        MismatchResult::Match
    } else {
        MismatchResult::Mismatch {
            ws: ws.to_string(),
            expected: expected.to_string(),
            playing,
        }
    }
}

struct MismatchState {
    pending: Option<(String, String, String, Instant)>,
    first_seen: Option<Instant>,
    confirmed_since: Option<Instant>,
    last_warn: Instant,
}

impl MismatchState {
    fn new() -> Self {
        Self {
            pending: None,
            first_seen: None,
            confirmed_since: None,
            last_warn: Instant::now(),
        }
    }

    fn tick(
        &mut self,
        result: MismatchResult,
        output_name: &str,
        mismatch_start_millis: &AtomicI64,
    ) {
        match result {
            MismatchResult::Skip => {}
            MismatchResult::Match => {
                if let Some(first) = self.first_seen.take() {
                    let dur = first.elapsed();
                    tracing::info!(
                        "[ws] mismatch cleared (duration={})",
                        format_duration(dur)
                    );
                }
                self.pending = None;
                self.confirmed_since = None;
                mismatch_start_millis.store(-1, Ordering::Relaxed);
            }
            MismatchResult::Mismatch {
                ws,
                expected,
                playing,
            } => {
                let triple = (ws.clone(), expected.clone(), playing.clone());
                if self.confirmed_since.is_some() {
                    let first = self.first_seen.unwrap_or_else(Instant::now);
                    let dur_secs = first.elapsed().as_secs() as i64;
                    mismatch_start_millis.store(dur_secs, Ordering::Relaxed);
                    if self.last_warn.elapsed() >= Duration::from_secs(300) {
                        tracing::warn!(
                            "[ws] mismatch: output={output_name} active_ws={ws} expected=\"{expected}\" playing=\"{playing}\" since={}",
                            format_duration(first.elapsed())
                        );
                        self.last_warn = Instant::now();
                    }
                } else if self.pending.as_ref().map(|(w, e, p, _)| (w, e, p))
                    == Some((&triple.0, &triple.1, &triple.2))
                {
                    let first = self.pending.as_ref().unwrap().3;
                    self.first_seen = Some(first);
                    self.confirmed_since = Some(Instant::now());
                    self.last_warn = Instant::now();
                    let since = first.elapsed();
                    mismatch_start_millis.store(since.as_secs() as i64, Ordering::Relaxed);
                    tracing::warn!(
                        "[ws] mismatch: output={output_name} active_ws={ws} expected=\"{expected}\" playing=\"{playing}\" since={}",
                        format_duration(since)
                    );
                } else {
                    self.pending = Some((triple.0, triple.1, triple.2, Instant::now()));
                }
            }
        }
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

pub(crate) fn spawn_workspace_listener(
    mpv_ptr: SharedMpvPtr,
    workspace_map: HashMap<String, String>,
    default_video: String,
    hold_active: Arc<AtomicBool>,
    deferred_video: Arc<Mutex<Option<String>>>,
    output_connected: Arc<AtomicBool>,
    output_name: Option<String>,
    ws_mismatch_secs: Arc<AtomicI64>,
) -> Option<std::thread::JoinHandle<()>> {
    let sig = match std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        Ok(s) => s,
        Err(_) => return None,
    };

    let xdg = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
    let socket_path = format!("{xdg}/hypr/{sig}/.socket2.sock");

    let jh = std::thread::spawn(move || {
        let stream = match UnixStream::connect(&socket_path) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("could not connect to Hyprland event socket: {e}");
                return;
            }
        };

        if let Err(e) = stream.set_read_timeout(Some(Duration::from_secs(1))) {
            tracing::warn!("could not set socket timeout: {e}");
            return;
        }

        tracing::info!(
            "workspace switching active ({} mappings)",
            workspace_map.len()
        );

        let mut reader = BufReader::new(stream);
        let mut buf = String::new();
        let mut mismatch = MismatchState::new();
        let mut last_mismatch_check = Instant::now();
        let out_name = output_name.as_deref().unwrap_or("unknown");

        loop {
            if SHUTDOWN.load(Ordering::Relaxed) {
                break;
            }

            // Mismatch check at top of loop (H3): runs every 30s, before read_line
            if !hold_active.load(Ordering::Relaxed)
                && last_mismatch_check.elapsed().as_secs() >= 30
            {
                last_mismatch_check = Instant::now();

                let ws = if let Some(ref name) = output_name {
                    query_workspace_for_output(name)
                } else {
                    get_active_workspace()
                };

                if let Some(ws) = ws {
                    let mpv_path = mpv_get_path(&mpv_ptr);
                    let result =
                        check_workspace_mismatch(&ws, &workspace_map, &default_video, &mpv_path);
                    mismatch.tick(result, out_name, &ws_mismatch_secs);
                }
            }

            buf.clear();
            match reader.read_line(&mut buf) {
                Ok(0) => {
                    tracing::warn!("Hyprland event socket closed, workspace switching disabled");
                    break;
                }
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => {
                    tracing::warn!(
                        "Hyprland event socket error: {e}, workspace switching disabled"
                    );
                    break;
                }
            }

            let line = buf.trim_end();
            if let Some(name) = line.strip_prefix("workspace>>") {
                if !output_connected.load(Ordering::Relaxed) {
                    tracing::info!("[ws] event={name} action=ignored-disconnected");
                    continue;
                }

                let desired = workspace_map.get(name).unwrap_or(&default_video);
                let current = match mpv_get_path(&mpv_ptr) {
                    MpvPath::Path(p) => p,
                    MpvPath::NoHandle => {
                        tracing::debug!("[ws] event=\"workspace>>{name}\" action=skipped-no-handle");
                        continue;
                    }
                    MpvPath::Unavailable => {
                        tracing::debug!("[ws] event=\"workspace>>{name}\" action=skipped-mpv-idle");
                        continue;
                    }
                };

                if *desired != current {
                    if hold_active.load(Ordering::Relaxed) {
                        tracing::debug!(
                            "[ws] event=\"workspace>>{name}\" action=deferred-hold video=\"{desired}\" believed=\"{current}\""
                        );
                        let mut def = deferred_video.lock().unwrap_or_else(|e| e.into_inner());
                        *def = Some(desired.clone());
                    } else if mpv_loadfile(&mpv_ptr, desired) {
                        tracing::info!(
                            "[ws] event=\"workspace>>{name}\" action=loaded video=\"{desired}\" believed=\"{current}\""
                        );
                    } else {
                        tracing::warn!(
                            "[ws] event=\"workspace>>{name}\" action=load-failed video=\"{desired}\" believed=\"{current}\""
                        );
                    }
                } else {
                    tracing::debug!(
                        "[ws] event=\"workspace>>{name}\" action=same-video believed=\"{current}\""
                    );
                }
            } else {
                tracing::trace!("[ws] event=\"{line}\"");
            }
        }
    });
    Some(jh)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(entries: &[(&str, &str)]) -> HashMap<String, String> {
        entries
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn mismatch_match() {
        let m = map(&[("1", "/a.mov")]);
        let result = check_workspace_mismatch("1", &m, "/default.mov", &MpvPath::Path("/a.mov".into()));
        assert_eq!(result, MismatchResult::Match);
    }

    #[test]
    fn mismatch_mismatch() {
        let m = map(&[("1", "/a.mov")]);
        let result = check_workspace_mismatch("1", &m, "/default.mov", &MpvPath::Path("/b.mov".into()));
        assert!(matches!(result, MismatchResult::Mismatch { .. }));
        if let MismatchResult::Mismatch { expected, playing, .. } = result {
            assert_eq!(expected, "/a.mov");
            assert_eq!(playing, "/b.mov");
        }
    }

    #[test]
    fn mismatch_unmapped_uses_default() {
        let m = map(&[("1", "/a.mov")]);
        let result = check_workspace_mismatch("5", &m, "/default.mov", &MpvPath::Path("/default.mov".into()));
        assert_eq!(result, MismatchResult::Match);
    }

    #[test]
    fn mismatch_unmapped_wrong_video() {
        let m = map(&[("1", "/a.mov")]);
        let result = check_workspace_mismatch("5", &m, "/default.mov", &MpvPath::Path("/wrong.mov".into()));
        assert!(matches!(result, MismatchResult::Mismatch { expected, .. } if expected == "/default.mov"));
    }

    #[test]
    fn mismatch_no_handle_skips() {
        let m = map(&[("1", "/a.mov")]);
        let result = check_workspace_mismatch("1", &m, "/default.mov", &MpvPath::NoHandle);
        assert_eq!(result, MismatchResult::Skip);
    }

    #[test]
    fn mismatch_unavailable_reports_none() {
        let m = map(&[("1", "/a.mov")]);
        let result = check_workspace_mismatch("1", &m, "/default.mov", &MpvPath::Unavailable);
        assert!(matches!(result, MismatchResult::Mismatch { playing, .. } if playing == "<none>"));
    }

    #[test]
    fn debounce_requires_two_ticks() {
        let atomic = Arc::new(AtomicI64::new(-1));
        let mut state = MismatchState::new();

        let mismatch = MismatchResult::Mismatch {
            ws: "1".into(),
            expected: "/a.mov".into(),
            playing: "/b.mov".into(),
        };

        // Tick 1: pending only, no confirmation
        state.tick(mismatch.clone(), "DP-4", &atomic);
        assert!(state.confirmed_since.is_none());
        assert!(state.pending.is_some());
        assert!(state.first_seen.is_none());
        assert_eq!(atomic.load(Ordering::Relaxed), -1);

        std::thread::sleep(Duration::from_millis(50));

        // Tick 2: same mismatch → confirmed, since tracks from tick 1
        state.tick(mismatch.clone(), "DP-4", &atomic);
        assert!(state.confirmed_since.is_some());
        assert!(state.first_seen.is_some());
        let since_secs = atomic.load(Ordering::Relaxed);
        assert!(since_secs >= 0, "atomic should hold duration since first seen");

        // Tick 3: match → cleared
        state.tick(MismatchResult::Match, "DP-4", &atomic);
        assert!(state.confirmed_since.is_none());
        assert!(state.first_seen.is_none());
        assert!(state.pending.is_none());
        assert_eq!(atomic.load(Ordering::Relaxed), -1);
    }

    #[test]
    fn debounce_different_mismatch_resets_pending() {
        let atomic = Arc::new(AtomicI64::new(-1));
        let mut state = MismatchState::new();

        state.tick(
            MismatchResult::Mismatch {
                ws: "1".into(),
                expected: "/a.mov".into(),
                playing: "/b.mov".into(),
            },
            "DP-4",
            &atomic,
        );
        assert!(state.pending.is_some());

        // Different mismatch replaces pending
        state.tick(
            MismatchResult::Mismatch {
                ws: "2".into(),
                expected: "/c.mov".into(),
                playing: "/d.mov".into(),
            },
            "DP-4",
            &atomic,
        );
        assert!(state.confirmed_since.is_none());
        assert_eq!(state.pending.as_ref().unwrap().0, "2");
    }

    #[test]
    fn mismatch_unmapped_ws_wrong_default() {
        let m = map(&[("1", "/a.mov"), ("2", "/b.mov")]);
        let cli_default = "/default.mov";
        let corrupted_default = "/a.mov";

        // With correct default: unmapped ws playing /a.mov is a mismatch
        let result = check_workspace_mismatch("5", &m, cli_default, &MpvPath::Path("/a.mov".into()));
        assert!(
            matches!(result, MismatchResult::Mismatch { expected, .. } if expected == cli_default),
            "detector should compare against CLI default for unmapped workspaces"
        );

        // With corrupted default: unmapped ws playing /a.mov would falsely match
        let result = check_workspace_mismatch("5", &m, corrupted_default, &MpvPath::Path("/a.mov".into()));
        assert_eq!(
            result,
            MismatchResult::Match,
            "corrupted default causes false negative — detector misses wrong video"
        );
    }
}
