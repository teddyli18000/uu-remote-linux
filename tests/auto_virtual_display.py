#!/usr/bin/env python3
"""Exercise automatic virtual-display lifecycle without cloud credentials."""

import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import zlib


REQUIRED_FLAGS = (
    "physical_present_suppressed",
    "fallback_started_after_disconnect",
    "fallback_kept_after_reconnect",
    "display_pid_stable",
    "window_id_stable",
    "cold_boot_started",
    "ffmpeg_idle_before_probes",
    "ffmpeg_idle_after_probes",
    "host_shutdown_released_display",
)


def paeth(a, b, c):
    p = a + b - c
    pa, pb, pc = abs(p - a), abs(p - b), abs(p - c)
    if pa <= pb and pa <= pc:
        return a
    if pb <= pc:
        return b
    return c


def magenta_pixels(path):
    data = path.read_bytes()
    if data[:8] != b"\x89PNG\r\n\x1a\n":
        raise RuntimeError(f"{path.name} is not a PNG")

    width = height = color_type = None
    compressed = bytearray()
    offset = 8
    while offset + 12 <= len(data):
        length = struct.unpack_from(">I", data, offset)[0]
        kind = data[offset + 4 : offset + 8]
        body = data[offset + 8 : offset + 8 + length]
        if len(body) != length:
            raise RuntimeError(f"{path.name} has a truncated PNG chunk")
        if kind == b"IHDR":
            width, height, depth, color_type, compression, filtering, interlace = struct.unpack(
                ">IIBBBBB", body
            )
            if depth != 8 or color_type not in (2, 6) or compression or filtering or interlace:
                raise RuntimeError(f"{path.name} uses an unsupported PNG format")
        elif kind == b"IDAT":
            compressed.extend(body)
        elif kind == b"IEND":
            break
        offset += 12 + length

    if not width or not height or color_type not in (2, 6):
        raise RuntimeError(f"{path.name} is missing a supported PNG header")

    bpp = 3 if color_type == 2 else 4
    stride = width * bpp
    raw = zlib.decompress(compressed)
    if len(raw) != height * (stride + 1):
        raise RuntimeError(f"{path.name} has an invalid PNG image size")

    count = 0
    previous = bytearray(stride)
    cursor = 0
    for _ in range(height):
        filter_type = raw[cursor]
        cursor += 1
        encoded = raw[cursor : cursor + stride]
        cursor += stride
        row = bytearray(stride)
        for index, value in enumerate(encoded):
            left = row[index - bpp] if index >= bpp else 0
            above = previous[index]
            upper_left = previous[index - bpp] if index >= bpp else 0
            if filter_type == 0:
                predictor = 0
            elif filter_type == 1:
                predictor = left
            elif filter_type == 2:
                predictor = above
            elif filter_type == 3:
                predictor = (left + above) // 2
            elif filter_type == 4:
                predictor = paeth(left, above, upper_left)
            else:
                raise RuntimeError(f"{path.name} has an invalid PNG filter")
            row[index] = (value + predictor) & 0xFF

        for index in range(0, stride, bpp):
            red, green, blue = row[index : index + 3]
            if red >= 230 and green <= 40 and blue >= 230:
                count += 1
        previous = row
    return width, height, count


def main():
    if len(sys.argv) != 3:
        print("usage: auto_virtual_display.py <uuremote-server> <artifact-dir>", file=sys.stderr)
        return 2

    binary = Path(sys.argv[1]).resolve()
    artifact_dir = Path(sys.argv[2]).resolve()
    if not binary.is_file():
        print(f"server binary does not exist: {binary}", file=sys.stderr)
        return 2
    artifact_dir.mkdir(parents=True, exist_ok=True)

    with tempfile.TemporaryDirectory(prefix="uu-virtual-display-") as temporary:
        temporary = Path(temporary)
        fixture = temporary / "drm"
        connector = fixture / "card0-HDMI-A-1"
        connector.mkdir(parents=True)
        (connector / "status").write_text("connected\n", encoding="ascii")
        runtime_dir = temporary / "runtime"
        runtime_dir.mkdir(mode=0o700)
        home_dir = temporary / "home"
        home_dir.mkdir()
        for name in ("xdg-config", "xdg-cache", "xdg-data"):
            (temporary / name).mkdir()
        output_dir = artifact_dir / "captures"
        output_dir.mkdir(exist_ok=True)

        env = {name: os.environ[name] for name in ("PATH", "LD_LIBRARY_PATH") if name in os.environ}
        env.update(
            {
                "HOME": str(home_dir),
                "TMPDIR": str(temporary),
                "UU_CONFIG": str(temporary / "no-config.json"),
                "UU_DESKTOP_BACKEND": "auto",
                "UU_LOGIN_WINDOW": "0",
                "UU_REMOTE_ASSIST": "0",
                "UU_AUDIO": "0",
                "UU_VIRTUAL_BUS": "private",
                "UU_VIRTUAL_DISPLAY_MODE": "xvfb",
                "UU_VIRTUAL_SESSION": "openbox-session",
                "UU_VIRTUAL_DISPLAY": ":99",
                "UU_VIRTUAL_SIZE": "1024x768",
                "XDG_RUNTIME_DIR": str(runtime_dir),
                "XDG_CONFIG_HOME": str(temporary / "xdg-config"),
                "XDG_CACHE_HOME": str(temporary / "xdg-cache"),
                "XDG_DATA_HOME": str(temporary / "xdg-data"),
                "LC_ALL": "C.UTF-8",
                "LANG": "C.UTF-8",
                "RUST_LOG": "warn",
            }
        )
        command = [str(binary), "virtual-display-acceptance", str(fixture), str(output_dir)]
        # Files preserve the last fixture stage even if a descendant retains inherited streams.
        with tempfile.TemporaryFile() as stdout, tempfile.TemporaryFile() as stderr:
            process = subprocess.Popen(command, env=env, stdout=stdout, stderr=stderr)
            try:
                returncode = process.wait(timeout=180)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
                returncode = 124
            stdout.seek(0)
            stderr.seek(0)
            output = stdout.read().decode("utf-8", errors="replace")
            diagnostic = stderr.read().decode("utf-8", errors="replace")
        if returncode != 0:
            stderr_tail = "\n".join(diagnostic.splitlines()[-40:])[-4000:]
            (artifact_dir / "acceptance.log").write_text(
                "virtual-display acceptance helper failed\n"
                f"exit_code={returncode}\n"
                f"stderr_tail:\n{stderr_tail}\n",
                encoding="utf-8",
            )
            print(f"virtual-display acceptance helper exited {returncode}", file=sys.stderr)
            if stderr_tail:
                print(stderr_tail, file=sys.stderr)
            return returncode

        lines = [line for line in output.splitlines() if line.strip()]
        if not lines:
            raise RuntimeError("helper did not emit its final JSON result")
        report = json.loads(lines[-1])
        if not isinstance(report, dict):
            raise RuntimeError("helper result must be a JSON object")
        for name in REQUIRED_FLAGS:
            if report.get(name) is not True:
                raise RuntimeError(f"acceptance failed: {name} was not true")
        if report.get("capture_count") != 2:
            raise RuntimeError("acceptance did not produce exactly two desktop captures")
        captures = report.get("captures")
        if not isinstance(captures, list) or len(captures) != 2:
            raise RuntimeError("helper result must name two capture PNGs")

        image_results = []
        for basename in captures:
            if not isinstance(basename, str) or Path(basename).name != basename:
                raise RuntimeError("capture paths must be output-directory basenames")
            image = output_dir / basename
            width, height, count = magenta_pixels(image)
            if width < 100 or height < 100 or count < 1000:
                raise RuntimeError(f"{basename} does not show the synthetic magenta desktop window")
            image_results.append({"file": basename, "width": width, "height": height, "magenta_pixels": count})

        safe_report = {name: report[name] for name in REQUIRED_FLAGS}
        safe_report.update({"capture_count": 2, "captures": image_results})
        (artifact_dir / "acceptance.log").write_text(
            "virtual-display acceptance passed\n" + json.dumps(safe_report, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        print("virtual-display acceptance passed: monitor decision, fallback lifecycle, captures, and idle encoder checks")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except Exception as error:
        print(f"virtual-display acceptance failed: {error}", file=sys.stderr)
        raise
