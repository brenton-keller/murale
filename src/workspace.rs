use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::mpv::{mpv_loadfile, SharedMpvPtr};
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

pub(crate) fn spawn_workspace_listener(
    mpv_ptr: SharedMpvPtr,
    workspace_map: HashMap<String, String>,
    default_video: String,
    hold_active: Arc<AtomicBool>,
    deferred_video: Arc<Mutex<Option<String>>>,
) -> Option<std::thread::JoinHandle<()>> {
    let sig = match std::env::var("HYPRLAND_INSTANCE_SIGNATURE") {
        Ok(s) => s,
        Err(_) => {
            tracing::warn!(
                "--workspace specified but HYPRLAND_INSTANCE_SIGNATURE not set; \
                 workspace switching disabled"
            );
            return None;
        }
    };

    let xdg = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".into());
    let socket_path = format!("{xdg}/hypr/{sig}/.socket2.sock");

    if !hold_active.load(Ordering::Relaxed) {
        if let Some(active) = get_active_workspace() {
            if let Some(video) = workspace_map.get(&active) {
                if *video != default_video {
                    tracing::info!("workspace {active}: loading {video}");
                    mpv_loadfile(&mpv_ptr, video);
                }
            }
        }
    }

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
        let mut current_video = default_video.clone();

        loop {
            if SHUTDOWN.load(Ordering::Relaxed) {
                break;
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
                let video = workspace_map.get(name).unwrap_or(&default_video);

                if *video != current_video {
                    if hold_active.load(Ordering::Relaxed) {
                        tracing::debug!("workspace {name}: deferred (hold mode)");
                        let mut def = deferred_video.lock().unwrap_or_else(|e| e.into_inner());
                        *def = Some(video.clone());
                    } else {
                        tracing::info!("workspace {name}: loading {video}");
                        if mpv_loadfile(&mpv_ptr, video) {
                            current_video = video.clone();
                        }
                    }
                }
            }
        }
    });
    Some(jh)
}
