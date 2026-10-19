//! Local, read-only VTE view for an existing remote PTY session.
//!
//! GTK runs in a helper process so it never races the assist window's GTK thread. The helper only
//! receives bytes from the session's existing PTY reader; it neither spawns nor controls a shell.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const VIEW_QUEUE_MESSAGES: usize = 256;
const MAX_OUTPUT_FRAME: usize = 16 * 1024;
const MAX_VERIFY_TEXT: usize = 256;
const MAX_STATUS_EVENTS: usize = 8;

#[derive(Debug)]
enum Message {
    Output(Vec<u8>),
    Resize(u16, u16),
    VerifyGrid,
    VerifyText { present: bool, needle: Vec<u8> },
}

#[derive(Default)]
struct State {
    ready: Option<bool>,
    checks: VecDeque<u8>,
}

struct Signals {
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Clone)]
pub(crate) struct Status(Arc<Signals>);

impl Status {
    fn new() -> Self {
        Self(Arc::new(Signals {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        }))
    }

    fn receive(&self, status: u8) {
        let mut state = self.0.state.lock().unwrap();
        match status {
            0 => state.ready = Some(false),
            1 => state.ready = Some(true),
            2..=5 => {
                if state.checks.len() == MAX_STATUS_EVENTS {
                    state.checks.pop_front();
                }
                state.checks.push_back(status);
            }
            _ => {}
        }
        self.0.changed.notify_all();
    }

    /// `None` means the mapped/failure handshake did not arrive before the deadline.
    pub(crate) fn wait_ready(&self, timeout: Duration) -> Option<bool> {
        let deadline = Instant::now() + timeout;
        let mut state = self.0.state.lock().unwrap();
        loop {
            if let Some(ready) = state.ready {
                return Some(ready);
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let (next, timed) = self.0.changed.wait_timeout(state, remaining).unwrap();
            state = next;
            if timed.timed_out() && state.ready.is_none() {
                return None;
            }
        }
    }

    fn wait_check(&self, success: u8, failure: u8, timeout: Duration) -> Option<bool> {
        let deadline = Instant::now() + timeout;
        let mut state = self.0.state.lock().unwrap();
        loop {
            if let Some(index) = state
                .checks
                .iter()
                .position(|v| *v == success || *v == failure)
            {
                return state.checks.remove(index).map(|v| v == success);
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let (next, timed) = self.0.changed.wait_timeout(state, remaining).unwrap();
            state = next;
            if timed.timed_out() && !state.checks.iter().any(|v| *v == success || *v == failure) {
                return None;
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct Control {
    tx: SyncSender<Message>,
    status: Status,
}

impl Control {
    pub(crate) fn status(&self) -> Status {
        self.status.clone()
    }

    pub(crate) fn verify_grid(&self, timeout: Duration) -> Result<bool, CheckError> {
        self.tx
            .try_send(Message::VerifyGrid)
            .map_err(map_send_error)?;
        self.status
            .wait_check(2, 3, timeout)
            .ok_or(CheckError::Timeout)
    }

    pub(crate) fn verify_text(
        &self,
        needle: &[u8],
        present: bool,
        timeout: Duration,
    ) -> Result<bool, CheckError> {
        if needle.is_empty() || needle.len() > MAX_VERIFY_TEXT {
            return Err(CheckError::Send(SendError::InvalidCheck));
        }
        self.tx
            .try_send(Message::VerifyText {
                present,
                needle: needle.to_vec(),
            })
            .map_err(map_send_error)?;
        self.status
            .wait_check(4, 5, timeout)
            .ok_or(CheckError::Timeout)
    }
}

pub(crate) struct View {
    tx: SyncSender<Message>,
    child: Option<Child>,
    status: Status,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum SendError {
    QueueFull,
    Closed,
    Oversized,
    InvalidCheck,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum CheckError {
    Send(SendError),
    Timeout,
}

fn map_send_error(error: mpsc::TrySendError<Message>) -> CheckError {
    CheckError::Send(match error {
        mpsc::TrySendError::Full(_) => SendError::QueueFull,
        mpsc::TrySendError::Disconnected(_) => SendError::Closed,
    })
}

impl View {
    pub(crate) fn output(&self, bytes: &[u8]) -> Result<(), SendError> {
        if bytes.len() > MAX_OUTPUT_FRAME {
            return Err(SendError::Oversized);
        }
        self.tx
            .try_send(Message::Output(bytes.to_vec()))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => SendError::QueueFull,
                mpsc::TrySendError::Disconnected(_) => SendError::Closed,
            })
    }

    pub(crate) fn resize(&self, cols: u16, rows: u16) -> Result<(), SendError> {
        self.tx
            .try_send(Message::Resize(cols, rows))
            .map_err(|e| match e {
                mpsc::TrySendError::Full(_) => SendError::QueueFull,
                mpsc::TrySendError::Disconnected(_) => SendError::Closed,
            })
    }

    pub(crate) fn control(&self) -> Control {
        Control {
            tx: self.tx.clone(),
            status: self.status.clone(),
        }
    }

    pub(crate) fn is_alive(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            return false;
        };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) | Err(_) => {
                self.child = None;
                false
            }
        }
    }

    pub(crate) fn pid(&self) -> u32 {
        self.child.as_ref().map(Child::id).unwrap_or(0)
    }

    /// Terminate only the renderer process. The remote PTY and shell are owned by Session.
    pub(crate) fn close_window(&mut self) {
        reap(self.child.take(), true);
    }
}

impl Drop for View {
    fn drop(&mut self) {
        // If the shell exits, leave the final terminal image available for review until the user
        // closes the helper window; wait in the background so this Child is still reaped.
        reap(self.child.take(), false);
    }
}

fn reap(child: Option<Child>, terminate: bool) {
    if let Some(mut child) = child {
        if terminate {
            let _ = child.kill();
        }
        let _ = std::thread::Builder::new()
            .name("uu-term-view-reap".into())
            .spawn(move || {
                let _ = child.wait();
            });
    }
}

pub(crate) fn available() -> bool {
    cfg!(uu_terminal_view)
}

pub(crate) fn spawn(session_id: u32, cols: u16, rows: u16) -> Option<View> {
    #[cfg(test)]
    {
        // Cargo's test executable is not the host binary and does not route the private helper
        // subcommand. Real renderer coverage is provided by the release terminal-acceptance probe.
        let _ = (session_id, cols, rows);
        return None;
    }
    #[cfg(not(uu_terminal_view))]
    {
        let _ = (session_id, cols, rows);
        warn_unavailable("binary was built without GTK/VTE support");
        return None;
    }
    #[cfg(all(not(test), uu_terminal_view))]
    {
        if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
            warn_unavailable("no graphical display is available");
            return None;
        }
        let executable = match std::env::current_exe() {
            Ok(path) => path,
            Err(_) => {
                log::warn!(
                    "Terminal local view unavailable: helper path lookup failed session={}",
                    session_id
                );
                return None;
            }
        };
        let mut child = match Command::new(executable)
            .arg("__terminal-view")
            .arg(session_id.to_string())
            .arg(cols.to_string())
            .arg(rows.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => {
                log::warn!(
                    "Terminal local view unavailable: helper launch failed session={}",
                    session_id
                );
                return None;
            }
        };
        let Some(mut input) = child.stdin.take() else {
            let _ = child.kill();
            log::warn!(
                "Terminal local view unavailable: helper input pipe failed session={}",
                session_id
            );
            return None;
        };
        let Some(mut output) = child.stdout.take() else {
            let _ = child.kill();
            log::warn!(
                "Terminal local view unavailable: helper status pipe failed session={}",
                session_id
            );
            return None;
        };
        let status = Status::new();
        let status_reader = status.clone();
        if std::thread::Builder::new()
            .name(format!("uu-term-status-{}", session_id))
            .spawn(move || {
                let mut byte = [0u8; 1];
                loop {
                    match output.read_exact(&mut byte) {
                        Ok(()) => status_reader.receive(byte[0]),
                        Err(_) => break,
                    }
                }
            })
            .is_err()
        {
            let _ = child.kill();
            log::warn!(
                "Terminal local view unavailable: status reader failed session={}",
                session_id
            );
            return None;
        }

        let (tx, rx) = mpsc::sync_channel(VIEW_QUEUE_MESSAGES);
        if std::thread::Builder::new()
            .name(format!("uu-term-view-{}", session_id))
            .spawn(move || {
                for message in rx {
                    let result = match message {
                        Message::Output(bytes) => {
                            let mut header = [0u8; 5];
                            header[0] = 1;
                            header[1..].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
                            input
                                .write_all(&header)
                                .and_then(|_| input.write_all(&bytes))
                        }
                        Message::Resize(cols, rows) => {
                            let mut frame = [0u8; 5];
                            frame[0] = 2;
                            frame[1..3].copy_from_slice(&cols.to_le_bytes());
                            frame[3..5].copy_from_slice(&rows.to_le_bytes());
                            input.write_all(&frame)
                        }
                        Message::VerifyGrid => input.write_all(&[3]),
                        Message::VerifyText { present, needle } => {
                            let mut header = [0u8; 6];
                            header[0] = 4;
                            header[1] = u8::from(present);
                            header[2..].copy_from_slice(&(needle.len() as u32).to_le_bytes());
                            input
                                .write_all(&header)
                                .and_then(|_| input.write_all(&needle))
                        }
                    };
                    if result.is_err() {
                        break;
                    }
                }
            })
            .is_err()
        {
            let _ = child.kill();
            log::warn!(
                "Terminal local view unavailable: writer thread failed session={}",
                session_id
            );
            return None;
        }
        log::debug!(
            "Terminal local view helper launched: session={}",
            session_id
        );
        Some(View {
            tx,
            child: Some(child),
            status,
        })
    }
}

fn warn_unavailable(reason: &str) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| log::warn!("Terminal local view unavailable: {}", reason));
}

/// Entry point used only by the private `__terminal-view` subcommand in the same executable.
pub fn run_helper(session_id: u32, cols: u16, rows: u16) {
    #[cfg(uu_terminal_view)]
    unsafe {
        uu_terminal_view_run(session_id, cols.max(1), rows.max(1));
    }
    #[cfg(not(uu_terminal_view))]
    {
        let _ = (session_id, cols, rows);
        eprintln!("Terminal local view unavailable: GTK/VTE support was not built");
    }
}

#[cfg(uu_terminal_view)]
extern "C" {
    fn uu_terminal_view_run(session_id: u32, cols: u16, rows: u16);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_frames_have_a_hard_limit() {
        let (tx, _rx) = mpsc::sync_channel(VIEW_QUEUE_MESSAGES);
        let view = View {
            tx,
            child: None,
            status: Status::new(),
        };
        assert!(view.output(&vec![b'x'; MAX_OUTPUT_FRAME]).is_ok());
        assert!(matches!(
            view.output(&vec![b'x'; MAX_OUTPUT_FRAME + 1]),
            Err(SendError::Oversized)
        ));
    }
}
