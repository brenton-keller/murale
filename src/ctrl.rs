use std::io::{BufRead, BufReader, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;
pub const CTRL_FD: i32 = 3;
pub const RELEASE_FD: i32 = 4;
const MAX_MSG_LEN: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpawnMode {
    Normal,
    Hold,
}

// --- Supervisor -> Child ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub version: u32,
    pub gen: u64,
    pub mode: SpawnMode,
    pub token: String,
}

#[derive(Serialize)]
struct HelloWire {
    hello: Hello,
}

#[derive(Serialize)]
struct CmdWire {
    id: u64,
    cmd: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pos: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    abstract_addr: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    release_token: Option<String>,
}

// --- Child -> Supervisor ---

#[derive(Debug, Deserialize, Clone)]
pub struct HelloAck {
    pub version: u32,
    pub pid: u32,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub path: String,
    pub pos: f64,
    pub dur: f64,
    pub fps: f64,
    pub codec: String,
    pub hwdec: String,
    pub width: u32,
    pub height: u32,
}

impl Snapshot {
    pub fn workload_key(&self) -> String {
        let basename = self.path.rsplit('/').next().unwrap_or(&self.path);
        format!(
            "{}|{}x{}|{}|{:.0}|{}",
            basename, self.width, self.height, self.codec, self.fps, self.hwdec
        )
    }

    pub fn frame_ms(&self) -> Option<f64> {
        if self.fps.is_finite() && self.fps > 0.0 {
            Some(1000.0 / self.fps)
        } else {
            None
        }
    }
}

#[derive(Debug)]
pub enum ChildMsg {
    HelloAck(HelloAck),
    Snapshot { id: u64, data: Snapshot },
    Ok { id: u64 },
    Event(Event),
    Error { id: Option<u64>, msg: String },
}

#[derive(Debug, Clone)]
pub enum Event {
    HoldReady,
    TargetRendered { gen: u64, pos: f64 },
    ReleaseArmed { seq: u64 },
    ReleaseSent { seq: u64 },
    SwapDone { seq: u64 },
    DisplayReady,
    ChildError(String),
}

// --- Channel I/O ---

pub struct CtrlChannel {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
    next_id: u64,
}

impl CtrlChannel {
    pub fn new(stream: UnixStream) -> Result<Self> {
        let reader_stream = stream.try_clone().context("clone ctrl stream")?;
        Ok(Self {
            stream,
            reader: BufReader::new(reader_stream),
            next_id: 1,
        })
    }

    pub fn raw_fd(&self) -> i32 {
        self.stream.as_raw_fd()
    }

    pub fn set_read_timeout(&self, timeout: Duration) -> Result<()> {
        self.stream.set_read_timeout(Some(timeout))?;
        Ok(())
    }

    pub fn clear_read_timeout(&self) -> Result<()> {
        self.stream.set_read_timeout(None)?;
        Ok(())
    }

    pub fn send_hello(&mut self, hello: Hello) -> Result<()> {
        let wire = HelloWire { hello };
        self.write_line(&serde_json::to_string(&wire)?)
    }

    pub fn send_cmd(&mut self, cmd: &str) -> Result<u64> {
        self.send_cmd_with(cmd, None, None, None, None, None)
    }

    pub fn send_cmd_with(
        &mut self,
        cmd: &str,
        pos: Option<f64>,
        target: Option<f64>,
        seq: Option<u64>,
        abstract_addr: Option<&str>,
        release_token: Option<&str>,
    ) -> Result<u64> {
        let id = self.next_id;
        self.next_id += 1;
        let wire = CmdWire {
            id,
            cmd: cmd.to_string(),
            pos,
            target,
            seq,
            abstract_addr: abstract_addr.map(String::from),
            release_token: release_token.map(String::from),
        };
        self.write_line(&serde_json::to_string(&wire)?)?;
        Ok(id)
    }

    pub fn recv(&mut self) -> Result<ChildMsg> {
        let mut line = String::new();
        let n = self.reader.read_line(&mut line)?;
        if n == 0 {
            bail!("control channel EOF");
        }
        if line.len() > MAX_MSG_LEN {
            bail!("message exceeds {} bytes", MAX_MSG_LEN);
        }
        parse_child_msg(&line)
    }

    fn write_line(&mut self, json: &str) -> Result<()> {
        self.stream.write_all(json.as_bytes())?;
        self.stream.write_all(b"\n")?;
        self.stream.flush()?;
        Ok(())
    }
}

fn parse_child_msg(line: &str) -> Result<ChildMsg> {
    let v: serde_json::Value = serde_json::from_str(line.trim()).context("invalid JSON")?;
    let obj = v.as_object().context("expected JSON object")?;

    if let Some(ack_val) = obj.get("hello_ack") {
        let ack: HelloAck = serde_json::from_value(ack_val.clone())?;
        return Ok(ChildMsg::HelloAck(ack));
    }

    if let Some(evt) = obj.get("event").and_then(|v| v.as_str()) {
        let event = match evt {
            "hold_ready" => Event::HoldReady,
            "display_ready" => Event::DisplayReady,
            "target_rendered" => Event::TargetRendered {
                gen: obj.get("gen").and_then(|v| v.as_u64()).unwrap_or(0),
                pos: obj.get("pos").and_then(|v| v.as_f64()).unwrap_or(0.0),
            },
            "release_armed" => Event::ReleaseArmed {
                seq: obj.get("seq").and_then(|v| v.as_u64()).unwrap_or(0),
            },
            "release_sent" => Event::ReleaseSent {
                seq: obj.get("seq").and_then(|v| v.as_u64()).unwrap_or(0),
            },
            "swap_done" => Event::SwapDone {
                seq: obj.get("seq").and_then(|v| v.as_u64()).unwrap_or(0),
            },
            "error" => Event::ChildError(
                obj.get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
            ),
            other => bail!("unknown event: {other}"),
        };
        return Ok(ChildMsg::Event(event));
    }

    if let Some(id) = obj.get("id").and_then(|v| v.as_u64()) {
        if let Some(snap) = obj.get("snapshot").and_then(|v| v.as_object()) {
            let data = Snapshot {
                path: snap
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                pos: snap.get("pos").and_then(|v| v.as_f64()).unwrap_or(0.0),
                dur: snap.get("dur").and_then(|v| v.as_f64()).unwrap_or(0.0),
                fps: snap.get("fps").and_then(|v| v.as_f64()).unwrap_or(0.0),
                codec: snap
                    .get("codec")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                hwdec: snap
                    .get("hwdec")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .into(),
                width: snap.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                height: snap.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32,
            };
            return Ok(ChildMsg::Snapshot { id, data });
        }
        if obj.get("result").and_then(|v| v.as_str()) == Some("ok") {
            return Ok(ChildMsg::Ok { id });
        }
        if let Some(msg) = obj.get("error").and_then(|v| v.as_str()) {
            return Ok(ChildMsg::Error {
                id: Some(id),
                msg: msg.into(),
            });
        }
    }

    bail!("unrecognized message: {}", line.trim())
}

// ====== Child-side protocol (renderer process) ======

#[derive(Debug)]
pub enum SupervisorCmd {
    Hello(Hello),
    GetSnapshot {
        id: u64,
    },
    Seek {
        id: u64,
        pos: f64,
    },
    ArmRender {
        id: u64,
        target: f64,
        seq: Option<u64>,
        release_token: Option<String>,
    },
    Release {
        id: u64,
        seq: u64,
        release_token: String,
    },
    ArmRelease {
        id: u64,
        seq: u64,
        abstract_addr: String,
        release_token: String,
    },
    PrepareRelease {
        id: u64,
        seq: u64,
        release_token: String,
    },
    Unpause {
        id: u64,
    },
    Shutdown {
        id: u64,
    },
}

impl SupervisorCmd {
    pub fn is_shutdown(&self) -> bool {
        matches!(self, Self::Shutdown { .. })
    }
}

pub fn parse_supervisor_msg(line: &str) -> Result<SupervisorCmd> {
    let v: serde_json::Value =
        serde_json::from_str(line.trim()).context("invalid JSON from supervisor")?;
    let obj = v.as_object().context("expected JSON object")?;

    if let Some(hello_val) = obj.get("hello") {
        let hello: Hello =
            serde_json::from_value(hello_val.clone()).context("invalid hello payload")?;
        return Ok(SupervisorCmd::Hello(hello));
    }

    let cmd = obj
        .get("cmd")
        .and_then(|v| v.as_str())
        .context("missing 'cmd' field")?;
    let id = obj.get("id").and_then(|v| v.as_u64()).unwrap_or(0);

    match cmd {
        "get_snapshot" => Ok(SupervisorCmd::GetSnapshot { id }),
        "seek" => Ok(SupervisorCmd::Seek {
            id,
            pos: obj
                .get("pos")
                .and_then(|v| v.as_f64())
                .context("seek: missing pos")?,
        }),
        "arm_render" => Ok(SupervisorCmd::ArmRender {
            id,
            target: obj
                .get("target")
                .and_then(|v| v.as_f64())
                .context("arm_render: missing target")?,
            seq: obj.get("seq").and_then(|v| v.as_u64()),
            release_token: obj
                .get("release_token")
                .and_then(|v| v.as_str())
                .map(String::from),
        }),
        "release" => Ok(SupervisorCmd::Release {
            id,
            seq: obj
                .get("seq")
                .and_then(|v| v.as_u64())
                .context("release: missing seq")?,
            release_token: obj
                .get("release_token")
                .and_then(|v| v.as_str())
                .context("release: missing release_token")?
                .into(),
        }),
        "arm_release" => Ok(SupervisorCmd::ArmRelease {
            id,
            seq: obj
                .get("seq")
                .and_then(|v| v.as_u64())
                .context("arm_release: missing seq")?,
            abstract_addr: obj
                .get("abstract_addr")
                .and_then(|v| v.as_str())
                .context("arm_release: missing abstract_addr")?
                .into(),
            release_token: obj
                .get("release_token")
                .and_then(|v| v.as_str())
                .context("arm_release: missing release_token")?
                .into(),
        }),
        "prepare_release" => Ok(SupervisorCmd::PrepareRelease {
            id,
            seq: obj
                .get("seq")
                .and_then(|v| v.as_u64())
                .context("prepare_release: missing seq")?,
            release_token: obj
                .get("release_token")
                .and_then(|v| v.as_str())
                .context("prepare_release: missing release_token")?
                .into(),
        }),
        "unpause" => Ok(SupervisorCmd::Unpause { id }),
        "shutdown" => Ok(SupervisorCmd::Shutdown { id }),
        other => bail!("unknown command: {other}"),
    }
}

/// Nonblocking control channel for the child (renderer) side.
pub struct ChildCtrlChannel {
    stream: UnixStream,
    buf: Vec<u8>,
    len: usize,
}

impl ChildCtrlChannel {
    pub fn from_fd(fd: std::os::fd::OwnedFd) -> std::io::Result<Self> {
        let stream: UnixStream = fd.into();
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            buf: vec![0u8; 4096],
            len: 0,
        })
    }

    pub fn send_line(&mut self, json: &str) -> std::io::Result<()> {
        use std::io::Write;
        let data = format!("{json}\n");
        let bytes = data.as_bytes();
        let mut written = 0;
        while written < bytes.len() {
            match self.stream.write(&bytes[written..]) {
                Ok(n) => written += n,
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn try_recv_line(&mut self) -> std::io::Result<Option<String>> {
        use std::io::Read;
        if let Some(nl) = self.buf[..self.len].iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.buf[..nl]).into_owned();
            let rest = self.len - nl - 1;
            self.buf.copy_within(nl + 1..self.len, 0);
            self.len = rest;
            return Ok(Some(line));
        }
        if self.len >= self.buf.len() {
            if self.buf.len() >= MAX_MSG_LEN {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "ctrl message exceeds limit",
                ));
            }
            self.buf.resize(self.buf.len() * 2, 0);
        }
        match self.stream.read(&mut self.buf[self.len..]) {
            Ok(0) => Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "ctrl channel EOF",
            )),
            Ok(n) => {
                self.len += n;
                if let Some(nl) = self.buf[..self.len].iter().position(|&b| b == b'\n') {
                    let line = String::from_utf8_lossy(&self.buf[..nl]).into_owned();
                    let rest = self.len - nl - 1;
                    self.buf.copy_within(nl + 1..self.len, 0);
                    self.len = rest;
                    Ok(Some(line))
                } else {
                    Ok(None)
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn try_recv_cmd(&mut self) -> std::io::Result<Option<SupervisorCmd>> {
        match self.try_recv_line()? {
            Some(line) => parse_supervisor_msg(&line)
                .map(Some)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
            None => Ok(None),
        }
    }

    pub fn send_hello_ack(&mut self) -> std::io::Result<()> {
        let json = serde_json::json!({
            "hello_ack": { "version": PROTOCOL_VERSION, "pid": std::process::id() }
        });
        self.send_line(&json.to_string())
    }

    pub fn send_snapshot(&mut self, id: u64, snap: &Snapshot) -> std::io::Result<()> {
        let json = serde_json::json!({
            "id": id,
            "snapshot": {
                "path": snap.path, "pos": snap.pos, "dur": snap.dur,
                "fps": snap.fps, "codec": snap.codec, "hwdec": snap.hwdec,
                "width": snap.width, "height": snap.height,
            }
        });
        self.send_line(&json.to_string())
    }

    pub fn send_event(&mut self, name: &str) -> std::io::Result<()> {
        self.send_line(&serde_json::json!({"event": name}).to_string())
    }

    pub fn send_target_rendered(&mut self, gen: u64, pos: f64) -> std::io::Result<()> {
        self.send_line(
            &serde_json::json!({"event": "target_rendered", "gen": gen, "pos": pos}).to_string(),
        )
    }

    pub fn send_swap_done(&mut self, seq: u64) -> std::io::Result<()> {
        self.send_line(&serde_json::json!({"event": "swap_done", "seq": seq}).to_string())
    }

    pub fn send_release_armed(&mut self, seq: u64) -> std::io::Result<()> {
        self.send_line(&serde_json::json!({"event": "release_armed", "seq": seq}).to_string())
    }

    pub fn send_release_sent(&mut self, seq: u64) -> std::io::Result<()> {
        self.send_line(&serde_json::json!({"event": "release_sent", "seq": seq}).to_string())
    }

    pub fn send_ok(&mut self, id: u64) -> std::io::Result<()> {
        self.send_line(&serde_json::json!({"id": id, "result": "ok"}).to_string())
    }

    pub fn send_error_msg(&mut self, msg: &str) -> std::io::Result<()> {
        self.send_line(&serde_json::json!({"event": "error", "msg": msg}).to_string())
    }

    pub fn send_cmd_error(&mut self, id: u64, msg: &str) -> std::io::Result<()> {
        self.send_line(&serde_json::json!({"id": id, "error": msg}).to_string())
    }
}

impl std::os::fd::AsFd for ChildCtrlChannel {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.stream.as_fd()
    }
}

// --- Validate that a raw fd is an open socket ---

pub fn validate_socket_fd(raw_fd: i32) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(raw_fd, &mut stat) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    if (stat.st_mode & libc::S_IFMT) != libc::S_IFSOCK {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("fd {raw_fd} is not a socket"),
        ));
    }
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) })
}

// --- Release datagram (24 bytes: u64 seq LE + [u8; 16] token) ---

pub const RELEASE_DATAGRAM_SIZE: usize = 24;

pub fn encode_release_datagram(seq: u64, token: &[u8; 16]) -> [u8; RELEASE_DATAGRAM_SIZE] {
    let mut buf = [0u8; RELEASE_DATAGRAM_SIZE];
    buf[..8].copy_from_slice(&seq.to_le_bytes());
    buf[8..].copy_from_slice(token);
    buf
}

pub fn decode_release_datagram(buf: &[u8; RELEASE_DATAGRAM_SIZE]) -> (u64, [u8; 16]) {
    let seq = u64::from_le_bytes(buf[..8].try_into().unwrap());
    let mut token = [0u8; 16];
    token.copy_from_slice(&buf[8..]);
    (seq, token)
}

// --- Abstract Unix datagram socket (A connects to B's release endpoint) ---

pub fn connect_abstract_dgram(name: &str) -> std::io::Result<std::os::unix::net::UnixDatagram> {
    use std::os::fd::FromRawFd;
    let name_bytes = name.as_bytes();
    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as _;
    if name_bytes.len() + 1 > addr.sun_path.len() {
        unsafe { libc::close(fd) };
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "abstract address too long",
        ));
    }
    for (i, &b) in name_bytes.iter().enumerate() {
        addr.sun_path[i + 1] = b as i8;
    }
    let addr_len =
        (std::mem::size_of::<libc::sa_family_t>() + 1 + name_bytes.len()) as libc::socklen_t;
    if unsafe { libc::connect(fd, &addr as *const _ as *const libc::sockaddr, addr_len) } != 0 {
        let err = std::io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }
    Ok(unsafe { std::os::unix::net::UnixDatagram::from_raw_fd(fd) })
}

// --- Hex decode (for release token bytes) ---

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}
