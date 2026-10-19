//! Bounded local terminal acceptance through TerminalConn and the real PTY/VTE path.
use super::*;

fn has_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

fn wait_for_output(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    session: u32,
    needles: &[&[u8]],
    timeout: std::time::Duration,
) -> Result<Vec<u8>, String> {
    let deadline = std::time::Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        while let Ok(packet) = rx.try_recv() {
            let Some(frame) = decode(&packet) else {
                return Err("invalid terminal response frame".into());
            };
            if frame.session != session {
                continue;
            }
            if frame.kind == EXIT {
                return Err("acceptance shell exited early".into());
            }
            if frame.kind == STDOUT {
                seen.extend_from_slice(&frame.payload);
            }
            if needles.iter().all(|needle| has_bytes(&seen, needle)) {
                return Ok(seen);
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err("timed out waiting for synthetic terminal output".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn wait_for_response(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    session: u32,
    kind: u8,
    timeout: std::time::Duration,
) -> Result<Frame, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        while let Ok(packet) = rx.try_recv() {
            let Some(frame) = decode(&packet) else {
                return Err("invalid terminal response frame".into());
            };
            if (session == 0 || frame.session == session) && frame.kind == kind {
                return Ok(frame);
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err("timed out waiting for terminal response".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
fn wait_for_view(
    session: &Session,
    timeout: std::time::Duration,
) -> Result<(u32, terminal_view::Control), String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let active = {
            let mut slot = session.view.lock().unwrap();
            slot.as_mut()
                .and_then(|view| view.is_alive().then(|| (view.pid(), view.control())))
        };
        if let Some((pid, control)) = active {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .unwrap_or_default();
            return match control.status().wait_ready(remaining) {
                Some(true) => Ok((pid, control)),
                Some(false) => Err("local VTE helper could not map its window".into()),
                None => Err("local VTE window did not report ready before timeout".into()),
            };
        }
        if std::time::Instant::now() >= deadline {
            return Err("local VTE helper did not start".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

fn wait_for_scrollback(
    session: &Session,
    needle: &[u8],
    timeout: std::time::Duration,
) -> Result<Vec<u8>, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let contents: Vec<u8> = session.scrollback.lock().unwrap().iter().copied().collect();
        if has_bytes(&contents, needle) {
            return Ok(contents);
        }
        if std::time::Instant::now() >= deadline {
            return Err("timed out waiting for detached PTY output".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || needle.len() > haystack.len() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

fn wait_for_pty_echo(
    session: &Session,
    enabled: bool,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let fd = std::os::fd::AsRawFd::as_raw_fd(&session.master);
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let mut attrs = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut attrs) } == 0
            && (attrs.c_lflag & libc::ECHO != 0) == enabled
        {
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err("PTY echo mode did not reach the expected state".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn drain_stdout(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    session: u32,
) -> Result<Vec<u8>, String> {
    let mut output = Vec::new();
    while let Ok(packet) = rx.try_recv() {
        let Some(frame) = decode(&packet) else {
            return Err("invalid terminal response frame".into());
        };
        if frame.session == session {
            if frame.kind == EXIT {
                return Err("acceptance shell exited early".into());
            }
            if frame.kind == STDOUT {
                output.extend(frame.payload);
            }
        }
    }
    Ok(output)
}

fn wait_for_exit(
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    session: u32,
    timeout: std::time::Duration,
) -> Result<i64, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        while let Ok(packet) = rx.try_recv() {
            let Some(frame) = decode(&packet) else {
                return Err("invalid terminal response frame".into());
            };
            if frame.session == session && frame.kind == EXIT {
                return Ok(json(&frame.payload)
                    .get("exit_code")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(-1));
            }
        }
        if std::time::Instant::now() >= deadline {
            return Err("timed out waiting for terminal exit".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn list_sessions(
    conn: &TerminalConn,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
) -> Result<Vec<serde_json::Value>, String> {
    const REQUEST_ID: u32 = u32::MAX - 7;
    conn.handle(&encode(LIST_REQ, REQUEST_ID, &[]));
    let response = wait_for_response(rx, REQUEST_ID, LIST_RESP, std::time::Duration::from_secs(2))?;
    json(&response.payload)
        .as_array()
        .cloned()
        .ok_or_else(|| "terminal list response was not an array".into())
}

fn wait_for_list_presence(
    conn: &TerminalConn,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    session: u32,
    present: bool,
    timeout: std::time::Duration,
) -> Result<Option<serde_json::Value>, String> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let sessions = list_sessions(conn, rx)?;
        let entry = sessions
            .into_iter()
            .find(|item| item.get("session_id").and_then(|v| v.as_u64()) == Some(session as u64));
        if entry.is_some() == present {
            return Ok(entry);
        }
        if std::time::Instant::now() >= deadline {
            return Err(if present {
                "terminal session disappeared from LIST"
            } else {
                "terminated session remained in LIST"
            }
            .into());
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
}

fn open_probe_session(
    conn: &TerminalConn,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    shell: &str,
) -> Result<Arc<Session>, String> {
    let payload = serde_json::json!({"cols": 80, "rows": 24, "shell": shell}).to_string();
    conn.handle(&encode(OPEN, 0, payload.as_bytes()));
    let opened = wait_for_response(rx, 0, OPEN_RESP, std::time::Duration::from_secs(5))?;
    if json(&opened.payload).get("code").and_then(|v| v.as_i64()) != Some(0) {
        return Err("acceptance session could not be opened".into());
    }
    registry()
        .lock()
        .unwrap()
        .get(&opened.session)
        .cloned()
        .ok_or_else(|| "acceptance session missing from registry".into())
}

fn close_view(session: &Session) {
    if let Some(mut view) = session.view.lock().unwrap().take() {
        view.close_window();
    }
}

/// Exercise the real TerminalConn, PTY reader, bounded scrollback, GTK/VTE view and reconnection
/// paths using synthetic shell input. It leaves one short, readable result screen open for review.
pub fn acceptance() -> Result<String, String> {
    if !terminal_view::available() {
        return Err("binary was built without GTK/VTE support".into());
    }
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Err("acceptance requires an available graphical display".into());
    }

    let probes = Arc::new(Mutex::new(Vec::<Arc<Session>>::new()));
    struct Cleanup {
        probes: Arc<Mutex<Vec<Arc<Session>>>>,
        keep_view: u32,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            for session in self.probes.lock().unwrap().iter() {
                let registered = registry()
                    .lock()
                    .unwrap()
                    .get(&session.id)
                    .is_some_and(|current| Arc::ptr_eq(current, session));
                if registered {
                    session.signal(libc::SIGHUP);
                    session.signal(libc::SIGKILL);
                }
                if session.id != self.keep_view {
                    close_view(session);
                }
            }
        }
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let conn = TerminalConn::new(tx);
    let session = open_probe_session(&conn, &mut rx, "/bin/bash")?;
    let id = session.id;
    probes.lock().unwrap().push(session.clone());
    let _cleanup = Cleanup {
        probes: probes.clone(),
        keep_view: id,
    };
    let (first_view_pid, first_view) = wait_for_view(&session, std::time::Duration::from_secs(8))?;
    let shell_pid = session.pid;

    let first_command = concat!(
        "cd /tmp || exit; export PS1='uu-test$ '; export UU_TERMINAL_ACCEPTANCE=live; ",
        "printf '\\033[2J\\033[H\\033[1;32mUU_TERM_VIEW_中文_MAIN_OK\\033[0m\\r\\n'; ",
        "a=UU_TERM_VIEW; b=_STATE; c=_OK; printf '%s%s%s pid=%s cwd=%s env=%s\\n' \"$a\" \"$b\" \"$c\" \"$$\" \"$PWD\" \"$UU_TERMINAL_ACCEPTANCE\"; ",
        "x=UU_TERM_VIEW; y=_STDERR; z=_OK; printf '%s%s%s\\n' \"$x\" \"$y\" \"$z\" >&2"
    );
    conn.handle(&encode(
        STDIN,
        id,
        format!("{}\n", first_command).as_bytes(),
    ));
    let first = wait_for_output(
        &mut rx,
        id,
        &[
            "UU_TERM_VIEW_中文_MAIN_OK".as_bytes(),
            b"UU_TERM_VIEW_STATE_OK",
            b"UU_TERM_VIEW_STDERR_OK",
        ],
        std::time::Duration::from_secs(8),
    )?;
    if !has_bytes(&first, format!("pid={}", shell_pid).as_bytes())
        || !has_bytes(&first, b"cwd=/tmp")
        || !has_bytes(&first, b"env=live")
    {
        return Err("acceptance output did not preserve the shell state".into());
    }
    if !first_view
        .verify_text(
            "UU_TERM_VIEW_中文_MAIN_OK".as_bytes(),
            true,
            std::time::Duration::from_secs(5),
        )
        .map_err(|_| "VTE text rendering check timed out".to_string())?
    {
        return Err("VTE did not render the Chinese ANSI marker".into());
    }

    // The expected line is assembled from separate shell variables, so it cannot be mistaken
    // for the echoed command text. Check every line exactly once and in order on both paths.
    let sequence_command = "a=UU_TERM; b=_SEQ; c=_ACCEPT; for i in {001..120}; do printf '%s%s%s_%s\\n' \"$a\" \"$b\" \"$c\" \"$i\"; done";
    conn.handle(&encode(
        STDIN,
        id,
        format!("{}\n", sequence_command).as_bytes(),
    ));
    let sequence_needles: Vec<Vec<u8>> = (1..=120)
        .map(|n| format!("UU_TERM_SEQ_ACCEPT_{:03}\r\n", n).into_bytes())
        .collect();
    let sequence_refs: Vec<&[u8]> = sequence_needles.iter().map(Vec::as_slice).collect();
    let sequence_output = wait_for_output(
        &mut rx,
        id,
        &sequence_refs,
        std::time::Duration::from_secs(8),
    )?;
    let mut previous = None;
    for line in &sequence_needles {
        if count_bytes(&sequence_output, line) != 1 {
            return Err("synthetic terminal lines were missing or duplicated".into());
        }
        let position = sequence_output
            .windows(line.len())
            .position(|window| window == line)
            .ok_or("synthetic terminal line was missing")?;
        if previous.is_some_and(|prior| position <= prior) {
            return Err("synthetic terminal lines arrived out of order".into());
        }
        previous = Some(position);
    }
    let final_sequence_line = sequence_needles[119]
        .strip_suffix(b"\r\n")
        .ok_or("invalid final synthetic line")?;
    if !first_view
        .verify_text(final_sequence_line, true, std::time::Duration::from_secs(5))
        .map_err(|_| "VTE sequence text check timed out".to_string())?
    {
        return Err("VTE did not render the final synthetic line".into());
    }

    conn.handle(&encode(RESIZE, id, br#"{"cols":100,"rows":30}"#));
    if !first_view
        .verify_grid(std::time::Duration::from_secs(5))
        .map_err(|_| "VTE resize check timed out".to_string())?
    {
        return Err("local window resize changed the remote VTE grid".into());
    }
    conn.handle(&encode(RESIZE, id, br#"{"cols":80,"rows":24}"#));
    if !first_view
        .verify_grid(std::time::Duration::from_secs(5))
        .map_err(|_| "VTE restore check timed out".to_string())?
    {
        return Err("VTE grid did not follow the remote resize".into());
    }

    conn.handle(&encode(DETACH, id, &[]));
    let detached_command = concat!(
        "a=UU_TERM_VIEW; b=_DETACHED; c=_OK; ",
        "printf '%s%s%s pid=%s cwd=%s env=%s\\n' \"$a\" \"$b\" \"$c\" \"$$\" \"$PWD\" \"$UU_TERMINAL_ACCEPTANCE\""
    );
    conn.handle(&encode(
        STDIN,
        id,
        format!("{}\n", detached_command).as_bytes(),
    ));
    let detached_marker = format!(
        "UU_TERM_VIEW_DETACHED_OK pid={} cwd=/tmp env=live",
        shell_pid
    );
    let detached_text = wait_for_scrollback(
        &session,
        detached_marker.as_bytes(),
        std::time::Duration::from_secs(5),
    )?;
    if count_bytes(&detached_text, detached_marker.as_bytes()) != 1 {
        return Err("detached output was duplicated in scrollback".into());
    }
    conn.handle(&encode(OPEN, id, br#"{"cols":80,"rows":24}"#));
    let reattached = wait_for_response(&mut rx, id, OPEN_RESP, std::time::Duration::from_secs(5))?;
    if json(&reattached.payload)
        .get("code")
        .and_then(|v| v.as_i64())
        != Some(0)
    {
        return Err("session reattach failed".into());
    }
    let replay = wait_for_output(
        &mut rx,
        id,
        &[detached_marker.as_bytes()],
        std::time::Duration::from_secs(8),
    )?;
    std::thread::sleep(std::time::Duration::from_millis(100));
    let mut replay_all = replay;
    replay_all.extend(drain_stdout(&mut rx, id)?);
    if count_bytes(&replay_all, detached_marker.as_bytes()) != 1 {
        return Err("reattach replay duplicated detached output".into());
    }
    let current = registry()
        .lock()
        .unwrap()
        .get(&id)
        .cloned()
        .ok_or("session disappeared during reattach")?;
    if !Arc::ptr_eq(&session, &current) || current.pid != shell_pid {
        return Err("reattach replaced the existing shell".into());
    }
    if wait_for_view(&session, std::time::Duration::from_secs(5))?.0 != first_view_pid {
        return Err("reattach replaced an open local window".into());
    }

    close_view(&session);
    session.ensure_view();
    let (reopened_view_pid, reopened_view) =
        wait_for_view(&session, std::time::Duration::from_secs(8))?;
    if reopened_view_pid == first_view_pid {
        return Err("closing the view did not create a fresh renderer".into());
    }
    let reopened_command = "a=UU_TERM_VIEW; b=_REOPENED; c=_OK; printf '%s%s%s pid=%s cwd=%s env=%s\\n' \"$a\" \"$b\" \"$c\" \"$$\" \"$PWD\" \"$UU_TERMINAL_ACCEPTANCE\"";
    conn.handle(&encode(
        STDIN,
        id,
        format!("{}\n", reopened_command).as_bytes(),
    ));
    let reopened_marker = format!(
        "UU_TERM_VIEW_REOPENED_OK pid={} cwd=/tmp env=live",
        shell_pid
    );
    let _ = wait_for_output(
        &mut rx,
        id,
        &[reopened_marker.as_bytes()],
        std::time::Duration::from_secs(8),
    )?;
    if !reopened_view
        .verify_text(
            reopened_marker.as_bytes(),
            true,
            std::time::Duration::from_secs(5),
        )
        .map_err(|_| "reopened VTE text check timed out".to_string())?
    {
        return Err("reopened view did not replay shell output".into());
    }

    let noecho_command = concat!(
        "a=UU_TERM_VIEW; b=_NOECHO; c=_READY; printf '%s%s%s\\n' \"$a\" \"$b\" \"$c\"; ",
        "IFS= read -r -s secret; printf '\\n'; ",
        "a=UU_TERM_VIEW; b=_NOECHO; c=_OK; printf '%s%s%s\\n' \"$a\" \"$b\" \"$c\"; unset secret"
    );
    conn.handle(&encode(
        STDIN,
        id,
        format!("{}\n", noecho_command).as_bytes(),
    ));
    let _ = wait_for_output(
        &mut rx,
        id,
        &[b"UU_TERM_VIEW_NOECHO_READY"],
        std::time::Duration::from_secs(5),
    )?;
    wait_for_pty_echo(&session, false, std::time::Duration::from_secs(2))?;
    let hidden_input: &[u8] = b"UU_TERM_VIEW_HIDDEN_SENTINEL_7C91";
    conn.handle(&encode(STDIN, id, &[hidden_input, b"\n"].concat()));
    let noecho = wait_for_output(
        &mut rx,
        id,
        &[b"UU_TERM_VIEW_NOECHO_OK"],
        std::time::Duration::from_secs(5),
    )?;
    if has_bytes(&noecho, hidden_input) {
        return Err("closed-echo input appeared in terminal output".into());
    }
    if !reopened_view
        .verify_text(hidden_input, false, std::time::Duration::from_secs(5))
        .map_err(|_| "VTE no-echo text check timed out".to_string())?
    {
        return Err("VTE displayed input from a no-echo prompt".into());
    }

    conn.handle(&encode(STDIN, id, b"sleep 15\n"));
    std::thread::sleep(std::time::Duration::from_millis(150));
    conn.handle(&encode(STDIN, id, b"\x03"));
    let ctrl_c_command =
        "a=UU_TERM_VIEW; b=_CTRL_C; c=_OK; printf '%s%s%s\\n' \"$a\" \"$b\" \"$c\"";
    conn.handle(&encode(
        STDIN,
        id,
        format!("{}\n", ctrl_c_command).as_bytes(),
    ));
    let _ = wait_for_output(
        &mut rx,
        id,
        &[b"UU_TERM_VIEW_CTRL_C_OK"],
        std::time::Duration::from_secs(5),
    )?;

    // A second real PTY must have independent process state and an independent VTE buffer.
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    let conn2 = TerminalConn::new(tx2);
    let session2 = open_probe_session(&conn2, &mut rx2, "/bin/bash")?;
    let id2 = session2.id;
    let shell_pid2 = session2.pid;
    probes.lock().unwrap().push(session2.clone());
    if id2 == id || shell_pid2 == shell_pid {
        return Err("separate terminal sessions shared an identity".into());
    }
    let (_, view2) = wait_for_view(&session2, std::time::Duration::from_secs(8))?;
    let second_command = concat!(
        "cd /var/tmp || exit; export UU_TERMINAL_ACCEPTANCE=two; ",
        "printf '\\033[2J\\033[H\\033[1;36mUU_TERM_VIEW_SESSION_TWO_OK\\033[0m\\r\\n'; ",
        "a=UU_TERM_VIEW; b=_TWO; c=_STATE; printf '%s%s%s pid=%s cwd=%s env=%s\\n' \"$a\" \"$b\" \"$c\" \"$$\" \"$PWD\" \"$UU_TERMINAL_ACCEPTANCE\""
    );
    conn2.handle(&encode(
        STDIN,
        id2,
        format!("{}\n", second_command).as_bytes(),
    ));
    let second = wait_for_output(
        &mut rx2,
        id2,
        &[b"UU_TERM_VIEW_SESSION_TWO_OK", b"UU_TERM_VIEW_TWO_STATE"],
        std::time::Duration::from_secs(8),
    )?;
    if !has_bytes(&second, format!("pid={}", shell_pid2).as_bytes())
        || !has_bytes(&second, b"cwd=/var/tmp")
        || !has_bytes(&second, b"env=two")
        || has_bytes(&second, format!("pid={}", shell_pid).as_bytes())
        || has_bytes(&second, b"env=live")
    {
        return Err("second session inherited output or shell state from the first".into());
    }
    if !view2
        .verify_text(
            b"UU_TERM_VIEW_SESSION_TWO_OK",
            true,
            std::time::Duration::from_secs(5),
        )
        .map_err(|_| "second VTE text check timed out".to_string())?
    {
        return Err("second VTE did not render its own marker".into());
    }
    if !view2
        .verify_text(
            "UU_TERM_VIEW_中文_MAIN_OK".as_bytes(),
            false,
            std::time::Duration::from_secs(5),
        )
        .map_err(|_| "second VTE isolation check timed out".to_string())?
    {
        return Err("second VTE contained the first session's output".into());
    }
    if !reopened_view
        .verify_text(
            b"UU_TERM_VIEW_SESSION_TWO_OK",
            false,
            std::time::Duration::from_secs(5),
        )
        .map_err(|_| "first VTE isolation check timed out".to_string())?
    {
        return Err("first VTE contained the second session's output".into());
    }
    conn2.handle(&encode(STDIN, id2, b"exit 23\n"));
    if wait_for_exit(&mut rx2, id2, std::time::Duration::from_secs(5))? != 23 {
        return Err("explicit shell exit code changed".into());
    }
    let _ = wait_for_list_presence(
        &conn2,
        &mut rx2,
        id2,
        false,
        std::time::Duration::from_secs(5),
    )?;
    close_view(&session2);

    // Drop both ends of the original connection channel while a harmless command is still running.
    let disconnect_command = concat!(
        "sleep 0.25; a=UU_TERM_VIEW; b=_DISCONNECT; c=_OK; ",
        "printf '%s%s%s pid=%s cwd=%s env=%s\\n' \"$a\" \"$b\" \"$c\" \"$$\" \"$PWD\" \"$UU_TERMINAL_ACCEPTANCE\""
    );
    conn.handle(&encode(
        STDIN,
        id,
        format!("{}\n", disconnect_command).as_bytes(),
    ));
    drop(conn);
    drop(rx);
    let disconnected_marker = format!(
        "UU_TERM_VIEW_DISCONNECT_OK pid={} cwd=/tmp env=live",
        shell_pid
    );
    let disconnected_text = wait_for_scrollback(
        &session,
        disconnected_marker.as_bytes(),
        std::time::Duration::from_secs(8),
    )?;
    if count_bytes(&disconnected_text, disconnected_marker.as_bytes()) != 1 {
        return Err("disconnected PTY output was duplicated".into());
    }
    if !registry()
        .lock()
        .unwrap()
        .get(&id)
        .is_some_and(|current| Arc::ptr_eq(current, &session) && current.pid == shell_pid)
    {
        return Err("dropping the connection terminated or replaced the shell".into());
    }

    let (tx3, mut rx3) = tokio::sync::mpsc::unbounded_channel();
    let conn3 = TerminalConn::new(tx3);
    let state = wait_for_list_presence(
        &conn3,
        &mut rx3,
        id,
        true,
        std::time::Duration::from_secs(3),
    )?
    .and_then(|entry| {
        entry
            .get("state")
            .and_then(|v| v.as_str())
            .map(str::to_owned)
    });
    if state.as_deref() != Some("detached") {
        return Err("disconnected session was not listed as detached".into());
    }
    conn3.handle(&encode(OPEN, id, br#"{"cols":80,"rows":24}"#));
    let reconnected =
        wait_for_response(&mut rx3, id, OPEN_RESP, std::time::Duration::from_secs(5))?;
    if json(&reconnected.payload)
        .get("code")
        .and_then(|v| v.as_i64())
        != Some(0)
    {
        return Err("connection-drop reattach failed".into());
    }
    let replay = wait_for_output(
        &mut rx3,
        id,
        &[disconnected_marker.as_bytes()],
        std::time::Duration::from_secs(8),
    )?;
    std::thread::sleep(std::time::Duration::from_millis(100));
    let mut replay_all = replay;
    replay_all.extend(drain_stdout(&mut rx3, id)?);
    if count_bytes(&replay_all, disconnected_marker.as_bytes()) != 1 {
        return Err("connection-drop replay duplicated output".into());
    }
    if wait_for_view(&session, std::time::Duration::from_secs(5))?.0 != reopened_view_pid {
        return Err("connection-drop reattach changed the local renderer".into());
    }

    // Verify that a normal CLOSE ends its PTY and no longer appears as live in LIST.
    let (tx4, mut rx4) = tokio::sync::mpsc::unbounded_channel();
    let conn4 = TerminalConn::new(tx4);
    let session4 = open_probe_session(&conn4, &mut rx4, "/bin/bash")?;
    let id4 = session4.id;
    probes.lock().unwrap().push(session4.clone());
    close_view(&session4);
    conn4.handle(&encode(CLOSE, id4, &[]));
    let _ = wait_for_list_presence(
        &conn4,
        &mut rx4,
        id4,
        false,
        std::time::Duration::from_secs(5),
    )?;

    // KILL acknowledges immediately; acceptance waits for the reader to reap and remove the session.
    let (tx5, mut rx5) = tokio::sync::mpsc::unbounded_channel();
    let conn5 = TerminalConn::new(tx5);
    let session5 = open_probe_session(&conn5, &mut rx5, "/bin/bash")?;
    let id5 = session5.id;
    probes.lock().unwrap().push(session5.clone());
    close_view(&session5);
    conn5.handle(&encode(KILL_REQ, id5, &[]));
    let killed = wait_for_response(&mut rx5, id5, KILL_RESP, std::time::Duration::from_secs(3))?;
    if json(&killed.payload).get("code").and_then(|v| v.as_i64()) != Some(0) {
        return Err("KILL was not acknowledged".into());
    }
    let _ = wait_for_list_presence(
        &conn5,
        &mut rx5,
        id5,
        false,
        std::time::Duration::from_secs(5),
    )?;

    let final_screen = concat!(
        "printf '\\033[2J\\033[H\\033[1;32m终端视图验收通过\\033[0m\\r\\n",
        "中文显示 / ANSI重绘 / 双会话隔离\\r\\n",
        "断线与窗口恢复 / 远端尺寸稳定\\r\\n'"
    );
    conn3.handle(&encode(STDIN, id, format!("{}\n", final_screen).as_bytes()));
    let final_marker = "终端视图验收通过".as_bytes();
    let _ = wait_for_output(
        &mut rx3,
        id,
        &[final_marker],
        std::time::Duration::from_secs(5),
    )?;
    let (_, final_view) = wait_for_view(&session, std::time::Duration::from_secs(5))?;
    if !final_view
        .verify_text(final_marker, true, std::time::Duration::from_secs(5))
        .map_err(|_| "final VTE text check timed out".to_string())?
    {
        return Err("final Chinese result screen did not render".into());
    }

    conn3.handle(&encode(KILL_REQ, id, &[]));
    let killed = wait_for_response(&mut rx3, id, KILL_RESP, std::time::Duration::from_secs(3))?;
    if json(&killed.payload).get("code").and_then(|v| v.as_i64()) != Some(0) {
        return Err("primary KILL was not acknowledged".into());
    }
    let _ = wait_for_list_presence(
        &conn3,
        &mut rx3,
        id,
        false,
        std::time::Duration::from_secs(5),
    )?;
    Ok(format!("session={} shell_pid={} mapped=true chinese_ansi=true grid_locked=true detach_reattach=true channel_drop=true view_reopen=true isolation=true noecho=true close_exit_kill=true", id, shell_pid))
}
