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
    generation: u64,
    consecutive_failures: u32,
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
}

enum WaitResult {
    Timeout,
    ChildDied(i32),
    Message(ChildMsg),
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
    let ready_dir = PathBuf::from(&xdg).join("murale");
    fs::create_dir_all(&ready_dir)?;

    let log_dir = ready_dir.join("logs");
    fs::create_dir_all(&log_dir)?;
    fs::create_dir_all(log_dir.join("failed"))?;

    acquire_lock(&ready_dir)?;

    let refresh_interval = std::env::var("MURALE_REFRESH_INTERVAL")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(7200));

    let mut state = State {
        ready_dir,
        log_dir,
        generation: 0,
        consecutive_failures: 0,
    };

    retire_logs(&state, &[]);

    let mut current = spawn_child(&cli, &mut state, SpawnMode::Normal, 0, None, None)?;
    recv_event(
        &mut current,
        |e| matches!(e, Event::DisplayReady),
        OVERLAP_TIMEOUT,
    )
    .context("initial display-ready timeout")?;
    info!("initial instance running (pid={})", current.pid);

    loop {
        if SHUTDOWN.load(Ordering::Relaxed) {
            info!("shutdown requested");
            current.shutdown();
            break;
        }

        match current.wait_or_timeout(refresh_interval)? {
            WaitResult::ChildDied(status) => {
                warn!("current pid={} died (status={})", current.pid, status);
                move_log_to_failed(&state, &current);
                current = recover(&cli, &mut state)?;
                continue;
            }
            WaitResult::Message(ChildMsg::Event(Event::ChildError(msg))) => {
                error!("child error: {msg}");
                current.shutdown();
                move_log_to_failed(&state, &current);
                current = recover(&cli, &mut state)?;
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

        state.consecutive_failures = 0;

        if let Err(e) = check_resources(&state.ready_dir) {
            warn!("resource gate: {e}");
            continue;
        }

        let next_slot = (current.slot + 1) % 2;
        match handoff(&cli, &mut state, &mut current, next_slot) {
            Ok(next) => {
                let old_gen = current.gen;
                current.shutdown();
                current = next;
                retire_logs(&state, &[&current.gen.to_string()]);
                info!(
                    "handoff complete: pid={} slot={} (retired gen {old_gen})",
                    current.pid, current.slot
                );
            }
            Err(e) => {
                warn!("handoff failed: {e}");
                continue;
            }
        }
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

    let child_video = video_override.unwrap_or(&cli.video);

    let mut cmd = Command::new("/proc/self/exe");
    cmd.arg("__internal-renderer");
    cmd.arg(child_video);
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

    // Revalidate identity
    let snap_a3 = current.get_snapshot().unwrap_or(snap_a.clone());
    let snap_b3 = next.get_snapshot().unwrap_or(snap_b.clone());
    if snap_a3.workload_key() != snap_b3.workload_key() {
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

    // Phase 7: Stabilization
    let stable_end = Instant::now() + HEALTH_WINDOW;
    while Instant::now() < stable_end {
        if !next.is_alive() {
            move_log_to_failed(state, &next);
            bail!("B died during stabilization");
        }
        if let Err(e) = check_resources(&state.ready_dir) {
            do_cleanup(&mut next, state);
            bail!("resource pressure: {e}");
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    if !next.is_alive() {
        move_log_to_failed(state, &next);
        bail!("B died after stabilization");
    }

    // Final path validation
    if let Ok(snap_final) = next.get_snapshot() {
        let desired = current
            .get_snapshot()
            .map(|s| s.path)
            .unwrap_or_else(|_| snap_a.path.clone());
        if snap_final.path != desired {
            do_cleanup(&mut next, state);
            bail!("B path mismatch: {} != {desired}", snap_final.path);
        }
    }

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
            Ok(mut child) => match recv_event(
                &mut child,
                |e| matches!(e, Event::DisplayReady),
                OVERLAP_TIMEOUT,
            ) {
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
    if unsafe { libc::statvfs(path_c.as_ptr(), &mut stat) } == 0 {
        let free_bytes = stat.f_bavail as u64 * stat.f_frsize as u64;
        if free_bytes < 52_428_800 {
            bail!("insufficient tmpfs: {} bytes free (need 50MiB)", free_bytes);
        }
    }

    // NVIDIA VRAM check (skip if no GPU)
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

    Ok(())
}

// ==================== Log retention ====================

fn retire_logs(state: &State, exclude_gens: &[&str]) {
    let mut excludes: Vec<String> = vec!["supervisor.log".into()];
    for g in exclude_gens {
        excludes.push(format!("{g}.log"));
    }

    prune_log_dir(&state.log_dir, LOG_MAX_AGE, LOG_MAX_BYTES, &excludes);
    let _ = fs::create_dir_all(state.log_dir.join("failed"));
    prune_log_dir(
        &state.log_dir.join("failed"),
        FAILED_LOG_MAX_AGE,
        FAILED_LOG_MAX_BYTES,
        &[],
    );

    // Rotate supervisor.log if > 2 MiB
    let sup_log = state.log_dir.join("supervisor.log");
    if let Ok(meta) = sup_log.metadata() {
        if meta.len() > SUP_LOG_MAX {
            if let Ok(data) = fs::read(&sup_log) {
                let keep_from = data.len().saturating_sub(SUP_LOG_KEEP as usize);
                let start = data[keep_from..]
                    .iter()
                    .position(|&b| b == b'\n')
                    .map(|p| keep_from + p + 1)
                    .unwrap_or(keep_from);
                let _ = fs::write(&sup_log, &data[start..]);
            }
        }
    }
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
