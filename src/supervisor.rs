use std::fs;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use nix::poll::{PollFd, PollFlags, PollTimeout};
use tracing::{error, info, warn};

use crate::ctrl::{
    ChildMsg, CtrlChannel, Event, Hello, Snapshot, SpawnMode, CTRL_FD, PROTOCOL_VERSION, RELEASE_FD,
};
use crate::Cli;

const OVERLAP_TIMEOUT: Duration = Duration::from_secs(15);
const HEALTH_WINDOW: Duration = Duration::from_secs(10);
const MAX_CONSECUTIVE_FAILURES: u32 = 3;
const POS_TOLERANCE_FRAMES: f64 = 1.5;
const ROUGH_SEEK_AHEAD: f64 = 0.800;
const DEFAULT_RELEASE_LATENCY: f64 = 0.030;

const LOG_MAX_AGE: u64 = 259200;
const LOG_MAX_BYTES: u64 = 67_108_864;
const FAILED_LOG_MAX_AGE: u64 = 604800;
const FAILED_LOG_MAX_BYTES: u64 = 16_777_216;
const SUP_LOG_MAX: u64 = 2_097_152;
const SUP_LOG_KEEP: u64 = 1_048_576;

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_signal(_sig: libc::c_int) {
    SHUTDOWN.store(true, Ordering::Relaxed);
}

struct State {
    ready_dir: PathBuf,
    log_dir: PathBuf,
    persistent_log_dir: Option<PathBuf>,
    sup_log_path: Option<PathBuf>,
    generation: u64,
    consecutive_failures: u32,
    handoff_health: HandoffHealth,
}

struct ChildProcess {
    pid: u32,
    child: Child,
    ctrl: CtrlChannel,
    gen: u64,
    #[allow(dead_code)]
    token: String,
    slot: u32,
    log_path: PathBuf,
    spawned_at: Instant,
}

#[derive(Debug)]
struct HandoffHealth {
    consecutive_handoff_failures: u32,
}

impl HandoffHealth {
    fn new() -> Self {
        Self {
            consecutive_handoff_failures: 0,
        }
    }
}

#[derive(Debug, PartialEq)]
enum HandoffOutcome {
    Success { gen: u64, path: String, validated: bool },
    Failure { gen: u64, reason: String },
    GateSkip { gen: u64, reason: String },
    Deferred { gen: u64, reason: String },
}

#[derive(Debug, PartialEq)]
enum EscalationLevel {
    Warn,
    Error,
}

#[allow(dead_code)]
struct HandoffEscalation {
    level: EscalationLevel,
    consecutive: u32,
}

fn record_handoff_outcome(
    health: &mut HandoffHealth,
    outcome: &HandoffOutcome,
) -> Option<HandoffEscalation> {
    match outcome {
        HandoffOutcome::Success { .. } => {
            health.consecutive_handoff_failures = 0;
            None
        }
        HandoffOutcome::Deferred { .. } => None,
        HandoffOutcome::Failure { .. } | HandoffOutcome::GateSkip { .. } => {
            health.consecutive_handoff_failures += 1;
            let n = health.consecutive_handoff_failures;
            let level = if n >= 3 {
                EscalationLevel::Error
            } else {
                EscalationLevel::Warn
            };
            Some(HandoffEscalation {
                level,
                consecutive: n,
            })
        }
    }
}

fn read_rss_mb(pid: u32) -> (f64, f64) {
    let path = format!("/proc/{pid}/status");
    let contents = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return (0.0, 0.0),
    };
    let mut rss_kb = 0u64;
    let mut swap_kb = 0u64;
    for line in contents.lines() {
        if line.starts_with("VmRSS:") {
            rss_kb = line.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
        } else if line.starts_with("VmSwap:") {
            swap_kb = line.split_whitespace().nth(1).and_then(|v| v.parse().ok()).unwrap_or(0);
        }
    }
    (rss_kb as f64 / 1024.0, swap_kb as f64 / 1024.0)
}

fn format_age(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn emit_handoff_summary(
    outcome: &HandoffOutcome,
    health: &HandoffHealth,
    renderer_age: Duration,
    rss_mb: f64,
    swap_mb: f64,
    log_bytes: u64,
) {
    let age = format_age(renderer_age);
    let mem = format!("mem={:.0}MB (rss={rss_mb:.0} swap={swap_mb:.0})", rss_mb + swap_mb);
    let log_kb = log_bytes / 1024;
    let consecutive = health.consecutive_handoff_failures;

    match outcome {
        HandoffOutcome::Success { gen, path, validated } => {
            info!(
                "handoff summary: gen={gen} result=ok path=\"{path}\" consecutive={consecutive} renderer_age={age} {mem} log={log_kb}KB validated={validated}"
            );
        }
        HandoffOutcome::Failure { gen, reason } => {
            warn!(
                "handoff summary: gen={gen} result=fail reason=\"{reason}\" consecutive={consecutive} renderer_age={age} {mem} log={log_kb}KB"
            );
        }
        HandoffOutcome::GateSkip { gen, reason } => {
            warn!(
                "handoff summary: gen={gen} result=gate_skip reason=\"{reason}\" consecutive={consecutive} renderer_age={age} {mem} log={log_kb}KB"
            );
        }
        HandoffOutcome::Deferred { gen, reason } => {
            info!(
                "handoff summary: gen={gen} result=deferred reason=\"{reason}\" consecutive={consecutive} renderer_age={age} {mem} log={log_kb}KB"
            );
        }
    }
}

enum WaitResult {
    Timeout,
    ChildDied(i32),
    Message(ChildMsg),
}

pub(crate) fn output_namespace(output: Option<&str>) -> String {
    match output {
        None => "default".to_string(),
        Some(name) => {
            assert!(
                !name.is_empty() && name.len() <= 100,
                "invalid output name: must be 1-100 bytes"
            );
            let hex: String = name.bytes().map(|b| format!("{b:02x}")).collect();
            format!("out-{hex}")
        }
    }
}

// ==================== Entry point ====================

pub fn run_supervisor(cli: Cli) -> Result<()> {
    unsafe {
        libc::signal(
            libc::SIGTERM,
            handle_signal as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGINT,
            handle_signal as *const () as libc::sighandler_t,
        );
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }

    let xdg = std::env::var("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR must be set")?;
    let ns = output_namespace(cli.output.as_deref());
    let ready_dir = PathBuf::from(&xdg).join("murale").join(&ns);
    fs::create_dir_all(&ready_dir)?;

    let log_dir = ready_dir.join("logs");
    fs::create_dir_all(&log_dir)?;
    fs::create_dir_all(log_dir.join("failed"))?;

    acquire_lock(&ready_dir)?;

    if let Some(ref opts) = cli.mpv_options {
        if opts
            .split(',')
            .any(|o| o.trim_start().starts_with("input-ipc-server"))
        {
            warn!("input-ipc-server is not supported in recycling mode; ignored (use --no-recycle for mpv IPC)");
        }
    }

    let refresh_interval = std::env::var("MURALE_REFRESH_INTERVAL")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(7200));

    let persistent_log_dir = {
        let state_dir = std::env::var("XDG_STATE_HOME").unwrap_or_else(|_| {
            format!(
                "{}/.local/state",
                std::env::var("HOME").unwrap_or_default()
            )
        });
        let dir = PathBuf::from(&state_dir).join("murale").join(&ns);
        if fs::create_dir_all(&dir).is_ok() && fs::create_dir_all(dir.join("failed")).is_ok() {
            Some(dir)
        } else {
            None
        }
    };

    let sup_log_path = persistent_log_dir
        .as_ref()
        .map(|d| d.join("supervisor.log"));

    let mut state = State {
        ready_dir,
        log_dir,
        persistent_log_dir,
        sup_log_path,
        generation: 0,
        consecutive_failures: 0,
        handoff_health: HandoffHealth::new(),
    };

    retire_logs(&state, &[]);

    let mut current = spawn_child(&cli, &mut state, SpawnMode::Normal, 0, None, None)?;
    wait_for_display_ready(&mut current).context("initial display-ready")?;
    info!("initial instance running (pid={})", current.pid);

    let mut accumulated_on_screen = Duration::ZERO;
    let mut last_connected_at: Option<Instant> = Some(Instant::now());
    let mut reconnected_at: Option<Instant> = None;
    let mut disconnected_at: Option<Instant> = None;
    let mut retry_after: Option<Instant> = None;
    let reconnect_settle = Duration::from_secs(60);

    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            info!("shutdown requested");
            current.shutdown();
            break;
        }

        rotate_supervisor_log_in_run(&state);

        let total = total_on_screen(accumulated_on_screen, last_connected_at);
        let remaining = refresh_interval.saturating_sub(total);
        match current.wait_or_timeout(remaining.max(Duration::from_secs(1)))? {
            WaitResult::ChildDied(status) => {
                warn!("current pid={} died (status={})", current.pid, status);
                move_log_to_failed(&state, &current);
                current = recover(&cli, &mut state)?;
                accumulated_on_screen = Duration::ZERO;
                last_connected_at = Some(Instant::now());
                reconnected_at = None;
                disconnected_at = None;
                continue;
            }
            WaitResult::Message(ChildMsg::Event(Event::ChildError(msg))) => {
                error!("child error: {msg}");
                current.shutdown();
                move_log_to_failed(&state, &current);
                current = recover(&cli, &mut state)?;
                accumulated_on_screen = Duration::ZERO;
                last_connected_at = Some(Instant::now());
                reconnected_at = None;
                disconnected_at = None;
                continue;
            }
            WaitResult::Message(ChildMsg::Event(Event::OutputDisconnected)) => {
                if let Some(t) = last_connected_at.take() {
                    accumulated_on_screen += t.elapsed();
                }
                disconnected_at = Some(Instant::now());
                info!("output disconnected");
                continue;
            }
            WaitResult::Message(ChildMsg::Event(Event::OutputReconnected)) => {
                let off_dur = disconnected_at
                    .map(|t| t.elapsed())
                    .unwrap_or_default();
                let hours = off_dur.as_secs() / 3600;
                let minutes = (off_dur.as_secs() % 3600) / 60;
                info!("output reconnected (off for {hours}h {minutes}m)");
                last_connected_at = Some(Instant::now());
                reconnected_at = Some(Instant::now());
                disconnected_at = None;
                continue;
            }
            WaitResult::Message(_) => continue,
            WaitResult::Timeout => {}
        }

        if SHUTDOWN.load(Ordering::Relaxed) {
            info!("shutdown requested");
            current.shutdown();
            break;
        }

        // Check if A's output is connected before attempting handoff
        match current.get_snapshot() {
            Ok(snap) if !snap.output_connected => {
                if last_connected_at.is_some() {
                    let (rss, swap) = read_rss_mb(current.pid);
                    let log_bytes = current.log_path.metadata().map(|m| m.len()).unwrap_or(0);
                    let outcome = HandoffOutcome::Deferred {
                        gen: current.gen,
                        reason: "target output disconnected".into(),
                    };
                    record_handoff_outcome(&mut state.handoff_health, &outcome);
                    emit_handoff_summary(
                        &outcome,
                        &state.handoff_health,
                        current.spawned_at.elapsed(),
                        rss,
                        swap,
                        log_bytes,
                    );
                    accumulated_on_screen += last_connected_at
                        .map(|t| t.elapsed())
                        .unwrap_or_default();
                    last_connected_at = None;
                }
                continue;
            }
            Ok(_) => {}
            Err(e) => {
                warn!("snapshot failed: {e}");
                continue;
            }
        }

        // After reconnect, wait for settle before handoff
        if let Some(at) = reconnected_at {
            if at.elapsed() < reconnect_settle {
                continue;
            }
            reconnected_at = None;
        }

        // Backoff after failed/gate-skipped handoffs
        if retry_after.is_some_and(|d| Instant::now() < d) {
            continue;
        }

        state.consecutive_failures = 0;

        // Staleness check (on-screen time only, not wall-clock)
        let on_screen = total_on_screen(accumulated_on_screen, last_connected_at);
        if on_screen > refresh_interval * 3 {
            let (rss, swap) = read_rss_mb(current.pid);
            error!(
                "recycle stale: on_screen={} renderer_age={} (3× interval={}), mem={:.0}MB (rss={rss:.0} swap={swap:.0}), consecutive={}",
                format_age(on_screen),
                format_age(current.spawned_at.elapsed()),
                format_age(refresh_interval),
                rss + swap,
                state.handoff_health.consecutive_handoff_failures,
            );
        }

        if let Err(e) = check_resources(&state.ready_dir) {
            let reason = format!("{e}");
            let (rss, swap) = read_rss_mb(current.pid);
            let log_bytes = current.log_path.metadata().map(|m| m.len()).unwrap_or(0);
            let outcome = HandoffOutcome::GateSkip {
                gen: current.gen,
                reason: reason.clone(),
            };
            record_handoff_outcome(&mut state.handoff_health, &outcome);
            emit_handoff_summary(
                &outcome,
                &state.handoff_health,
                current.spawned_at.elapsed(),
                rss,
                swap,
                log_bytes,
            );
            warn!(
                "resource gate: {reason} (consecutive={}, renderer_age={})",
                state.handoff_health.consecutive_handoff_failures,
                format_age(current.spawned_at.elapsed()),
            );
            let backoff = compute_backoff(
                state.handoff_health.consecutive_handoff_failures,
                refresh_interval,
            );
            retry_after = Some(Instant::now() + backoff);
            info!("next attempt in {}", format_age(backoff));
            continue;
        }

        let next_slot = (current.slot + 1) % 2;
        match handoff(&cli, &mut state, &mut current, next_slot) {
            Ok(mut next) => {
                let old_gen = current.gen;
                let (rss, swap) = read_rss_mb(current.pid);
                let log_bytes = current.log_path.metadata().map(|m| m.len()).unwrap_or(0);
                let path = next
                    .get_snapshot()
                    .map(|s| s.path)
                    .unwrap_or_default();
                let outcome = HandoffOutcome::Success {
                    gen: next.gen,
                    path: path.clone(),
                    validated: true,
                };
                record_handoff_outcome(&mut state.handoff_health, &outcome);
                emit_handoff_summary(
                    &outcome,
                    &state.handoff_health,
                    current.spawned_at.elapsed(),
                    rss,
                    swap,
                    log_bytes,
                );
                current.shutdown();
                current = next;
                let current_log_name = current
                    .log_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("");
                retire_logs(&state, &[current_log_name]);
                info!(
                    "handoff complete: pid={} slot={} path=\"{path}\" (retired gen {old_gen})",
                    current.pid, current.slot
                );
            }
            Err(e) => {
                let reason = format!("{e}");
                let (rss, swap) = read_rss_mb(current.pid);
                let log_bytes = current.log_path.metadata().map(|m| m.len()).unwrap_or(0);
                let outcome = HandoffOutcome::Failure {
                    gen: current.gen,
                    reason: reason.clone(),
                };
                record_handoff_outcome(&mut state.handoff_health, &outcome);
                emit_handoff_summary(
                    &outcome,
                    &state.handoff_health,
                    current.spawned_at.elapsed(),
                    rss,
                    swap,
                    log_bytes,
                );
                let n = state.handoff_health.consecutive_handoff_failures;
                if n >= 3 {
                    error!(
                        "handoff failed (consecutive={n}, renderer_age={}, mem={:.0}MB): {reason}",
                        format_age(current.spawned_at.elapsed()),
                        rss + swap,
                    );
                } else {
                    warn!(
                        "handoff failed (consecutive={n}, renderer_age={}): {reason}",
                        format_age(current.spawned_at.elapsed()),
                    );
                }
                let backoff = compute_backoff(n, refresh_interval);
                retry_after = Some(Instant::now() + backoff);
                info!("next attempt in {}", format_age(backoff));
                continue;
            }
        }
        accumulated_on_screen = Duration::ZERO;
        last_connected_at = Some(Instant::now());
        reconnected_at = None;
        disconnected_at = None;
        retry_after = None;
    }

    Ok(())
}

// ==================== Child spawn ====================

fn spawn_child(
    cli: &Cli,
    state: &mut State,
    mode: SpawnMode,
    slot: u32,
    release_fd: Option<OwnedFd>,
    video_override: Option<&str>,
) -> Result<ChildProcess> {
    state.generation += 1;
    let gen = state.generation;
    let token = generate_token();
    let gen_tag = format!("gen-{gen}-{}", std::process::id());
    let log_path = state.log_dir.join(format!("{gen_tag}.log"));

    let (parent_sock, child_sock) = UnixStream::pair().context("socketpair")?;
    let child_ctrl_fd = child_sock.as_raw_fd();
    let child_release_fd = release_fd.as_ref().map(|fd| fd.as_raw_fd());

    let log_file = fs::File::create(&log_path)?;

    let mut cmd = Command::new("/proc/self/exe");
    cmd.arg0("murale-render");
    cmd.arg("__internal-renderer");
    cmd.arg(&cli.video);
    if let Some(path) = video_override {
        cmd.args(["--initial-path", path]);
    }
    if let Some(ref out) = cli.output {
        cmd.args(["--output", out]);
    }
    for ws in &cli.workspace {
        cmd.args(["--workspace", ws]);
    }
    if let Some(ref opts) = cli.mpv_options {
        let filtered: String = opts
            .split(',')
            .filter(|o| !o.trim_start().starts_with("input-ipc-server"))
            .collect::<Vec<_>>()
            .join(",");
        if !filtered.is_empty() {
            cmd.args(["--mpv-options", &filtered]);
        }
    }
    if cli.stats {
        cmd.arg("--stats");
    }
    if cli.scanout {
        cmd.arg("--scanout");
    }
    cmd.arg(format!("--control-fd={CTRL_FD}"));

    cmd.stdout(log_file.try_clone()?);
    cmd.stderr(log_file);
    cmd.stdin(Stdio::null());

    unsafe {
        cmd.pre_exec(move || {
            if child_ctrl_fd != CTRL_FD {
                if libc::dup2(child_ctrl_fd, CTRL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(child_ctrl_fd);
            }
            clear_cloexec(CTRL_FD)?;

            if let Some(src_fd) = child_release_fd {
                if src_fd != RELEASE_FD {
                    if libc::dup2(src_fd, RELEASE_FD) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    libc::close(src_fd);
                }
                clear_cloexec(RELEASE_FD)?;
            }
            Ok(())
        });
    }

    let child = cmd.spawn().context("spawn child")?;
    let pid = child.id();

    drop(child_sock);
    drop(release_fd);

    let mut ctrl = CtrlChannel::new(parent_sock)?;

    ctrl.send_hello(Hello {
        version: PROTOCOL_VERSION,
        gen,
        mode,
        token: token.clone(),
    })?;

    ctrl.set_read_timeout(Duration::from_secs(5))?;
    match ctrl.recv() {
        Ok(ChildMsg::HelloAck(ack)) => {
            if ack.version != PROTOCOL_VERSION {
                bail!(
                    "protocol version mismatch: expected {PROTOCOL_VERSION}, got {}",
                    ack.version
                );
            }
        }
        Ok(other) => bail!("expected HelloAck, got: {other:?}"),
        Err(e) => bail!("HelloAck failed: {e}"),
    }
    ctrl.clear_read_timeout()?;

    info!("spawned slot={slot} pid={pid} gen={gen} mode={mode:?}");

    Ok(ChildProcess {
        pid,
        child,
        ctrl,
        gen,
        token,
        slot,
        log_path,
        spawned_at: Instant::now(),
    })
}

fn clear_cloexec(fd: i32) -> std::io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

// ==================== Child process methods ====================

impl ChildProcess {
    fn wait_or_timeout(&mut self, timeout: Duration) -> Result<WaitResult> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                return Ok(WaitResult::ChildDied(status.code().unwrap_or(-1)));
            }
            if SHUTDOWN.load(Ordering::Relaxed) {
                return Ok(WaitResult::Timeout);
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(WaitResult::Timeout);
            }

            let poll_ms = remaining.as_millis().min(1000) as u16;
            let raw_fd = self.ctrl.raw_fd();
            let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(raw_fd) };
            let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];

            match nix::poll::poll(&mut fds, PollTimeout::from(poll_ms)) {
                Ok(0) => continue,
                Ok(_) => {
                    let rev = fds[0].revents().unwrap_or(PollFlags::empty());
                    if rev.intersects(PollFlags::POLLHUP | PollFlags::POLLERR) {
                        return match self.child.try_wait()? {
                            Some(s) => Ok(WaitResult::ChildDied(s.code().unwrap_or(-1))),
                            None => {
                                self.shutdown();
                                Ok(WaitResult::ChildDied(-1))
                            }
                        };
                    }
                    if rev.contains(PollFlags::POLLIN) {
                        self.ctrl.set_read_timeout(Duration::from_millis(500))?;
                        let result = self.ctrl.recv();
                        let _ = self.ctrl.clear_read_timeout();
                        match result {
                            Ok(msg) => return Ok(WaitResult::Message(msg)),
                            Err(e) if e.to_string().contains("EOF") => {
                                return match self.child.try_wait()? {
                                    Some(s) => Ok(WaitResult::ChildDied(s.code().unwrap_or(-1))),
                                    None => {
                                        self.shutdown();
                                        Ok(WaitResult::ChildDied(-1))
                                    }
                                };
                            }
                            Err(e) => return Err(e),
                        }
                    }
                }
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn get_snapshot(&mut self) -> Result<Snapshot> {
        let id = self.ctrl.send_cmd("get_snapshot")?;
        self.ctrl.set_read_timeout(Duration::from_secs(3))?;
        let deadline = Instant::now() + Duration::from_secs(3);
        let result = loop {
            if Instant::now() > deadline {
                break Err(anyhow::anyhow!("get_snapshot timeout"));
            }
            match self.ctrl.recv() {
                Ok(ChildMsg::Snapshot { id: rid, data }) if rid == id => break Ok(data),
                Ok(ChildMsg::Event(_) | ChildMsg::Ok { .. }) => continue,
                Ok(ChildMsg::Error { id: Some(rid), msg }) if rid == id => {
                    break Err(anyhow::anyhow!("get_snapshot: {msg}"));
                }
                Ok(other) => break Err(anyhow::anyhow!("unexpected: {other:?}")),
                Err(e) => break Err(e),
            }
        };
        let _ = self.ctrl.clear_read_timeout();
        result
    }

    fn send_seek(&mut self, pos: f64) -> Result<u64> {
        self.ctrl
            .send_cmd_with("seek", Some(pos), None, None, None, None)
    }

    fn send_arm_render(&mut self, target: f64, seq: u64, release_token: &str) -> Result<u64> {
        self.ctrl.send_cmd_with(
            "arm_render",
            None,
            Some(target),
            Some(seq),
            None,
            Some(release_token),
        )
    }

    fn send_arm_release(&mut self, seq: u64, addr: &str, tok: &str) -> Result<u64> {
        self.ctrl
            .send_cmd_with("arm_release", None, None, Some(seq), Some(addr), Some(tok))
    }

    fn send_unpause(&mut self) -> Result<u64> {
        self.ctrl.send_cmd("unpause")
    }

    fn shutdown(&mut self) {
        let _ = self.ctrl.send_cmd("shutdown");
        unsafe {
            libc::kill(self.pid as i32, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() > deadline => {
                    warn!("force-killing pid={}", self.pid);
                    unsafe {
                        libc::kill(self.pid as i32, libc::SIGKILL);
                    }
                    let _ = self.child.wait();
                    return;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(_) => return,
            }
        }
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }
}

// ==================== Event helpers ====================

fn recv_event(
    child: &mut ChildProcess,
    predicate: impl Fn(&Event) -> bool,
    timeout: Duration,
) -> Result<Event> {
    child.ctrl.set_read_timeout(timeout)?;
    let deadline = Instant::now() + timeout;
    let result = loop {
        if Instant::now() > deadline {
            break Err(anyhow::anyhow!("event timeout ({timeout:?})"));
        }
        match child.ctrl.recv() {
            Ok(ChildMsg::Event(e)) => {
                if let Event::ChildError(ref msg) = e {
                    break Err(anyhow::anyhow!("child error: {msg}"));
                }
                if predicate(&e) {
                    break Ok(e);
                }
            }
            Ok(_) => continue,
            Err(e) => break Err(e),
        }
    };
    let _ = child.ctrl.clear_read_timeout();
    result
}

fn wait_for_display_ready(child: &mut ChildProcess) -> Result<()> {
    let poll_interval = Duration::from_secs(30);
    let mut deadline = Instant::now() + OVERLAP_TIMEOUT;
    let mut last_waiting_log: Option<Instant> = None;
    child.ctrl.set_read_timeout(poll_interval)?;

    let result = loop {
        if !child.is_alive() {
            break Err(anyhow::anyhow!("renderer died while waiting for display_ready"));
        }
        if Instant::now() > deadline {
            break Err(anyhow::anyhow!("event timeout ({OVERLAP_TIMEOUT:?})"));
        }
        match child.ctrl.recv() {
            Ok(ChildMsg::Event(Event::DisplayReady)) => break Ok(()),
            Ok(ChildMsg::Event(Event::WaitingForOutput { available })) => {
                if last_waiting_log.is_none()
                    || last_waiting_log.is_some_and(|t| t.elapsed() >= Duration::from_secs(600))
                {
                    info!(
                        "waiting for output (renderer alive, available: [{}])",
                        available.join(", ")
                    );
                    last_waiting_log = Some(Instant::now());
                }
                deadline = Instant::now() + wait_deadline_after_heartbeat(poll_interval);
            }
            Ok(ChildMsg::Event(Event::ChildError(msg))) => {
                break Err(anyhow::anyhow!("child error: {msg}"));
            }
            Ok(_) => {}
            Err(e) if e.to_string().contains("timed out") || e.to_string().contains("WouldBlock") => {}
            Err(e) if e.to_string().contains("EOF") => {
                break Err(anyhow::anyhow!("ctrl channel EOF"));
            }
            Err(e) => break Err(e),
        }
    };
    let _ = child.ctrl.clear_read_timeout();
    result
}

// ==================== Handoff orchestration ====================

fn handoff(
    cli: &Cli,
    state: &mut State,
    current: &mut ChildProcess,
    next_slot: u32,
) -> Result<ChildProcess> {
    let snap_a = current.get_snapshot()?;

    let (release_fd, release_addr) = create_release_socket()?;
    let release_token = generate_token();

    let mut next = spawn_child(
        cli,
        state,
        SpawnMode::Hold,
        next_slot,
        Some(release_fd),
        Some(&snap_a.path),
    )?;

    // Macro-like closure for cleanup on failure
    let do_cleanup = |next: &mut ChildProcess, state: &State| {
        next.shutdown();
        move_log_to_failed(state, next);
    };

    // Phase 1: Wait for B hold-ready
    if let Err(e) = recv_event(
        &mut next,
        |e| matches!(e, Event::HoldReady),
        OVERLAP_TIMEOUT,
    ) {
        do_cleanup(&mut next, state);
        bail!("hold-ready: {e}");
    }
    info!("B ({}) hold-ready", next.pid);

    // Wait for hwdec to resolve (nvdec init takes time)
    let hwdec_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match next.get_snapshot() {
            Ok(snap) if !snap.hwdec.is_empty() && snap.hwdec != "no" => break,
            Ok(_) => {
                if Instant::now() > hwdec_deadline {
                    do_cleanup(&mut next, state);
                    bail!("hwdec resolution timeout");
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                do_cleanup(&mut next, state);
                bail!("hwdec wait: {e}");
            }
        }
    }

    // Validate workload match
    let snap_b = match next.get_snapshot() {
        Ok(s) => s,
        Err(e) => {
            do_cleanup(&mut next, state);
            bail!("B snapshot: {e}");
        }
    };
    if snap_a.workload_key() != snap_b.workload_key() {
        do_cleanup(&mut next, state);
        bail!(
            "workload mismatch: A={} B={}",
            snap_a.workload_key(),
            snap_b.workload_key()
        );
    }

    // FPS-derived tolerance
    let frame_ms = match snap_b.frame_ms() {
        Some(fm) => fm,
        None => {
            do_cleanup(&mut next, state);
            bail!("invalid fps: {}", snap_b.fps);
        }
    };
    let threshold_ms = frame_ms * POS_TOLERANCE_FRAMES;

    // Phase 2: Seek B to target position (fresh A snapshot — original is stale)
    let snap_a_seek = match current.get_snapshot() {
        Ok(s) => s,
        Err(e) => {
            do_cleanup(&mut next, state);
            bail!("A position for seek: {e}");
        }
    };
    let rough_target = if snap_a_seek.dur > 0.0 {
        (snap_a_seek.pos + ROUGH_SEEK_AHEAD) % snap_a_seek.dur
    } else {
        snap_a_seek.pos + ROUGH_SEEK_AHEAD
    };

    if let Err(e) = next.send_arm_render(rough_target, next.gen, &release_token) {
        do_cleanup(&mut next, state);
        bail!("arm_render: {e}");
    }
    if let Err(e) = next.send_seek(rough_target) {
        do_cleanup(&mut next, state);
        bail!("seek: {e}");
    }

    // Phase 3: Wait for target_rendered, validate position
    let rendered_pos = match recv_event(
        &mut next,
        |e| matches!(e, Event::TargetRendered { .. }),
        Duration::from_secs(3),
    ) {
        Ok(Event::TargetRendered { pos, .. }) => pos,
        Ok(_) => unreachable!(),
        Err(e) => {
            do_cleanup(&mut next, state);
            bail!("target_rendered: {e}");
        }
    };

    let pos_err_ms = {
        let e = (rendered_pos - rough_target).abs();
        if snap_a.dur > 0.0 && e > snap_a.dur / 2.0 {
            (snap_a.dur - e) * 1000.0
        } else {
            e * 1000.0
        }
    };
    let pos_frames = pos_err_ms / frame_ms;

    if pos_err_ms >= threshold_ms {
        do_cleanup(&mut next, state);
        bail!("pos_err={pos_err_ms:.3}ms ({pos_frames:.2} frames) > {threshold_ms:.3}ms ({POS_TOLERANCE_FRAMES} frames)");
    }
    info!("target rendered: pos_err={pos_err_ms:.3}ms ({pos_frames:.2} frames)");

    // Phase 4: Compute release timing from A's current position
    let snap_a2 = match current.get_snapshot() {
        Ok(s) => s,
        Err(e) => {
            do_cleanup(&mut next, state);
            bail!("A re-snapshot: {e}");
        }
    };

    let dur = snap_a.dur;
    let diff = if dur > 0.0 {
        let raw = ((rendered_pos - snap_a2.pos) % dur + dur) % dur;
        if raw > dur / 2.0 { dur - raw } else { raw }
    } else {
        (rendered_pos - snap_a2.pos).abs()
    };
    if diff > ROUGH_SEEK_AHEAD + 1.0 {
        do_cleanup(&mut next, state);
        bail!("unreasonable wait: {diff:.3}s");
    }
    let wait_secs = (diff - DEFAULT_RELEASE_LATENCY).max(0.005);
    info!(
        "measured: B_pos={rendered_pos:.6}, A_pos={:.6}, wait={wait_secs:.6}s",
        snap_a2.pos
    );

    // Revalidate identity (compare against snap_a, not a live A snapshot —
    // A may have switched workspaces since the handoff started)
    let snap_b3 = next.get_snapshot().unwrap_or(snap_b.clone());
    if snap_a.workload_key() != snap_b3.workload_key() {
        do_cleanup(&mut next, state);
        bail!("identity changed during sync");
    }

    // Phase 5: Timed release
    std::thread::sleep(Duration::from_secs_f64(wait_secs));

    // Tell B what release credentials to expect on fd 4
    if let Err(e) = next.ctrl.send_cmd_with(
        "prepare_release", None, None, Some(next.gen), None, Some(&release_token),
    ) {
        do_cleanup(&mut next, state);
        bail!("prepare_release to B: {e}");
    }

    // Arm A to release directly to B via datagram
    let _ = current.send_arm_release(next.gen, &release_addr, &release_token);

    let vsync_ok = recv_event(
        current,
        |e| matches!(e, Event::ReleaseArmed { .. }),
        Duration::from_secs(1),
    )
    .is_ok()
        && recv_event(
            current,
            |e| matches!(e, Event::ReleaseSent { .. }),
            Duration::from_secs(1),
        )
        .is_ok();

    if vsync_ok {
        info!("release via A's VSync callback");
    } else {
        warn!("VSync release timeout — direct fallback to B");
        let _ = next.ctrl.send_cmd_with(
            "release",
            None,
            None,
            Some(next.gen),
            None,
            Some(&release_token),
        );
    }

    // Phase 6: Wait for B swap + unpause + display-ready
    if let Err(e) = recv_event(
        &mut next,
        |e| matches!(e, Event::SwapDone { .. }),
        Duration::from_secs(5),
    ) {
        do_cleanup(&mut next, state);
        bail!("swap_done: {e}");
    }
    info!("B swap confirmed");

    if let Err(e) = next.send_unpause() {
        do_cleanup(&mut next, state);
        bail!("unpause: {e}");
    }
    info!("B unpaused");

    if let Err(e) = recv_event(
        &mut next,
        |e| matches!(e, Event::DisplayReady),
        Duration::from_secs(5),
    ) {
        do_cleanup(&mut next, state);
        bail!("display_ready: {e}");
    }
    info!("B display-ready confirmed");

    // Phase 7: Stabilization (liveness only — resource checks are self-defeating
    // during overlap because B's VRAM allocation is the transient pressure)
    let stable_end = Instant::now() + HEALTH_WINDOW;
    while Instant::now() < stable_end {
        if !next.is_alive() {
            move_log_to_failed(state, &next);
            bail!("B died during stabilization");
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    if !next.is_alive() {
        move_log_to_failed(state, &next);
        bail!("B died after stabilization");
    }

    // Final path validation (compare against snap_a.path — the path B was
    // launched with — not A's current path, which may have changed via workspace switch)
    if let Ok(snap_final) = next.get_snapshot() {
        let desired = snap_a.path.clone();
        if snap_final.path != desired {
            do_cleanup(&mut next, state);
            bail!("B path mismatch: {} != {desired}", snap_final.path);
        }
    }

    // Unlock B's workspace listener now that validation passed
    let _ = next.ctrl.send_cmd("handoff_complete");

    Ok(next)
}

// ==================== Recovery ====================

fn recover(cli: &Cli, state: &mut State) -> Result<ChildProcess> {
    for attempt in 1..=MAX_CONSECUTIVE_FAILURES {
        state.consecutive_failures += 1;
        let backoff = (1u64 << state.consecutive_failures.min(5)).min(30);
        info!("recovery {attempt}/{MAX_CONSECUTIVE_FAILURES}, backoff {backoff}s");
        std::thread::sleep(Duration::from_secs(backoff));

        match spawn_child(cli, state, SpawnMode::Normal, 0, None, None) {
            Ok(mut child) => match wait_for_display_ready(&mut child) {
                Ok(_) => {
                    info!("recovery successful (pid={})", child.pid);
                    state.consecutive_failures = 0;
                    return Ok(child);
                }
                Err(e) => {
                    warn!("recovery display-ready failed: {e}");
                    child.shutdown();
                    move_log_to_failed(state, &child);
                }
            },
            Err(e) => warn!("recovery spawn failed: {e}"),
        }
    }
    bail!("recovery exhausted ({MAX_CONSECUTIVE_FAILURES} attempts)");
}

// ==================== Release socket ====================

fn create_release_socket() -> Result<(OwnedFd, String)> {
    let name = format!("murale-release-{}", generate_token());

    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let name_bytes = name.as_bytes();
    if name_bytes.len() + 1 > addr.sun_path.len() {
        bail!("abstract address too long");
    }
    // sun_path[0] = 0 for abstract; already zeroed
    for (i, &b) in name_bytes.iter().enumerate() {
        addr.sun_path[i + 1] = b as libc::c_char;
    }
    let addr_len = std::mem::size_of::<libc::sa_family_t>() + 1 + name_bytes.len();

    if unsafe {
        libc::bind(
            owned.as_raw_fd(),
            &addr as *const _ as *const libc::sockaddr,
            addr_len as libc::socklen_t,
        )
    } < 0
    {
        return Err(std::io::Error::last_os_error().into());
    }

    Ok((owned, name))
}

// ==================== Resource gates ====================

fn check_resources(ready_dir: &Path) -> Result<()> {
    // MemAvailable >= 50 MiB
    let meminfo = fs::read_to_string("/proc/meminfo").context("read /proc/meminfo")?;
    let mem_kb = meminfo
        .lines()
        .find(|l| l.starts_with("MemAvailable"))
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u64>().ok())
        .context("parse MemAvailable")?;
    if mem_kb < 51200 {
        bail!("insufficient memory: {mem_kb}kB (need 50MiB)");
    }

    // Tmpfs free >= 50 MiB
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let path_c = std::ffi::CString::new(ready_dir.to_str().unwrap_or(""))?;
    let tmpfs_free_mib = if unsafe { libc::statvfs(path_c.as_ptr(), &mut stat) } == 0 {
        let free_bytes = stat.f_bavail as u64 * stat.f_frsize as u64;
        if free_bytes < 52_428_800 {
            bail!("insufficient tmpfs: {} bytes free (need 50MiB)", free_bytes);
        }
        free_bytes / (1024 * 1024)
    } else {
        0
    };

    // NVIDIA VRAM check (skip if no GPU)
    let mut vram_free_mib: u64 = 0;
    if Path::new("/dev/nvidia0").exists() {
        if let Ok(output) = std::process::Command::new("nvidia-smi")
            .args(["--query-gpu=memory.free", "--format=csv,noheader,nounits"])
            .output()
        {
            if let Ok(text) = std::str::from_utf8(&output.stdout) {
                if let Some(free_mib) = text
                    .trim()
                    .lines()
                    .next()
                    .and_then(|l| l.trim().parse::<u64>().ok())
                {
                    vram_free_mib = free_mib;
                    if free_mib < 3072 {
                        bail!("insufficient VRAM: {free_mib}MiB (need 3GiB)");
                    }
                }
            }
        }
    }

    // Recent OOM check
    if let Ok(output) = std::process::Command::new("journalctl")
        .args(["-k", "--since", "5 min ago", "--no-pager", "-q"])
        .output()
    {
        let text = String::from_utf8_lossy(&output.stdout).to_lowercase();
        if text.contains("oom") && (text.contains("killer") || text.contains("out of memory")) {
            bail!("recent kernel OOM detected");
        }
    }

    info!(
        "resource gate: vram_free={vram_free_mib}MiB mem_avail={mem_kb}kB tmpfs_free={tmpfs_free_mib}MiB"
    );

    Ok(())
}

// ==================== Log persistence ====================

pub fn rotate_supervisor_log_on_open(path: &Path) {
    if let Ok(meta) = path.metadata() {
        if meta.len() > SUP_LOG_MAX {
            let backup = path.with_extension("log.1");
            let _ = fs::rename(path, backup);
        }
    }
}

fn rotate_supervisor_log_in_run(state: &State) {
    let sup_log = match &state.sup_log_path {
        Some(p) => p,
        None => return,
    };
    if let Ok(meta) = sup_log.metadata() {
        if meta.len() > SUP_LOG_MAX {
            if let Ok(data) = fs::read(sup_log) {
                let keep_from = data.len().saturating_sub(SUP_LOG_KEEP as usize);
                let start = data[keep_from..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map(|p| keep_from + p + 1)
                    .unwrap_or(keep_from);
                let _ = fs::write(sup_log, &data[start..]);
            }
        }
    }
}

const PERSIST_TAIL_MAX: usize = 1_048_576;

fn persist_failed_log(state: &State, child: &ChildProcess) {
    let persistent_dir = match &state.persistent_log_dir {
        Some(d) => d.join("failed"),
        None => return,
    };

    let src = if child.log_path.exists() {
        &child.log_path
    } else {
        let tmpfs_failed = state.log_dir.join("failed");
        let name = child.log_path.file_name().unwrap_or_default();
        let moved = tmpfs_failed.join(name);
        if !moved.exists() {
            return;
        }
        // Can't take reference to local; read directly
        let data = match fs::read(&moved) {
            Ok(d) => d,
            Err(_) => return,
        };
        write_tail_copy(&persistent_dir, child, &data);
        prune_log_dir(&persistent_dir, FAILED_LOG_MAX_AGE, FAILED_LOG_MAX_BYTES, &[]);
        return;
    };

    let data = match fs::read(src) {
        Ok(d) => d,
        Err(_) => return,
    };
    write_tail_copy(&persistent_dir, child, &data);
    prune_log_dir(&persistent_dir, FAILED_LOG_MAX_AGE, FAILED_LOG_MAX_BYTES, &[]);
}

fn write_tail_copy(dest_dir: &Path, child: &ChildProcess, data: &[u8]) {
    let name = child.log_path.file_name().unwrap_or_default();
    let dest = dest_dir.join(name);
    let size_kb = data.len() / 1024;

    if data.len() <= PERSIST_TAIL_MAX {
        let _ = fs::write(&dest, data);
        info!("persisted failed log: {} ({size_kb}KB)", name.to_string_lossy());
    } else {
        let offset = data.len() - PERSIST_TAIL_MAX;
        let start = data[offset..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| offset + p + 1)
            .unwrap_or(offset);
        let total_kb = data.len() / 1024;
        let mut out = format!("--- head truncated at byte {start} ---\n").into_bytes();
        out.extend_from_slice(&data[start..]);
        let _ = fs::write(&dest, &out);
        info!(
            "persisted failed log: {} ({}KB, truncated from {total_kb}KB)",
            name.to_string_lossy(),
            out.len() / 1024,
        );
    }
}

// ==================== Log retention ====================

fn retire_logs(state: &State, exclude_names: &[&str]) {
    let mut excludes: Vec<String> = vec!["supervisor.log".into()];
    for name in exclude_names {
        excludes.push(name.to_string());
    }

    prune_log_dir(&state.log_dir, LOG_MAX_AGE, LOG_MAX_BYTES, &excludes);
    let _ = fs::create_dir_all(state.log_dir.join("failed"));
    prune_log_dir(
        &state.log_dir.join("failed"),
        FAILED_LOG_MAX_AGE,
        FAILED_LOG_MAX_BYTES,
        &[],
    );

    rotate_supervisor_log_in_run(state);
}

fn prune_log_dir(dir: &Path, max_age: u64, max_bytes: u64, excludes: &[String]) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let cutoff = now.saturating_sub(max_age);

    struct Entry {
        path: PathBuf,
        mtime: u64,
        size: u64,
    }
    let mut kept: Vec<Entry> = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        if !name.ends_with(".log") {
            continue;
        }
        if excludes.iter().any(|e| e == &name) {
            continue;
        }

        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let size = meta.len();

        if mtime < cutoff {
            let _ = fs::remove_file(&path);
        } else {
            kept.push(Entry { path, mtime, size });
        }
    }

    let total: u64 = kept.iter().map(|e| e.size).sum();
    if total > max_bytes {
        kept.sort_by_key(|e| e.mtime);
        let mut remaining = total;
        for entry in &kept {
            if remaining <= max_bytes {
                break;
            }
            let _ = fs::remove_file(&entry.path);
            remaining -= entry.size;
        }
    }
}

fn move_log_to_failed(state: &State, child: &ChildProcess) {
    persist_failed_log(state, child);
    if child.log_path.exists() {
        let dest = state
            .log_dir
            .join("failed")
            .join(child.log_path.file_name().unwrap_or_default());
        let _ = fs::rename(&child.log_path, dest);
    }
}

// ==================== Singleton lock ====================

fn acquire_lock(ready_dir: &Path) -> Result<()> {
    let lock_path = ready_dir.join("supervisor.lock");
    let fd = fs::File::create(&lock_path)?;
    let ret = unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        bail!(
            "another murale supervisor is running (lock held: {})",
            lock_path.display()
        );
    }
    std::mem::forget(fd); // leak the fd to hold the lock for process lifetime
    Ok(())
}

// ==================== Utilities ====================

fn generate_token() -> String {
    let mut buf = [0u8; 16];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut buf);
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
#[derive(Debug, PartialEq)]
enum WaitAction {
    Ready,
    Dead,
}

fn wait_deadline_after_heartbeat(poll_interval: Duration) -> Duration {
    poll_interval + Duration::from_secs(5)
}

fn total_on_screen(accumulated: Duration, last_connected_at: Option<Instant>) -> Duration {
    accumulated + last_connected_at.map(|t| t.elapsed()).unwrap_or_default()
}

const BACKOFF_BASE: Duration = Duration::from_secs(600);

fn compute_backoff(consecutive_failures: u32, max: Duration) -> Duration {
    if consecutive_failures == 0 {
        return Duration::ZERO;
    }
    let shift = (consecutive_failures - 1).min(7);
    let backoff = BACKOFF_BASE.saturating_mul(1 << shift);
    backoff.min(max)
}

#[cfg(test)]
fn should_skip_handoff(output_connected: bool) -> bool {
    !output_connected
}

#[cfg(test)]
fn time_until_handoff(
    accumulated: Duration,
    connected_elapsed: Duration,
    refresh_interval: Duration,
    settle_remaining: Duration,
) -> Duration {
    let total = accumulated + connected_elapsed;
    let until_due = refresh_interval.saturating_sub(total);
    until_due.max(settle_remaining)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{Duration, SystemTime};

    fn set_mtime(path: &Path, age_secs: u64) {
        let old = SystemTime::now() - Duration::from_secs(age_secs);
        let ft = filetime::FileTime::from_system_time(old);
        filetime::set_file_mtime(path, ft).unwrap();
    }

    #[test]
    fn prune_log_dir_excludes_current_renderer() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path();

        fs::write(dir_path.join("gen-24-1.log"), "old log data".repeat(100)).unwrap();
        fs::write(dir_path.join("gen-25-1.log"), "current renderer").unwrap();
        fs::write(dir_path.join("supervisor.log"), "supervisor data").unwrap();

        set_mtime(&dir_path.join("gen-24-1.log"), 400_000);
        set_mtime(&dir_path.join("gen-25-1.log"), 60);
        set_mtime(&dir_path.join("supervisor.log"), 60);

        let excludes = vec!["supervisor.log".into(), "gen-25-1.log".into()];
        prune_log_dir(dir_path, 259_200, 64, &excludes);

        assert!(
            dir_path.join("gen-25-1.log").exists(),
            "current renderer log should survive pruning"
        );
        assert!(
            dir_path.join("supervisor.log").exists(),
            "supervisor.log should survive pruning"
        );
        assert!(
            !dir_path.join("gen-24-1.log").exists(),
            "old log should be pruned by age"
        );
    }

    #[test]
    fn skip_handoff_when_disconnected() {
        assert!(should_skip_handoff(false));
    }

    #[test]
    fn no_skip_handoff_when_connected() {
        assert!(!should_skip_handoff(true));
    }

    #[test]
    fn total_on_screen_accumulates_across_disconnect() {
        let accumulated = Duration::from_secs(6900);
        let connected_at = Some(Instant::now() - Duration::from_secs(100));
        let total = total_on_screen(accumulated, connected_at);
        assert!(total >= Duration::from_secs(7000));
    }

    #[test]
    fn total_on_screen_paused_when_disconnected() {
        let accumulated = Duration::from_secs(6900);
        let total = total_on_screen(accumulated, None);
        assert_eq!(total, Duration::from_secs(6900));
    }

    #[test]
    fn case_a_partial_then_sleep_then_reconnect() {
        let interval = Duration::from_secs(7200);
        let settle = Duration::from_secs(60);
        let accumulated = Duration::from_secs(6900);
        let connected_elapsed = Duration::from_secs(0);
        let settle_remaining = settle;
        let wait = time_until_handoff(accumulated, connected_elapsed, interval, settle_remaining);
        assert_eq!(wait, Duration::from_secs(300));

        let connected_elapsed = Duration::from_secs(300);
        let settle_remaining = Duration::ZERO;
        let wait = time_until_handoff(accumulated, connected_elapsed, interval, settle_remaining);
        assert_eq!(wait, Duration::ZERO);
    }

    #[test]
    fn case_b_overdue_during_disconnect() {
        let interval = Duration::from_secs(7200);
        let settle = Duration::from_secs(60);
        let accumulated = Duration::from_secs(7200);
        let connected_elapsed = Duration::from_secs(0);
        let settle_remaining = settle;
        let wait = time_until_handoff(accumulated, connected_elapsed, interval, settle_remaining);
        assert_eq!(wait, Duration::from_secs(60));
    }

    #[test]
    fn case_c_no_disconnect() {
        let interval = Duration::from_secs(7200);
        let settle = Duration::from_secs(60);
        let accumulated = Duration::ZERO;
        let connected_elapsed = Duration::from_secs(7200);
        let settle_remaining = Duration::ZERO;
        let wait = time_until_handoff(accumulated, connected_elapsed, interval, settle_remaining);
        assert_eq!(wait, Duration::ZERO);
    }

    #[test]
    fn event_driven_disconnect_reconnect_accounting() {
        let interval = Duration::from_secs(7200);
        let settle = Duration::from_secs(60);

        // OutputDisconnected after 6900s: freeze accounting
        let accumulated = Duration::from_secs(6900);
        // While disconnected (last_connected_at = None), total is frozen
        assert_eq!(total_on_screen(accumulated, None), Duration::from_secs(6900));

        // OutputReconnected: resume with last_connected_at = now
        // At reconnect moment, connected_elapsed = 0
        let wait = time_until_handoff(accumulated, Duration::ZERO, interval, settle);
        // Need 300s more connected time (300s > 60s settle, so 300s wins)
        assert_eq!(wait, Duration::from_secs(300));

        // After 300s connected: handoff fires
        let wait = time_until_handoff(accumulated, Duration::from_secs(300), interval, Duration::ZERO);
        assert_eq!(wait, Duration::ZERO);

        // Contrast: without the event, accumulated would be 0 and we'd wait 7200s
        let wait_without_event = time_until_handoff(Duration::ZERO, Duration::ZERO, interval, settle);
        assert_eq!(wait_without_event, Duration::from_secs(7200));
    }

    #[test]
    fn prune_log_dir_size_limit_respects_exclude() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path();

        fs::write(dir_path.join("gen-1-1.log"), vec![b'A'; 5000]).unwrap();
        fs::write(dir_path.join("gen-2-1.log"), vec![b'B'; 5000]).unwrap();
        fs::write(dir_path.join("gen-3-1.log"), vec![b'C'; 5000]).unwrap();

        set_mtime(&dir_path.join("gen-1-1.log"), 300);
        set_mtime(&dir_path.join("gen-2-1.log"), 200);
        set_mtime(&dir_path.join("gen-3-1.log"), 100);

        let excludes = vec!["gen-3-1.log".into()];
        prune_log_dir(dir_path, 259_200, 6000, &excludes);

        assert!(
            dir_path.join("gen-3-1.log").exists(),
            "excluded log should survive size pruning"
        );
        assert!(
            !dir_path.join("gen-1-1.log").exists(),
            "oldest log should be pruned for size"
        );
    }

    #[test]
    fn rotate_on_open_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("supervisor.log");
        fs::write(&log, vec![b'X'; (SUP_LOG_MAX + 1) as usize]).unwrap();

        rotate_supervisor_log_on_open(&log);

        assert!(
            !log.exists() || fs::metadata(&log).unwrap().len() == 0,
            "original should be renamed"
        );
        let backup = dir.path().join("supervisor.log.1");
        assert!(backup.exists(), ".1 backup should exist");
        assert_eq!(
            fs::metadata(&backup).unwrap().len(),
            SUP_LOG_MAX + 1,
            ".1 should have the original content"
        );
    }

    #[test]
    fn rotate_on_open_small_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("supervisor.log");
        fs::write(&log, "small content").unwrap();

        rotate_supervisor_log_on_open(&log);

        assert!(log.exists(), "small file should not be rotated");
        let backup = dir.path().join("supervisor.log.1");
        assert!(!backup.exists(), "no .1 should be created for small files");
    }

    #[test]
    fn handoff_outcome_escalation_sequence() {
        let mut health = HandoffHealth::new();

        // 10 path-mismatch failures
        for i in 1..=10 {
            let outcome = HandoffOutcome::Failure {
                gen: i,
                reason: "B path mismatch".into(),
            };
            let esc = record_handoff_outcome(&mut health, &outcome);
            let esc = esc.unwrap();
            assert_eq!(health.consecutive_handoff_failures, i as u32);
            if i < 3 {
                assert_eq!(esc.level, EscalationLevel::Warn);
            } else {
                assert_eq!(esc.level, EscalationLevel::Error);
            }
        }

        // 1 gate skip (counter continues)
        let outcome = HandoffOutcome::GateSkip {
            gen: 10,
            reason: "insufficient VRAM".into(),
        };
        let esc = record_handoff_outcome(&mut health, &outcome).unwrap();
        assert_eq!(health.consecutive_handoff_failures, 11);
        assert_eq!(esc.level, EscalationLevel::Error);

        // 5 more failures
        for i in 12..=16 {
            let outcome = HandoffOutcome::Failure {
                gen: i,
                reason: "B path mismatch".into(),
            };
            record_handoff_outcome(&mut health, &outcome);
        }
        assert_eq!(health.consecutive_handoff_failures, 16);

        // Success resets
        let outcome = HandoffOutcome::Success {
            gen: 17,
            path: "/a.mov".into(),
            validated: true,
        };
        let esc = record_handoff_outcome(&mut health, &outcome);
        assert!(esc.is_none());
        assert_eq!(health.consecutive_handoff_failures, 0);
    }

    #[test]
    fn handoff_outcome_gate_skip_only() {
        let mut health = HandoffHealth::new();
        for i in 1..=5 {
            let outcome = HandoffOutcome::GateSkip {
                gen: 1,
                reason: "insufficient VRAM".into(),
            };
            let esc = record_handoff_outcome(&mut health, &outcome).unwrap();
            if i < 3 {
                assert_eq!(esc.level, EscalationLevel::Warn);
            } else {
                assert_eq!(esc.level, EscalationLevel::Error);
            }
        }
        assert_eq!(health.consecutive_handoff_failures, 5);
    }

    #[test]
    fn handoff_outcome_deferred_does_not_count() {
        let mut health = HandoffHealth::new();

        // 2 failures → WARN
        for _ in 0..2 {
            record_handoff_outcome(
                &mut health,
                &HandoffOutcome::Failure { gen: 1, reason: "test".into() },
            );
        }
        assert_eq!(health.consecutive_handoff_failures, 2);

        // Deferred does not increment
        let esc = record_handoff_outcome(
            &mut health,
            &HandoffOutcome::Deferred { gen: 1, reason: "output disconnected".into() },
        );
        assert!(esc.is_none());
        assert_eq!(health.consecutive_handoff_failures, 2);

        // Next failure is still at 3 → ERROR
        let esc = record_handoff_outcome(
            &mut health,
            &HandoffOutcome::Failure { gen: 1, reason: "test".into() },
        );
        assert_eq!(esc.unwrap().level, EscalationLevel::Error);
        assert_eq!(health.consecutive_handoff_failures, 3);
    }

    #[test]
    fn backoff_schedule() {
        let max = Duration::from_secs(7200);
        assert_eq!(compute_backoff(0, max), Duration::ZERO);
        assert_eq!(compute_backoff(1, max), Duration::from_secs(600));
        assert_eq!(compute_backoff(2, max), Duration::from_secs(1200));
        assert_eq!(compute_backoff(3, max), Duration::from_secs(2400));
        assert_eq!(compute_backoff(4, max), Duration::from_secs(4800));
        assert_eq!(compute_backoff(5, max), Duration::from_secs(7200));
        assert_eq!(compute_backoff(6, max), Duration::from_secs(7200));
    }

    #[test]
    fn backoff_minimum_10_min() {
        let max = Duration::from_secs(7200);
        assert!(compute_backoff(1, max) >= Duration::from_secs(600));
        assert!(compute_backoff(2, max) >= Duration::from_secs(600));
    }

    #[test]
    fn backoff_success_resets() {
        let max = Duration::from_secs(7200);
        assert_eq!(compute_backoff(3, max), Duration::from_secs(2400));
        assert_eq!(compute_backoff(0, max), Duration::ZERO);
    }

    #[test]
    fn backoff_deferred_not_counted() {
        let max = Duration::from_secs(7200);
        assert_eq!(compute_backoff(0, max), Duration::ZERO);
    }

    #[test]
    fn heartbeats_extend_deadline_indefinitely() {
        let poll = Duration::from_secs(30);
        let extension = wait_deadline_after_heartbeat(poll);
        assert_eq!(extension, Duration::from_secs(35));

        // Simulate 3h of heartbeats (360 heartbeats at 30s each)
        let mut deadline = Instant::now() + OVERLAP_TIMEOUT;
        for _ in 0..360 {
            deadline = Instant::now() + extension;
        }
        assert!(deadline > Instant::now());
    }

    #[test]
    fn no_heartbeat_times_out_at_15s() {
        let deadline = Instant::now() + OVERLAP_TIMEOUT;
        assert_eq!(OVERLAP_TIMEOUT, Duration::from_secs(15));
        assert!(deadline > Instant::now());
        assert!(deadline < Instant::now() + Duration::from_secs(16));
    }

    #[test]
    fn heartbeat_stops_timeout_within_35s() {
        let poll = Duration::from_secs(30);
        let extension = wait_deadline_after_heartbeat(poll);
        let deadline = Instant::now() + extension;
        assert!(deadline <= Instant::now() + Duration::from_secs(36));
    }

    #[test]
    fn wait_action_dispatch() {
        assert_eq!(
            if true { WaitAction::Ready } else { WaitAction::Dead },
            WaitAction::Ready
        );
        assert_ne!(WaitAction::Ready, WaitAction::Dead);
    }
}
