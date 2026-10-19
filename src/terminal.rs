//! UU remote terminal (CT_TERMINAL) over BINARY_DATA_CHANNEL.
//! Frame layout recovered from the official Mac host codec (UURemoteServer 0x1001a7834 decode,
//! 0x1001a7480 encode): "TERM", version u8 = 1, type u8, two flag bytes, session_id u32 LE,
//! payload_len u32 LE (<= 1 MiB), payload. Type values are TerminalFrameType case index + 1.
//! Control payloads are JSON (cols/rows/shell/option, code/message, exit_code, password).
//! Sessions outlive a viewer connection (like the official tmux-backed sessions) and replay
//! a bounded scrollback on reattach. Terminal input/output bytes are never logged.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc::UnboundedSender;

#[path = "terminal_view.rs"]
mod terminal_view;
#[path = "terminal_acceptance.rs"]
mod acceptance_probe;

pub use acceptance_probe::acceptance;

pub const OPEN: u8 = 1;
pub const STDIN: u8 = 2;
pub const RESIZE: u8 = 3;
pub const CLOSE: u8 = 4;
pub const STDOUT: u8 = 5;
pub const EXIT: u8 = 6;
pub const SIGNAL: u8 = 7;
pub const ERROR: u8 = 8;
pub const LIST_REQ: u8 = 9;
pub const LIST_RESP: u8 = 10;
pub const KILL_REQ: u8 = 11;
pub const KILL_RESP: u8 = 12;
pub const OPEN_RESP: u8 = 13;
pub const UNLOCK_REQ: u8 = 14;
pub const UNLOCK_RESP: u8 = 15;
pub const DETACH: u8 = 16;

const MAGIC: &[u8; 4] = b"TERM";
const HEADER: usize = 16;
const MAX_PAYLOAD: usize = 0x10_0000;
const SCROLLBACK: usize = 256 * 1024;
const OUTPUT_CHUNK: usize = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame { pub kind: u8, pub flags: [u8; 2], pub session: u32, pub payload: Vec<u8> }

pub fn encode(kind: u8, session: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + payload.len());
    out.extend_from_slice(MAGIC);
    out.push(1);
    out.push(kind);
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&session.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    out
}

pub fn decode(data: &[u8]) -> Option<Frame> {
    if data.len() < HEADER || &data[..4] != MAGIC { return None; }
    let kind = data[5];
    if !(1..=16).contains(&kind) { return None; }
    let session = u32::from_le_bytes(data[8..12].try_into().ok()?);
    let len = u32::from_le_bytes(data[12..16].try_into().ok()?) as usize;
    if len > MAX_PAYLOAD || data.len() != HEADER + len { return None; }
    Some(Frame { kind, flags: [data[6], data[7]], session, payload: data[HEADER..].to_vec() })
}

pub fn is_frame(data: &[u8]) -> bool { data.len() >= 4 && &data[..4] == MAGIC }

struct Session {
    id: u32,
    shell: String,
    created_ms: u64,
    last_active_ms: AtomicU64,
    master: OwnedFd,
    pid: i32,
    sink: Mutex<Option<UnboundedSender<Vec<u8>>>>,
    scrollback: Mutex<VecDeque<u8>>,
    size: Mutex<(u16, u16)>,
    view: Mutex<Option<terminal_view::View>>,
}

impl Session {
    /// Lock order sink → scrollback → view matches OPEN/ensure_view. Holding the sink lock through
    /// the view fanout makes a chunk appear in replay or live output, never both.
    fn record_output(&self, data: &[u8]) {
        self.touch();
        let mut sink = self.sink.lock().unwrap();
        {
            let mut sb = self.scrollback.lock().unwrap();
            sb.extend(data);
            let excess = sb.len().saturating_sub(SCROLLBACK);
            sb.drain(..excess);
        }
        if let Some(tx) = sink.as_ref() {
            if tx.send(encode(STDOUT, self.id, data)).is_err() { *sink = None; }
        }
        let mut view = self.view.lock().unwrap();
        let Some(current) = view.as_ref() else { return };
        if let Err(error) = current.output(data) {
            let reason = match error {
                terminal_view::SendError::QueueFull => "queue_full",
                terminal_view::SendError::Closed => "writer_closed",
                terminal_view::SendError::Oversized => "oversized_frame",
                terminal_view::SendError::InvalidCheck => "invalid_check",
            };
            if let Some(mut old) = view.take() { old.close_window(); }
            log::warn!("Terminal local view dropped: session={} reason={}", self.id, reason);
        }
    }
    fn touch(&self) { self.last_active_ms.fetch_max(now_ms(), Ordering::Relaxed); }
    /// Serialize snapshot/install against record_output, so output is replayed or live, never both.
    fn ensure_view(&self) {
        if !terminal_view::available() { let _ = terminal_view::spawn(self.id, 80, 24); return; }
        let _sink = self.sink.lock().unwrap();
        let size = self.size.lock().unwrap();
        let scrollback = self.scrollback.lock().unwrap();
        let mut view = self.view.lock().unwrap();
        if view.as_mut().is_some_and(terminal_view::View::is_alive) { return; }
        if let Some(mut old) = view.take() { old.close_window(); }
        let Some(mut next) = terminal_view::spawn(self.id, size.0, size.1) else { return };
        let replay: Vec<u8> = scrollback.iter().copied().collect();
        for chunk in replay.chunks(OUTPUT_CHUNK) {
            if next.output(chunk).is_err() {
                next.close_window();
                log::warn!("Terminal local view dropped: session={} reason=replay_queue_full", self.id);
                return;
            }
        }
        *view = Some(next);
    }
    fn emit(&self, kind: u8, payload: &[u8]) -> bool {
        let mut sink = self.sink.lock().unwrap();
        if let Some(tx) = sink.as_ref() {
            if tx.send(encode(kind, self.id, payload)).is_ok() { return true; }
            *sink = None;
        }
        false
    }
    // The size lock is held across the ioctl so a delayed restore cannot reapply a stale size.
    fn resize(&self, cols: u16, rows: u16) {
        let mut size = self.size.lock().unwrap();
        *size = (cols, rows);
        self.set_winsize(cols, rows);
        self.view_resize(cols, rows);
    }
    fn view_resize(&self, cols: u16, rows: u16) {
        let mut view = self.view.lock().unwrap();
        let Some(current) = view.as_ref() else { return };
        if let Err(error) = current.resize(cols, rows) {
            let reason = match error {
                terminal_view::SendError::QueueFull => "queue_full",
                terminal_view::SendError::Closed => "writer_closed",
                terminal_view::SendError::Oversized => "oversized_frame",
                terminal_view::SendError::InvalidCheck => "invalid_check",
            };
            if let Some(mut old) = view.take() { old.close_window(); }
            log::warn!("Terminal local view dropped: session={} reason={}", self.id, reason);
        }
    }
    fn set_winsize(&self, cols: u16, rows: u16) {
        let win = libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        unsafe { libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ as _, &win); }
    }
    /// Full-screen programs repaint only on SIGWINCH, which an unchanged size never raises:
    /// shrink by one row, then restore the latest requested size after a pause.
    fn force_redraw(self: &Arc<Self>) {
        {
            let size = self.size.lock().unwrap();
            if size.1 <= 1 { return; }
            self.set_winsize(size.0, size.1 - 1);
        }
        let s = self.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            let size = s.size.lock().unwrap();
            s.set_winsize(size.0, size.1);
        });
    }
    fn write(&self, data: &[u8]) {
        let mut off = 0;
        while off < data.len() {
            let n = unsafe { libc::write(self.master.as_raw_fd(), data[off..].as_ptr().cast(), data.len() - off) };
            if n <= 0 { break; }
            off += n as usize;
        }
    }
    fn signal(&self, sig: i32) { unsafe { libc::kill(-self.pid, sig); } }
}

fn registry() -> &'static Mutex<HashMap<u32, Arc<Session>>> {
    static REG: OnceLock<Mutex<HashMap<u32, Arc<Session>>>> = OnceLock::new();
    REG.get_or_init(Default::default)
}
static NEXT_ID: AtomicU32 = AtomicU32::new(1);

fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64 }

fn spawn(requested_id: u32, cols: u16, rows: u16, shell: Option<&str>) -> Result<Arc<Session>, String> {
    use std::os::unix::process::CommandExt;
    let win = nix::pty::Winsize { ws_row: rows.max(1), ws_col: cols.max(1), ws_xpixel: 0, ws_ypixel: 0 };
    let pty = nix::pty::openpty(Some(&win), None).map_err(|e| format!("openpty failed: {e}"))?;
    // The renderer receives only the output pipe; do not let it retain this or other sessions' PTYs.
    for fd in [&pty.master, &pty.slave] {
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(format!("pty close-on-exec failed: {}", std::io::Error::last_os_error()));
        }
    }
    let shell = shell.filter(|s| s.starts_with('/') && std::path::Path::new(s).exists()).map(str::to_owned)
        .or_else(|| std::env::var("SHELL").ok().filter(|s| std::path::Path::new(s).exists()))
        .unwrap_or_else(|| "/bin/bash".into());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let slave = pty.slave;
    let io = |fd: &OwnedFd| fd.try_clone().map(std::process::Stdio::from).map_err(|e| format!("pty dup failed: {e}"));
    let mut cmd = std::process::Command::new(&shell);
    cmd.arg("-l").current_dir(&home)
        .env("TERM", "xterm-256color").env("COLORTERM", "truecolor")
        .env("LANG", std::env::var("LANG").unwrap_or_else(|_| "C.UTF-8".into()))
        .stdin(io(&slave)?).stdout(io(&slave)?).stderr(io(&slave)?);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 { return Err(std::io::Error::last_os_error()); }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 { return Err(std::io::Error::last_os_error()); }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("shell spawn failed: {e}"))?;
    drop(slave);
    // Official Mac 4.42.0 (TerminalManager.open) allocates an ID only for OPEN(0).
    let id = if requested_id != 0 { requested_id } else {
        loop {
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            if id != 0 && !registry().lock().unwrap().contains_key(&id) { break id; }
        }
    };
    let created_ms = now_ms();
    let session = Arc::new(Session { id, shell: shell.clone(), created_ms, last_active_ms: AtomicU64::new(created_ms), pid: child.id() as i32,
        master: pty.master, sink: Mutex::new(None), scrollback: Mutex::new(VecDeque::new()), size: Mutex::new((cols, rows)),
        view: Mutex::new(terminal_view::spawn(id, cols, rows)) });
    let mut reader = std::fs::File::from(session.master.try_clone().map_err(|e| format!("pty dup failed: {e}"))?);
    let worker = session.clone();
    std::thread::Builder::new().name(format!("uu-term-{id}")).spawn(move || {
        let mut buf = vec![0u8; OUTPUT_CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => worker.record_output(&buf[..n]),
            }
        }
        let code = child.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
        log::info!("Terminal session exited: id={} exit_code={}", worker.id, code);
        worker.emit(EXIT, serde_json::json!({"exit_code": code}).to_string().as_bytes());
        registry().lock().unwrap().remove(&worker.id);
    }).map_err(|e| format!("reader thread failed: {e}"))?;
    registry().lock().unwrap().insert(id, session.clone());
    log::info!("Terminal session spawned: id={} cols={} rows={}", id, cols, rows);
    Ok(session)
}

/// Ends every terminal session (interactive shells ignore SIGTERM, which otherwise
/// makes systemd wait for its stop timeout). Called when the server shuts down.
pub fn shutdown_all() {
    let sessions: Vec<Arc<Session>> = registry().lock().unwrap().values().cloned().collect();
    for s in &sessions {
        s.signal(libc::SIGHUP);
        if let Some(mut view) = s.view.lock().unwrap().take() { view.close_window(); }
    }
    if !sessions.is_empty() {
        std::thread::sleep(std::time::Duration::from_millis(300));
        for s in &sessions { s.signal(libc::SIGKILL); }
        log::info!("Terminal sessions closed on shutdown: count={}", sessions.len());
    }
}

fn json(payload: &[u8]) -> serde_json::Value { serde_json::from_slice(payload).unwrap_or(serde_json::Value::Null) }
fn dims(v: &serde_json::Value) -> (u16, u16) {
    let get = |k: &str, d: u64| v.get(k).and_then(|x| x.as_u64().or_else(|| x.as_f64().map(|f| f as u64))).unwrap_or(d).clamp(1, 1000) as u16;
    (get("cols", 80), get("rows", 24))
}
fn signal_number(payload: &[u8]) -> i32 {
    let text = String::from_utf8_lossy(payload);
    let v = json(payload);
    let name = v.as_str().map(str::to_owned).or_else(|| v.as_object().and_then(|o| o.values().find_map(|x| x.as_str().map(str::to_owned))))
        .unwrap_or_else(|| text.into_owned());
    if let Some(n) = v.as_object().and_then(|o| o.values().find_map(|x| x.as_i64())) { return n as i32; }
    match name.trim().trim_start_matches("SIG") {
        "TERM" => libc::SIGTERM, "KILL" => libc::SIGKILL, "HUP" => libc::SIGHUP, "QUIT" => libc::SIGQUIT,
        "ABRT" => libc::SIGABRT, "PIPE" => libc::SIGPIPE, "FPE" => libc::SIGFPE, "SEGV" => libc::SIGSEGV, "ILL" => libc::SIGILL,
        _ => libc::SIGINT,
    }
}
fn reply(code: i64, message: &str) -> Vec<u8> { serde_json::json!({"code": code, "message": message}).to_string().into_bytes() }

/// One viewer connection's terminal endpoint; `out` sends complete frames on BINARY_DATA_CHANNEL.
pub struct TerminalConn { out: UnboundedSender<Vec<u8>> }

/// Runs the read-only VTE helper requested by the private `__terminal-view` subcommand.
pub fn run_view_helper(session_id: u32, cols: u16, rows: u16) {
    terminal_view::run_helper(session_id, cols, rows);
}

impl TerminalConn {
    pub fn new(out: UnboundedSender<Vec<u8>>) -> Self { Self { out } }
    fn send(&self, kind: u8, session: u32, payload: &[u8]) { let _ = self.out.send(encode(kind, session, payload)); }

    pub fn handle(&self, data: &[u8]) {
        let Some(frame) = decode(data) else { log::warn!("Terminal frame rejected: bytes={}", data.len()); return; };
        if frame.kind != STDIN { log::info!("Terminal frame: type={} session={} payload_bytes={}", frame.kind, frame.session, frame.payload.len()); }
        let existing = registry().lock().unwrap().get(&frame.session).cloned();
        if matches!(frame.kind, OPEN | STDIN | RESIZE | SIGNAL | DETACH) {
            if let Some(s) = &existing { s.touch(); }
        }
        match frame.kind {
            OPEN => {
                let v = json(&frame.payload);
                let (cols, rows) = dims(&v);
                let reattach = existing.is_some();
                let session = match existing {
                    Some(s) => { s.resize(cols, rows); Ok(s) }
                    None => spawn(frame.session, cols, rows, v.get("shell").and_then(|s| s.as_str())),
                };
                match session {
                    Ok(s) => {
                        // Queue the acknowledgement before the PTY reader can emit live output.
                        let mut sink = s.sink.lock().unwrap();
                        self.send(OPEN_RESP, s.id, &reply(0, "ok"));
                        *sink = Some(self.out.clone());
                        let replay: Vec<u8> = s.scrollback.lock().unwrap().iter().copied().collect();
                        // A full scrollback exceeds SCTP's 64 KiB outbound message limit.
                        for chunk in replay.chunks(OUTPUT_CHUNK) { self.send(STDOUT, s.id, chunk); }
                        drop(sink);
                        s.ensure_view();
                        // The replay tail starts mid-redraw for full-screen programs.
                        if reattach { s.force_redraw(); }
                    }
                    Err(e) => { log::warn!("Terminal open failed: {}", e); self.send(OPEN_RESP, frame.session, &reply(1, &e)); }
                }
            }
            STDIN => if let Some(s) = existing { s.write(&frame.payload) },
            RESIZE => if let Some(s) = existing { let (c, r) = dims(&json(&frame.payload)); s.resize(c, r) },
            SIGNAL => if let Some(s) = existing { s.signal(signal_number(&frame.payload)) },
            CLOSE => if let Some(s) = existing { *s.sink.lock().unwrap() = None; s.signal(libc::SIGHUP); },
            DETACH => if let Some(s) = existing { *s.sink.lock().unwrap() = None },
            KILL_REQ => {
                // Windows 4.43 terminal_management_model sends a request ID in the header,
                // and the target session_id in JSON. Empty payloads retain legacy header targeting.
                let id = if frame.payload.is_empty() { Some(frame.session) } else {
                    json(&frame.payload).get("session_id").and_then(|x| x.as_u64()).and_then(|v| u32::try_from(v).ok())
                }.filter(|id| *id != 0);
                let Some(id) = id else { self.send(KILL_RESP, frame.session, &reply(1, "Invalid session_id")); return; };
                let target = registry().lock().unwrap().get(&id).cloned();
                match target {
                    Some(s) => { *s.sink.lock().unwrap() = None; s.signal(libc::SIGHUP); s.signal(libc::SIGKILL); self.send(KILL_RESP, frame.session, &reply(0, "ok")); }
                    None => self.send(KILL_RESP, frame.session, &reply(1, "Session not found")),
                }
            }
            LIST_REQ => {
                let sessions: Vec<serde_json::Value> = registry().lock().unwrap().values().map(|s| {
                    let attached = s.sink.lock().unwrap().is_some();
                    serde_json::json!({"session_id": s.id, "name": format!("bash-{}", s.id), "shell": s.shell,
                        "state": if attached { "running" } else { "detached" }, "created_at_ms": s.created_ms,
                        "last_active_ms": s.last_active_ms.load(Ordering::Relaxed)})
                }).collect();
                log::info!("Terminal list sessions: count={}", sessions.len());
                // Official Mac 4.42.0 sends a bare array with the request's header ID.
                // Windows 4.42.0 terminal_management_model (0x1410a209a) requires last_active_ms in every row.
                self.send(LIST_RESP, frame.session, serde_json::Value::Array(sessions).to_string().as_bytes());
            }
            UNLOCK_REQ => self.send(UNLOCK_RESP, frame.session, &reply(0, "ok")),
            _ => self.send(ERROR, frame.session, &reply(1, "unsupported frame")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_round_trip_matches_official_layout() {
        let f = encode(OPEN_RESP, 7, b"{}");
        assert_eq!(&f[..8], &[b'T', b'E', b'R', b'M', 1, 13, 0, 0]);
        assert_eq!(&f[8..16], &[7, 0, 0, 0, 2, 0, 0, 0]);
        assert_eq!(decode(&f), Some(Frame { kind: 13, flags: [0, 0], session: 7, payload: b"{}".to_vec() }));
        assert_eq!(decode(&f[..17]), None);
        let mut bad = f.clone(); bad[5] = 17; assert_eq!(decode(&bad), None);
    }
    #[test]
    fn signal_names_map_to_posix_numbers() {
        assert_eq!(signal_number(b"SIGTERM"), libc::SIGTERM);
        assert_eq!(signal_number(br#"{"signal":"SIGKILL"}"#), libc::SIGKILL);
        assert_eq!(signal_number(b"whatever"), libc::SIGINT);
    }
    #[test]
    fn list_reply_is_an_array_and_preserves_request_id() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let conn = TerminalConn::new(tx);
        for request in [0, 7, u32::MAX] {
            conn.handle(&encode(LIST_REQ, request, &[]));
            let response = decode(&rx.try_recv().unwrap()).unwrap();
            assert_eq!(response.kind, LIST_RESP);
            assert_eq!(response.session, request);
            assert!(json(&response.payload).is_array());
        }
    }
    #[test]
    fn kill_management_targets_payload_and_preserves_other_sessions() {
        struct Cleanup(Vec<Arc<Session>>);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                for s in &self.0 {
                    let live = registry().lock().unwrap().get(&s.id).is_some_and(|v| Arc::ptr_eq(v, s));
                    if live { s.signal(libc::SIGKILL); }
                }
            }
        }
        let mut cleanup = Cleanup(Vec::new());
        let target = spawn(u32::MAX - 10, 80, 24, Some("/bin/sh")).unwrap();
        cleanup.0.push(target.clone());
        let other = spawn(u32::MAX - 11, 80, 24, Some("/bin/sh")).unwrap();
        cleanup.0.push(other.clone());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let conn = TerminalConn::new(tx);
        let response = |rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>, request, code| {
            let f = decode(&rx.try_recv().unwrap()).unwrap();
            assert_eq!(f.kind, KILL_RESP);
            assert_eq!(f.session, request, "KILL response must echo the request ID");
            assert_eq!(json(&f.payload)["code"], code);
        };
        let wait_removed = |id| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while registry().lock().unwrap().contains_key(&id) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(!registry().lock().unwrap().contains_key(&id), "killed shell must be reaped and removed");
        };

        // A request ID can equal another live session ID; it is never the payload's kill target.
        let payload = serde_json::json!({"session_id": target.id}).to_string();
        conn.handle(&encode(KILL_REQ, other.id, payload.as_bytes()));
        response(&mut rx, other.id, 0);
        wait_removed(target.id);
        assert_eq!(unsafe { libc::kill(other.pid, 0) }, 0, "unselected shell must survive");

        for payload in [b"{".as_slice(), b"{}", b"[]", b"null", br#"{"session_id":0}"#,
            br#"{"session_id":-1}"#, br#"{"session_id":1.5}"#, br#"{"session_id":"1"}"#,
            br#"{"session_id":4294967296}"#, br#"{"session_id":8589934580}"#] {
            conn.handle(&encode(KILL_REQ, other.id, payload));
            response(&mut rx, other.id, 1);
        }
        assert!(registry().lock().unwrap().contains_key(&other.id));
        assert_eq!(unsafe { libc::kill(other.pid, 0) }, 0, "invalid payload must not fall back to header");
        conn.handle(&encode(KILL_REQ, 0, &[]));
        response(&mut rx, 0, 1);
        conn.handle(&encode(KILL_REQ, 7, payload.as_bytes()));
        response(&mut rx, 7, 1); // well-formed target that has already exited

        conn.handle(&encode(KILL_REQ, other.id, &[])); // legacy header-target client
        response(&mut rx, other.id, 0);
        wait_removed(other.id);
    }
    #[test]
    fn force_redraw_shrinks_then_restores_latest_size() {
        let pty = nix::pty::openpty(None, None).unwrap();
        let session = Arc::new(Session { id: u32::MAX - 2, shell: "/bin/sh".into(), created_ms: now_ms(),
            last_active_ms: AtomicU64::new(now_ms()),
            master: pty.master, pid: 0, sink: Mutex::new(None),
            scrollback: Mutex::new(VecDeque::new()), size: Mutex::new((80, 24)), view: Mutex::new(None) });
        let rows = |s: &Session| {
            let mut win = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
            unsafe { libc::ioctl(s.master.as_raw_fd(), libc::TIOCGWINSZ as _, &mut win); }
            (win.ws_col, win.ws_row)
        };
        session.resize(100, 30);
        session.force_redraw();
        assert_eq!(rows(&session), (100, 29));
        session.resize(120, 40);
        std::thread::sleep(std::time::Duration::from_millis(400));
        assert_eq!(rows(&session), (120, 40), "restore must use the latest requested size");
    }
    #[test]
    fn reconnect_replays_large_history_in_bounded_frames() {
        let pty = nix::pty::openpty(None, None).unwrap();
        let id = u32::MAX - 1;
        let history: Vec<u8> = (0..SCROLLBACK).map(|n| (n % 251) as u8).collect();
        let session = Arc::new(Session { id, shell: "/bin/sh".into(), created_ms: now_ms(),
            last_active_ms: AtomicU64::new(now_ms()),
            master: pty.master, pid: 0, sink: Mutex::new(None),
            scrollback: Mutex::new(history.iter().copied().collect()), size: Mutex::new((80, 24)), view: Mutex::new(None) });
        registry().lock().unwrap().insert(id, session.clone());
        struct Cleanup(u32);
        impl Drop for Cleanup { fn drop(&mut self) { registry().lock().unwrap().remove(&self.0); } }
        let _cleanup = Cleanup(id);

        for _ in 0..2 {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let conn = TerminalConn::new(tx);
            session.last_active_ms.store(1, Ordering::Relaxed);
            conn.handle(&encode(OPEN, id, br#"{"cols":80,"rows":24}"#));
            let opened = decode(&rx.try_recv().unwrap()).unwrap();
            assert_eq!(opened.kind, OPEN_RESP);
            assert_eq!(opened.session, id);
            let mut replay = Vec::new();
            while replay.len() < history.len() {
                let packet = rx.try_recv().unwrap();
                // SCTP's default outbound limit includes the TERM header.
                assert!(packet.len() <= 65536, "terminal replay exceeds the SCTP message limit");
                let frame = decode(&packet).unwrap();
                assert_eq!(frame.kind, STDOUT);
                assert_eq!(frame.session, id);
                replay.extend(frame.payload);
            }
            assert!(replay == history, "terminal replay must preserve every byte in order");
            assert!(session.emit(STDOUT, b"live output"));
            assert_eq!(decode(&rx.try_recv().unwrap()).unwrap().payload, b"live output");
            conn.handle(&encode(LIST_REQ, 7, &[]));
            assert_eq!(decode(&rx.try_recv().unwrap()).unwrap().kind, LIST_RESP);
            conn.handle(&encode(DETACH, id, &[]));
            conn.handle(&encode(LIST_REQ, 8, &[]));
            let list = decode(&rx.try_recv().unwrap()).unwrap();
            let sessions = json(&list.payload);
            let row = sessions.as_array().unwrap().iter().find(|s| s["session_id"] == id).unwrap();
            assert_eq!(row["state"], "detached");
            assert!(row["last_active_ms"].as_u64().unwrap() >= session.created_ms);
        }
    }
    #[test]
    fn open_spawns_shell_and_streams_output() {
        for requested_id in [0, u32::MAX] {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let conn = TerminalConn::new(tx);
            conn.handle(&encode(OPEN, requested_id, br#"{"cols":100,"rows":30,"shell":"/bin/sh"}"#));
            let resp = decode(&rx.blocking_recv().unwrap()).unwrap();
            assert_eq!(resp.kind, OPEN_RESP);
            assert_eq!(json(&resp.payload)["code"], 0);
            assert_ne!(resp.session, 0);
            if requested_id != 0 { assert_eq!(resp.session, requested_id); }

            let (list_tx, mut list_rx) = tokio::sync::mpsc::unbounded_channel();
            TerminalConn::new(list_tx).handle(&encode(LIST_REQ, 7, &[]));
            let list = decode(&list_rx.try_recv().unwrap()).unwrap();
            let sessions: Vec<serde_json::Value> = serde_json::from_slice(&list.payload).unwrap();
            let session = sessions.iter().find(|s| s["session_id"] == resp.session).unwrap();
            assert_eq!(session["shell"], "/bin/sh");
            assert_eq!(session["state"], "running");
            // Windows 4.42.0's terminal_management_model rejects a nonempty list
            // unless every row includes both integer timestamps (empty lists skip this).
            let last_active = session["last_active_ms"].as_u64().expect("Windows requires last_active_ms");
            assert!(last_active >= session["created_at_ms"].as_u64().unwrap());

            conn.handle(&encode(STDIN, resp.session, b"echo uu-term-ok; exit 3\n"));
            let mut seen = Vec::new();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut exit = None;
            while std::time::Instant::now() < deadline && exit.is_none() {
                if let Ok(m) = rx.try_recv() {
                    let f = decode(&m).unwrap();
                    assert_eq!(f.session, resp.session);
                    if f.kind == STDOUT { seen.extend(f.payload) } else if f.kind == EXIT { exit = Some(json(&f.payload)["exit_code"].clone()) }
                } else { std::thread::sleep(std::time::Duration::from_millis(20)); }
            }
            assert!(String::from_utf8_lossy(&seen).contains("uu-term-ok"));
            assert_eq!(exit, Some(serde_json::json!(3)));
        }
    }
}

/// Terminal RPCs carried in Message 21 RpcRequest: 28 terminal_check_env_req (CheckEnvReq, empty)
/// -> RpcResponse 24 CheckEnvResp{1 code, 2 message}; 29 RenameSessionReq{1 session_id, 2 name}
/// -> RpcResponse 26 RenameSessionResp{1 session_id, 2 code, 3 message}. Returns the RpcResponse body.
pub fn rpc_response(rpc: &[crate::protocol::WireField]) -> Option<Vec<u8>> {
    use crate::protocol::WireValue;
    fn varint(mut v: u64, out: &mut Vec<u8>) { while v >= 128 { out.push((v as u8 & 127) | 128); v >>= 7; } out.push(v as u8); }
    fn bytes(field: u64, v: &[u8], out: &mut Vec<u8>) { varint(field << 3 | 2, out); varint(v.len() as u64, out); out.extend_from_slice(v); }
    let header = rpc.iter().find_map(|f| match (f.number, &f.value) { (1, WireValue::Bytes(b)) => Some(*b), _ => None }).unwrap_or(&[]);
    let mut out = Vec::new();
    bytes(1, header, &mut out);
    if rpc.iter().any(|f| f.number == 28) {
        let mut body = Vec::new();
        bytes(2, b"ok", &mut body); // code 0 (default) = available
        bytes(24, &body, &mut out);
        log::info!("Terminal check_env answered: available");
        return Some(out);
    }
    if let Some(WireValue::Bytes(req)) = rpc.iter().find(|f| f.number == 29).map(|f| &f.value) {
        let fields = crate::protocol::decode_wire(req).ok()?;
        let session = fields.iter().find_map(|f| match (f.number, &f.value) { (1, WireValue::Varint(v)) => Some(*v), _ => None }).unwrap_or(0);
        let mut body = Vec::new();
        if session != 0 { varint(1 << 3, &mut body); varint(session, &mut body); }
        bytes(3, b"ok", &mut body);
        bytes(26, &body, &mut out);
        log::info!("Terminal rename answered: session={}", session);
        return Some(out);
    }
    None
}
