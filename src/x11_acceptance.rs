use crate::{
    capture::ScreenCapture, clipboard, desktop_backend::DesktopBackend, input::InputInjector,
};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{mpsc, Arc},
    thread,
    time::{Duration, Instant},
};

const IN: &str = "UU X11 incoming 中文🙂\nsecond line";
const OUT: &str = "UU X11 outgoing café 🚀\nsecond line";
const GTK: &str = r#"
import gi, json, sys, threading
gi.require_version("Gtk", "3.0")
from gi.repository import Gdk, GLib, Gtk
s = {"motion": 0, "click": 0, "key": 0}
w = Gtk.Window(title="UU X11 acceptance")
w.set_decorated(False); w.set_default_size(520, 320); w.set_size_request(520, 320)
fixed = Gtk.Fixed(); button = Gtk.Button(label="Synthetic click target")
button.set_size_request(150, 48); entry = Gtk.Entry(); entry.set_size_request(260, 42)
fixed.put(button, 40, 40); fixed.put(entry, 40, 130); w.add(fixed)
clip = Gtk.Clipboard.get(Gdk.SELECTION_CLIPBOARD)
def emit(value):
    sys.stdout.write(json.dumps(value, ensure_ascii=True) + "\n"); sys.stdout.flush()
def motion(*_): s["motion"] += 1; return False
def clicked(*_): s["click"] += 1; entry.grab_focus()
def key(*_): s["key"] += 1; return False
button.add_events(Gdk.EventMask.POINTER_MOTION_MASK); button.connect("motion-notify-event", motion)
button.connect("clicked", clicked); entry.connect("key-press-event", key)
w.connect("destroy", lambda *_: Gtk.main_quit())
def clipboard_ready(_clip, text, expected):
    emit({"matches": (text or "") == expected})
def focus_ready(): emit({"focused": entry.has_focus()}); return False
def handle(r):
    op = r.get("op")
    if op == "snapshot":
        text = entry.get_text()
        emit({"motion": s["motion"], "click": s["click"], "key": s["key"], "entry_is_a": text == "a"})
    elif op == "focus":
        w.present(); entry.grab_focus(); GLib.idle_add(focus_ready)
    elif op == "read": clip.request_text(clipboard_ready, r.get("expected", ""))
    elif op == "set": clip.set_text(r.get("text", ""), -1); emit({"set": True})
    return False
def commands():
    for line in sys.stdin:
        try: GLib.idle_add(handle, json.loads(line))
        except ValueError: pass
w.show_all(); threading.Thread(target=commands, daemon=True).start()
GLib.idle_add(lambda: (emit({"event": "ready"}), False)[1])
Gtk.main()
"#;

struct Fixture {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<String>,
    reader: Option<thread::JoinHandle<()>>,
}
impl Fixture {
    fn start() -> Self {
        let mut child = Command::new("/usr/bin/python3")
            .args(["-u", "-c", GTK])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("start isolated GTK fixture");
        let stdin = child.stdin.take().expect("GTK fixture stdin");
        let stdout = child.stdout.take().expect("GTK fixture stdout");
        let (tx, rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines().flatten() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let f = Self {
            child,
            stdin,
            rx,
            reader: Some(reader),
        };
        let ready = f.line(Duration::from_secs(8));
        assert_eq!(ready["event"], "ready", "GTK fixture did not become ready");
        f
    }
    fn line(&self, wait: Duration) -> Value {
        serde_json::from_str(&self.rx.recv_timeout(wait).expect("GTK fixture timed out"))
            .expect("GTK fixture response was not JSON")
    }
    fn request(&mut self, op: &str, fields: Value, wait: Duration) -> Value {
        let mut fields = fields.as_object().cloned().unwrap_or_default();
        fields.insert("op".into(), json!(op));
        writeln!(self.stdin, "{}", Value::Object(fields)).expect("write GTK fixture request");
        self.stdin.flush().expect("flush GTK fixture request");
        self.line(wait)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

struct InputCleanup(Arc<InputInjector>);
impl Drop for InputCleanup {
    fn drop(&mut self) {
        self.0.release_held();
    }
}

fn xvfb_owns(display: &str) -> bool {
    let Some(number) = display.strip_prefix(':').and_then(|s| s.split('.').next()) else {
        return false;
    };
    let wanted = format!(":{number}");
    std::fs::read_dir("/proc")
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .any(|p| {
            let cmd = std::fs::read(p.path().join("cmdline")).unwrap_or_default();
            let mut args = cmd.split(|b| *b == 0).filter(|a| !a.is_empty());
            args.next().is_some_and(|a| {
                (a == b"Xvfb" || a.ends_with(b"/Xvfb")) && args.any(|arg| arg == wanted.as_bytes())
            })
        })
}

fn xdotool(args: &[String]) -> String {
    let out = Command::new("timeout")
        .arg("5s")
        .arg("xdotool")
        .args(args)
        .output()
        .expect("run bounded xdotool command");
    assert!(out.status.success(), "xdotool fixture operation failed");
    String::from_utf8_lossy(&out.stdout).into_owned()
}
fn xdt(op: &str, window: &str, args: &[&str]) {
    let mut command = vec![op.into(), window.into()];
    command.extend(args.iter().map(|arg| (*arg).into()));
    xdotool(&command);
}
fn find_window(pid: u32) -> String {
    let mut args: Vec<_> = ["search", "--sync", "--onlyvisible", "--pid"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    args.push(pid.to_string());
    xdotool(&args)
        .lines()
        .next()
        .expect("xdotool found no fixture window")
        .trim()
        .into()
}
fn window_position(id: &str) -> (i32, i32) {
    let out = xdotool(&["getwindowgeometry".into(), "--shell".into(), id.into()]);
    let coord = |key| {
        out.lines()
            .find_map(|line| line.strip_prefix(&format!("{key}=")))
            .and_then(|n| n.parse().ok())
            .expect("xdotool omitted fixture position")
    };
    (coord("X"), coord("Y"))
}
fn clipboard_arrived(f: &mut Fixture, text: &str, limit: Duration) -> bool {
    let until = Instant::now() + limit;
    while Instant::now() < until {
        let left = until.saturating_duration_since(Instant::now());
        if f.request(
            "read",
            json!({"expected": text}),
            left.min(Duration::from_millis(700)),
        )["matches"]
            == true
        {
            return true;
        }
        thread::sleep(Duration::from_millis(80));
    }
    false
}

#[test]
#[ignore = "requires UU_X11_ACCEPTANCE=1 and a dedicated Xvfb display"]
fn actual_x11_input_and_clipboard() {
    assert!(
        std::env::var("UU_X11_ACCEPTANCE").ok().as_deref() == Some("1")
            && std::env::var("UU_DESKTOP_BACKEND").ok().as_deref() == Some("x11"),
        "require UU_X11_ACCEPTANCE=1 and UU_DESKTOP_BACKEND=x11"
    );
    let display = std::env::var("DISPLAY").expect("DISPLAY must point to Xvfb");
    assert!(xvfb_owns(&display), "refusing to inject input outside Xvfb");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("create test runtime");
    let (screen, mut bridge) = runtime.block_on(async {
        let screen = ScreenCapture::new().expect("open Xvfb display");
        assert_eq!(screen.backend, DesktopBackend::X11);
        assert!(
            screen.width >= 640 && screen.height >= 400,
            "Xvfb screen too small"
        );
        let mut bridge = clipboard::start(screen.display.clone());
        let ready = tokio::time::timeout(Duration::from_secs(5), &mut bridge.ready)
            .await
            .expect("clipboard startup timed out")
            .expect("clipboard startup channel closed");
        assert!(ready, "X11 clipboard could not connect");
        (screen, bridge)
    });

    let input = Arc::new(InputInjector::new());
    let _cleanup = InputCleanup(input.clone());
    let mut fixture = Fixture::start();
    let window = find_window(fixture.child.id());
    xdt("windowmove", &window, &["20", "20"]);
    xdt("windowraise", &window, &[]);
    xdt("windowfocus", &window, &[]);
    let (x, y) = window_position(&window);
    assert!(x >= 0 && y >= 0, "fixture window outside Xvfb");

    let move_to = |x: u32, y: u32| {
        input.mouse_absolute_normalized(
            f64::from(x) / f64::from(screen.width),
            f64::from(y) / f64::from(screen.height),
            (screen.width, screen.height),
        );
        input.flush_pointer();
    };
    move_to((x + 115) as u32, (y + 64) as u32);
    input.mouse_button(1, true);
    input.mouse_button(1, false);
    assert_eq!(
        fixture.request("focus", json!({}), Duration::from_secs(3))["focused"],
        true,
        "GTK entry did not receive focus"
    );
    input.windows_key_event(0x41, true);
    input.windows_key_event(0x41, false);

    let state = fixture.request("snapshot", json!({}), Duration::from_secs(3));
    for key in ["motion", "click", "key"] {
        assert!(
            state[key].as_u64().unwrap_or(0) > 0,
            "GTK fixture missed synthetic {key}"
        );
    }
    assert_eq!(state["entry_is_a"], true, "GTK entry missed synthetic key");

    bridge.incoming.send_replace(Some(IN.into()));
    assert!(
        clipboard_arrived(&mut fixture, IN, Duration::from_secs(5)),
        "GTK missed Unicode clipboard input"
    );
    let _ = bridge.outgoing.borrow_and_update();
    assert_eq!(
        fixture.request("set", json!({"text": OUT}), Duration::from_secs(3))["set"],
        true
    );
    let until = Instant::now() + Duration::from_secs(5);
    let mut copied = false;
    while Instant::now() < until {
        if bridge.outgoing.has_changed().unwrap_or(false)
            && bridge.outgoing.borrow_and_update().as_deref() == Some(OUT)
        {
            copied = true;
            break;
        }
        thread::sleep(Duration::from_millis(40));
    }
    assert!(copied, "X11 clipboard missed Unicode fixture text");
    println!("X11 acceptance passed: motion={} clicks={} keys={} clipboard_in_bytes={} clipboard_out_bytes={}",
        state["motion"].as_u64().unwrap_or(0), state["click"].as_u64().unwrap_or(0),
        state["key"].as_u64().unwrap_or(0), IN.len(), OUT.len());
}
