#!/usr/bin/env python3
"""Exercise virtual-output activation without replacing a live Xorg desktop session."""

import json
import os
from pathlib import Path
import pwd
import re
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import zlib


MODE = "1024x768_60.00"
WINDOW_TITLE = "UU Same Session Display Acceptance"
DISPLAY_SIZE = "1024x768"


def require(ok, message):
    if not ok:
        raise RuntimeError(message)


def acceptance_identity():
    if os.geteuid() == 0:
        user = pwd.getpwnam("nobody")
        return user.pw_uid, user.pw_gid, user.pw_name, user.pw_dir, user.pw_shell
    return os.getuid(), os.getgid(), os.environ.get("USER", ""), os.path.expanduser("~"), "/bin/sh"


def demote(uid, gid):
    if os.geteuid() == 0:
        os.setgid(gid)
        os.setuid(uid)


def chown_tree(root, uid, gid):
    if os.geteuid() != 0:
        return
    for current, directories, files in os.walk(str(root)):
        os.chown(current, uid, gid)
        for name in directories + files:
            os.chown(os.path.join(current, name), uid, gid)


def runner_env(base, display, authority):
    env = {
        "PATH": os.environ.get("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"),
        "HOME": str(base / "home"),
        "USER": base.name,
        "LOGNAME": base.name,
        "SHELL": "/bin/sh",
        "DISPLAY": display,
        "XAUTHORITY": str(authority),
        "XDG_SESSION_TYPE": "x11",
        "XDG_CURRENT_DESKTOP": "GNOME-Flashback:GNOME",
        "XDG_RUNTIME_DIR": str(base / "runtime"),
        "XDG_CONFIG_HOME": str(base / "config"),
        "XDG_CACHE_HOME": str(base / "cache"),
        "XDG_DATA_HOME": str(base / "data"),
        "GDK_BACKEND": "x11",
        "LC_ALL": "C.UTF-8",
        "LANG": "C.UTF-8",
        "UU_CONFIG": str(base / "no-config.json"),
        "UU_DESKTOP_BACKEND": "x11",
        "UU_VIRTUAL_SIZE": DISPLAY_SIZE,
        "UU_LOGIN_WINDOW": "0",
        "UU_REMOTE_ASSIST": "0",
        "UU_AUDIO": "0",
        "UU_VIDEO_ENCODER": "cpu",
        "UU_VIDEO_CODEC": "h264",
        "RUST_LOG": "warn",
        "NO_AT_BRIDGE": "1",
    }
    return env


def popen(command, env, uid, gid, stdout, stderr=None, run_as_root=False, **kwargs):
    return subprocess.Popen(
        command,
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=stdout,
        stderr=stderr if stderr is not None else subprocess.STDOUT,
        preexec_fn=None if run_as_root else (lambda: demote(uid, gid)),
        start_new_session=True,
        **kwargs
    )


def command_output(command, env, uid, gid, timeout=10):
    process = popen(command, env, uid, gid, subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        output, error = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        stop_process(process)
        raise RuntimeError("command timed out: " + " ".join(command))
    if process.returncode:
        detail = error.decode("utf-8", errors="replace")[-1200:]
        raise RuntimeError("command failed: {}\n{}".format(" ".join(command), detail))
    return output.decode("utf-8", errors="replace")


def stop_process(process, timeout=3):
    if process is None or process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        return
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


def cvt_modeline():
    output = subprocess.check_output(["cvt", "1024", "768", "60"], stderr=subprocess.STDOUT)
    line = next((line.strip() for line in output.decode("ascii", errors="replace").splitlines()
                 if line.strip().startswith("Modeline ")), None)
    if line is None:
        raise RuntimeError("cvt did not return a modeline for the Xorg fixture")
    parts = shlex.split(line)
    require(len(parts) >= 12, "cvt returned an incomplete modeline")
    return parts[1], parts[2:]


def make_xorg_config(path):
    label, timing = cvt_modeline()
    config = """Section "Monitor"
    Identifier "AcceptanceMonitor"
    HorizSync 5.0 - 1000.0
    VertRefresh 5.0 - 200.0
    Modeline "{label}" {timing}
EndSection

Section "Device"
    Identifier "AcceptanceDummy"
    Driver "dummy"
    VideoRam 256000
EndSection

Section "Screen"
    Identifier "AcceptanceScreen"
    Device "AcceptanceDummy"
    Monitor "AcceptanceMonitor"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
        Virtual 1024 768
        Modes "{label}"
    EndSubSection
EndSection
""".format(label=label, timing=" ".join(timing))
    path.write_text(config, encoding="ascii")
    return label


def display_candidates():
    for number in range(100, 180):
        if not Path("/tmp/.X{}-lock".format(number)).exists() and not Path("/tmp/.X11-unix/X{}".format(number)).exists():
            yield ":{}".format(number)


def root_window_id(env, uid, gid):
    output = command_output(["xwininfo", "-root"], env, uid, gid)
    match = re.search(r"Window id:\s+(0x[0-9a-fA-F]+)", output)
    if match is None:
        raise RuntimeError("xwininfo did not report the Xorg root window")
    return match.group(1)


def xrandr_query(env, uid, gid):
    return command_output(["xrandr", "--query"], env, uid, gid)


def output_header(snapshot, name):
    for line in snapshot.splitlines():
        if line.split()[:1] == [name]:
            return line.strip()
    return None


def output_is_active(snapshot, name):
    line = output_header(snapshot, name)
    return line is not None and " connected" in line and re.search(r"\b\d+x\d+[+-]\d+[+-]\d+\b", line) is not None


def find_window(env, uid, gid, app):
    if app.poll() is not None:
        raise RuntimeError("synthetic xmessage window exited before verification")
    output = command_output(["xdotool", "search", "--onlyvisible", "--name", "^{}$".format(WINDOW_TITLE)], env, uid, gid)
    window_ids = [int(value.strip()) for value in output.splitlines() if value.strip()]
    if len(window_ids) != 1:
        raise RuntimeError("expected one visible synthetic xmessage window")
    return "0x{:x}".format(window_ids[0])


def read_xorg_pid(display):
    number = display[1:].split(".", 1)[0]
    path = Path("/tmp/.X{}-lock".format(number))
    try:
        return int(path.read_text(encoding="ascii").strip())
    except (OSError, ValueError):
        return None


def wait_desktop(env, uid, gid, session, timeout=60):
    deadline = time.time() + timeout
    while time.time() < deadline:
        if session.poll() is not None:
            raise RuntimeError("GNOME Flashback session exited before the panel became ready")
        processes = set()
        for entry in Path("/proc").iterdir():
            if not entry.name.isdigit():
                continue
            try:
                name = (entry / "comm").read_text(encoding="ascii").strip()
                status = (entry / "status").read_text(encoding="ascii")
                uid_line = next(line for line in status.splitlines() if line.startswith("Uid:"))
                owner_uid = int(uid_line.split()[1])
            except (OSError, StopIteration, ValueError):
                continue
            if owner_uid == uid:
                processes.add(name)
        try:
            wm = command_output(["xprop", "-root", "_NET_SUPPORTING_WM_CHECK"], env, uid, gid)
        except RuntimeError:
            wm = ""
        wm_id = re.search(r"window id # (0x[0-9a-fA-F]+)", wm)
        if "gnome-panel" in processes and "metacity" in processes and wm_id:
            wm_name = command_output(["xprop", "-id", wm_id.group(1), "_NET_WM_NAME"], env, uid, gid)
            if "Metacity" in wm_name:
                return {"window_manager_id": wm_id.group(1), "panel_process": True}
        time.sleep(0.25)
    raise RuntimeError("GNOME Flashback panel and Metacity window manager did not become ready")


class XorgSession:
    def __init__(self, root, artifact_dir, mode, uid, gid, username, home, shell):
        self.root = root
        self.mode = mode
        self.uid = uid
        self.gid = gid
        self.display = next(display_candidates(), None)
        require(self.display is not None, "no unused Xorg display number from :100 to :179")
        self.base = root / mode
        self.base.mkdir()
        for name in ("home", "runtime", "config", "cache", "data", "fixture"):
            directory = self.base / name
            directory.mkdir()
            directory.chmod(0o700 if name == "runtime" else 0o755)
        (self.base / "fixture" / "card0-HDMI-A-1").mkdir()
        (self.base / "fixture" / "card0-VIRTUAL-1").mkdir()
        (self.base / "fixture" / "card0-VIRTUAL-1" / "status").write_text("connected\n", encoding="ascii")
        initial_status = "connected" if mode == "hotplug" else "disconnected"
        (self.base / "fixture" / "card0-HDMI-A-1" / "status").write_text(initial_status + "\n", encoding="ascii")
        self.fixture = self.base / "fixture"
        self.artifacts = artifact_dir / mode
        self.artifacts.mkdir(parents=True, exist_ok=True)
        chown_tree(self.artifacts, uid, gid)
        self.config_file = self.base / "xorg.conf"
        self.mode_label = make_xorg_config(self.config_file)
        require(self.mode_label == MODE, "cvt mode label changed; update the Rust fixture mode consistently")
        self.authority = self.base / "Xauthority"
        self.authority.write_bytes(b"")
        self.authority.chmod(0o600)
        cookie = os.urandom(16).hex()
        subprocess.check_call(["xauth", "-f", str(self.authority), "add", self.display, "MIT-MAGIC-COOKIE-1", cookie])
        self.env = runner_env(self.base, self.display, self.authority)
        self.env["USER"] = username
        self.env["LOGNAME"] = username
        self.env["SHELL"] = shell
        self.env["HOME"] = str(self.base / "home")
        (self.base / "home").chmod(0o755)
        self.xorg_log = open(str(self.base / "xorg.log"), "wb")
        self.session_log = open(str(self.base / "session.log"), "wb")
        self.xorg = None
        self.session = None
        self.app = None
        chown_tree(self.base, uid, gid)

    def start(self):
        xorg_binary = "/usr/lib/xorg/Xorg" if Path("/usr/lib/xorg/Xorg").is_file() else "Xorg"
        xorg_args = [xorg_binary, self.display, "-noreset", "-nolisten", "tcp", "-auth", str(self.authority),
                     "-config", str(self.config_file), "-logfile", str(self.base / "xorg.log")]
        if os.geteuid() == 0:
            xorg_args.append("-novtswitch")
        self.xorg = popen(
            xorg_args,
            self.env, self.uid, self.gid, self.xorg_log,
            run_as_root=os.geteuid() == 0,
        )
        ready = False
        for _ in range(200):
            if self.xorg.poll() is not None:
                raise RuntimeError("Xorg exited during startup; see {}".format(self.base / "xorg.log"))
            try:
                command_output(["xdpyinfo"], self.env, self.uid, self.gid, timeout=2)
                ready = True
                break
            except RuntimeError:
                time.sleep(0.1)
        require(ready, "Xorg did not become ready")
        snapshot = xrandr_query(self.env, self.uid, self.gid)
        require(output_header(snapshot, "DUMMY0") is not None, "xserver-xorg-video-dummy did not expose DUMMY0")
        require(output_header(snapshot, "DUMMY1") is not None, "xserver-xorg-video-dummy did not expose DUMMY1")
        for output in ("DUMMY0", "DUMMY1"):
            command_output(["xrandr", "--addmode", output, self.mode_label], self.env, self.uid, self.gid)
        if self.mode == "hotplug":
            command_output(["xrandr", "--output", "DUMMY0", "--mode", self.mode_label, "--primary", "--pos", "0x0",
                            "--output", "DUMMY1", "--off", "--fb", DISPLAY_SIZE], self.env, self.uid, self.gid)
            require(output_is_active(xrandr_query(self.env, self.uid, self.gid), "DUMMY0"), "DUMMY0 did not activate")
        else:
            command_output(["xrandr", "--output", "DUMMY0", "--off", "--output", "DUMMY1", "--off",
                            "--fb", DISPLAY_SIZE], self.env, self.uid, self.gid)
            cold_snapshot = xrandr_query(self.env, self.uid, self.gid)
            require(not output_is_active(cold_snapshot, "DUMMY0"), "cold Xorg unexpectedly activated DUMMY0")
            require(not output_is_active(cold_snapshot, "DUMMY1"), "cold Xorg unexpectedly activated DUMMY1")

        self.session = popen(
            ["dbus-run-session", "--", "gnome-session", "--session=gnome-flashback-metacity"],
            self.env, self.uid, self.gid, self.session_log,
        )
        self.desktop_state = wait_desktop(self.env, self.uid, self.gid, self.session)
        self.app = popen(
            ["xmessage", "-name", "uu-same-session-acceptance", "-title", WINDOW_TITLE,
             "-bg", "#ff00ff", "-fg", "#101010", "-geometry", "420x220+20+60",
             "UU same-session desktop acceptance"],
            self.env, self.uid, self.gid, subprocess.DEVNULL,
        )
        window = None
        for _ in range(100):
            try:
                window = find_window(self.env, self.uid, self.gid, self.app)
                break
            except RuntimeError:
                time.sleep(0.1)
        require(window is not None, "synthetic magenta xmessage window did not map")
        initial_status = "connected" if self.mode == "hotplug" else "disconnected"
        self.identity = {
            "display": self.display,
            "xserver_pid": read_xorg_pid(self.display) or self.xorg.pid,
            "xorg_process_pid": self.xorg.pid,
            "root_window_id": root_window_id(self.env, self.uid, self.gid),
            "window_id": window,
            "app_pid": self.app.pid,
            "window_manager_id": self.desktop_state["window_manager_id"],
            "panel_process": self.desktop_state["panel_process"],
            "initial_drm_physical_status": initial_status,
            "initial_randr_DUMMY0": output_header(xrandr_query(self.env, self.uid, self.gid), "DUMMY0"),
            "initial_randr_DUMMY1": output_header(xrandr_query(self.env, self.uid, self.gid), "DUMMY1"),
        }
        self.identity["xorg_start_ticks"] = process_start_ticks(self.identity["xserver_pid"])
        self.identity["app_start_ticks"] = process_start_ticks(self.app.pid)
        return self

    def verify_still_alive(self):
        require(self.xorg is not None and self.xorg.poll() is None, "the original Xorg process exited")
        require(self.session is not None and self.session.poll() is None, "the original desktop session exited")
        require(self.app is not None and self.app.poll() is None, "the original xmessage process exited")
        server_pid = read_xorg_pid(self.display) or self.xorg.pid
        require(server_pid == self.identity["xserver_pid"], "the Xorg lock now names another server PID")
        require(process_start_ticks(server_pid) == self.identity["xorg_start_ticks"], "the Xorg PID was replaced")
        require(process_start_ticks(self.app.pid) == self.identity["app_start_ticks"], "the xmessage PID was replaced")
        require(root_window_id(self.env, self.uid, self.gid) == self.identity["root_window_id"], "the Xorg root window changed")
        require(find_window(self.env, self.uid, self.gid, self.app) == self.identity["window_id"], "the original xmessage window changed")
        return True

    def stop(self):
        stop_process(self.app)
        stop_process(self.session)
        stop_process(self.xorg)
        for stream in (self.xorg_log, self.session_log):
            try:
                stream.close()
            except Exception:
                pass


def process_start_ticks(pid):
    try:
        contents = Path("/proc/{}/stat".format(pid)).read_text(encoding="ascii")
        tail = contents[contents.rfind(")") + 2:].split()
        return tail[19]
    except (OSError, IndexError):
        return None


def magenta_pixels(path):
    data = path.read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise RuntimeError("{} is not a PNG".format(path.name))
    width = height = color_type = None
    compressed = bytearray()
    offset = 8
    while offset + 12 <= len(data):
        length = int.from_bytes(data[offset:offset + 4], "big")
        kind = data[offset + 4:offset + 8]
        body = data[offset + 8:offset + 8 + length]
        if len(body) != length:
            raise RuntimeError("{} has a truncated PNG chunk".format(path.name))
        if kind == b"IHDR":
            width = int.from_bytes(body[0:4], "big")
            height = int.from_bytes(body[4:8], "big")
            depth, color_type = body[8], body[9]
            if depth != 8 or color_type != 6 or body[10:13] != b"\0\0\0":
                raise RuntimeError("{} uses an unsupported PNG format".format(path.name))
        elif kind == b"IDAT":
            compressed.extend(body)
        elif kind == b"IEND":
            break
        offset += 12 + length
    if not width or not height or color_type != 6:
        raise RuntimeError("{} is missing an RGBA PNG header".format(path.name))
    stride = width * 4
    raw = zlib.decompress(compressed)
    if len(raw) != height * (stride + 1):
        raise RuntimeError("{} has an invalid PNG image size".format(path.name))
    count = 0
    cursor = 0
    for _ in range(height):
        if raw[cursor] != 0:
            raise RuntimeError("{} uses an unsupported PNG filter".format(path.name))
        cursor += 1
        row = raw[cursor:cursor + stride]
        cursor += stride
        for index in range(0, stride, 4):
            red, green, blue = row[index:index + 3]
            if red >= 230 and green <= 40 and blue >= 230:
                count += 1
    return width, height, count


def run_helper(binary, session, mode):
    artifact_dir = session.artifacts
    command = [str(binary), "same-session-acceptance", str(session.fixture), str(artifact_dir), mode]
    process = popen(command, session.env, session.uid, session.gid, subprocess.PIPE, stderr=subprocess.PIPE)
    try:
        stdout, stderr = process.communicate(timeout=150)
    except subprocess.TimeoutExpired:
        stop_process(process)
        raise RuntimeError("same-session Rust helper timed out in {} mode".format(mode))
    output = stdout.decode("utf-8", errors="replace")
    diagnostic = stderr.decode("utf-8", errors="replace")
    if process.returncode:
        # Synthetic display only: retain effective RandR state and the failing operation's
        # server reply. These diagnostics never run against an operator's desktop.
        diagnostics = []
        _, timings = cvt_modeline()
        operations = [["--query"], ["--newmode", "uu-remote-1024x768-60"] + timings,
                      ["--addmode", "DUMMY1", "uu-remote-1024x768-60"],
                      ["--output", "DUMMY1", "--mode", "uu-remote-1024x768-60", "--primary", "--pos", "0x0"],
                      ["--output", "DUMMY1", "--mode", MODE, "--primary", "--pos", "0x0"], ["--query"]]
        for args in operations:
            try:
                diagnostics.append(command_output(["xrandr", "--display", session.display] + args, session.env, session.uid, session.gid))
            except RuntimeError as error:
                diagnostics.append(str(error))
        (artifact_dir / "randr-failure.log").write_text("\n".join(diagnostics), encoding="utf-8")
        raise RuntimeError("same-session Rust helper failed in {} mode (exit={})\n{}".format(
            mode, process.returncode, "\n".join(diagnostic.splitlines()[-40:])[-4000:]))
    lines = [line for line in output.splitlines() if line.strip()]
    if not lines:
        raise RuntimeError("same-session Rust helper emitted no result")
    report = json.loads(lines[-1])
    require(isinstance(report, dict), "same-session helper result must be a JSON object")
    return report


def verify_report(report, mode, session):
    if mode == "hotplug":
        required = (
            "physical_present_suppressed", "disconnected_debounced_twice",
            "same_session_display_preserved", "virtual_output_retained_after_replug",
            "ffmpeg_idle_before_probes", "ffmpeg_idle_after_probes", "manager_shutdown_kept_xorg",
        )
        expected_captures = ("hotplug-before.png", "hotplug-after-switch.png", "hotplug-after-replug.png")
    else:
        required = (
            "started_without_physical_output", "cold_start_enabled_virtual_output",
            "full_session_capture_ready", "ffmpeg_idle_before_probes", "ffmpeg_idle_after_probes",
            "manager_shutdown_kept_xorg",
        )
        expected_captures = ("cold-after-activation.png", "cold-after-reconnect.png")
    for name in required:
        require(report.get(name) is True, "acceptance failed: {} was not true".format(name))
    require(report.get("mode") == mode, "Rust helper reported the wrong fixture mode")
    require(report.get("probe_count") == 2, "acceptance did not run the initial and reconnect probes")
    captures = report.get("captures")
    require(isinstance(captures, list) and tuple(captures) == expected_captures, "Rust helper reported unexpected capture files")
    images = []
    for basename in captures:
        require(Path(basename).name == basename, "capture path escaped its artifact directory")
        width, height, count = magenta_pixels(session.artifacts / basename)
        require(width >= 320 and height >= 220 and count >= 1000,
                "{} does not show the synthetic magenta window".format(basename))
        images.append({"file": basename, "width": width, "height": height, "magenta_pixels": count})
    require(session.verify_still_alive(), "original Xorg session did not survive manager shutdown")
    return images


def write_failure(artifact_dir, error, sessions):
    lines = ["same-session display acceptance failed", str(error)]
    for session in sessions:
        if session is None:
            continue
        for name, stream in (("xorg.log", session.xorg_log), ("session.log", session.session_log)):
            try:
                with open(stream.name, "r", encoding="utf-8", errors="replace") as log_file:
                    contents = log_file.read()
            except Exception:
                continue
            if contents:
                lines.append("{} ({}) tail:\n{}".format(name, session.mode, "\n".join(contents.splitlines()[-30:])[-3000:]))
    artifact_dir.mkdir(parents=True, exist_ok=True)
    (artifact_dir / "acceptance.log").write_text("\n".join(lines) + "\n", encoding="utf-8")


def main():
    if len(sys.argv) != 3:
        print("usage: same_session_display.py <uuremote-server> <artifact-dir>", file=sys.stderr)
        return 2
    binary = Path(sys.argv[1]).resolve()
    artifact_dir = Path(sys.argv[2]).resolve()
    if not binary.is_file():
        print("server binary does not exist: {}".format(binary), file=sys.stderr)
        return 2
    artifact_dir.mkdir(parents=True, exist_ok=True)
    uid, gid, username, home, shell = acceptance_identity()
    require(uid != 0, "an unprivileged desktop session user is required")
    sessions = []
    temporary = tempfile.TemporaryDirectory(prefix="uu-same-session-")
    try:
        root = Path(temporary.name)
        if os.geteuid() == 0:
            os.chown(root, uid, gid)
            root.chmod(0o755)
        reports = {}
        for mode in ("hotplug", "cold"):
            session = XorgSession(root, artifact_dir, mode, uid, gid, username, home, shell)
            sessions.append(session)
            session.start()
            report = run_helper(binary, session, mode)
            images = verify_report(report, mode, session)
            for image in images:
                image["file"] = (Path(mode) / image["file"]).as_posix()
            reports[mode] = {
                "report": report,
                "session": session.identity,
                "verified_captures": images,
                "xorg_and_full_desktop_survived": True,
            }
            session.stop()
        safe_report = {"hotplug": reports["hotplug"], "cold": reports["cold"]}
        (artifact_dir / "acceptance.log").write_text(
            "same-session display acceptance passed\n" + json.dumps(safe_report, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        print("same-session display acceptance passed: Xorg session identity, DUMMY output lifecycle, capture, and encoder cleanup")
        return 0
    except Exception as error:
        write_failure(artifact_dir, error, sessions)
        print("same-session display acceptance failed: {}".format(error), file=sys.stderr)
        return 1
    finally:
        for session in reversed(sessions):
            session.stop()
        temporary.cleanup()


if __name__ == "__main__":
    raise SystemExit(main())
