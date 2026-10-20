//! Account-free integration acceptance for virtual outputs in one live Xorg session.
//!
//! The Python fixture owns Xorg, the desktop session, and the synthetic RandR topology. This
//! helper drives the real display manager against a temporary DRM tree and probes capture and
//! encoding without starting the cloud host or contacting a remote client.

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::Duration,
};

use crate::{capture::ScreenCapture, virtual_display::{DisplayConfig, VirtualDisplayManager}};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn require(ok: bool, message: &'static str) -> Result<()> {
    if ok { Ok(()) } else { Err(message.into()) }
}

fn x11_context() -> Result<DisplayConfig> {
    require(std::env::var_os("WAYLAND_DISPLAY").is_none(), "same-session acceptance requires an Xorg session")?;
    require(std::env::var("XDG_SESSION_TYPE").ok().as_deref() == Some("x11"), "same-session acceptance requires XDG_SESSION_TYPE=x11")?;
    let display = std::env::var("DISPLAY").map_err(|_| "same-session acceptance requires DISPLAY")?;
    require(!display.is_empty(), "DISPLAY must not be empty")?;
    let authority = std::env::var_os("XAUTHORITY")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".Xauthority")))
        .ok_or("same-session acceptance requires XAUTHORITY")?;
    require(authority.is_file(), "XAUTHORITY file is missing")?;
    Ok(DisplayConfig { display, authority })
}

fn xrandr(config: &DisplayConfig, args: &[&str]) -> Result<String> {
    let mut command = Command::new("xrandr");
    config.apply_to_command(&mut command);
    let output = command.args(args).output()?;
    if !output.status.success() {
        return Err(format!("xrandr command failed (exit={})", output.status).into());
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn output_active(snapshot: &str, output: &str) -> bool {
    snapshot.lines().any(|line| {
        let mut fields = line.split_whitespace();
        fields.next() == Some(output)
            && fields.next() == Some("connected")
            && fields.any(|field| {
                let Some((width, rest)) = field.split_once('x') else { return false };
                let height = rest.split(['+', '-']).next().unwrap_or("");
                !width.is_empty()
                    && !height.is_empty()
                    && width.bytes().all(|byte| byte.is_ascii_digit())
                    && height.bytes().all(|byte| byte.is_ascii_digit())
            })
    })
}

fn output_primary_at_origin(snapshot: &str, output: &str) -> bool {
    snapshot.lines().any(|line| {
        let mut fields = line.split_whitespace();
        if fields.next() != Some(output) || fields.next() != Some("connected") {
            return false;
        }
        let rest: Vec<_> = fields.collect();
        rest.contains(&"primary") && rest.iter().any(|field| {
            let Some((width, geometry)) = field.split_once('x') else { return false };
            let Some((height, origin)) = geometry.split_once('+') else { return false };
            width.bytes().all(|byte| byte.is_ascii_digit())
                && height.bytes().all(|byte| byte.is_ascii_digit())
                && origin.starts_with("0+0")
        })
    })
}

fn fixture_status(fixture: &Path) -> Result<String> {
    Ok(fs::read_to_string(fixture.join("card0-HDMI-A-1/status"))?.trim().to_owned())
}

fn set_fixture_status(fixture: &Path, value: &str) -> Result<()> {
    fs::write(fixture.join("card0-HDMI-A-1/status"), format!("{value}\n"))?;
    Ok(())
}

fn canonical_fixture(path: &Path) -> Result<PathBuf> {
    let fixture = fs::canonicalize(path)?;
    require(!fixture.starts_with("/sys"), "acceptance fixture must be outside /sys")?;
    require(fixture.join("card0-HDMI-A-1/status").is_file(), "physical connector fixture is missing")?;
    Ok(fixture)
}

fn capture_png(config: &DisplayConfig, path: &Path) -> Result<(u32, u32)> {
    use std::io::Write;
    let screen = ScreenCapture::new()?;
    require(screen.display == config.display, "capture opened a different X display")?;
    require(screen.width > 0 && screen.height > 0, "Xorg capture dimensions are empty")?;
    let mut bgra = screen.capture_frame()?;
    require(bgra.len() == screen.width as usize * screen.height as usize * 4, "Xorg capture frame size is invalid")?;
    for pixel in bgra.chunks_exact_mut(4) {
        pixel.swap(0, 2);
        pixel[3] = 255;
    }
    let png = crate::cursor::encode_png(screen.width, screen.height, &bgra);
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(&png)?;
    Ok((screen.width, screen.height))
}

fn ffmpeg_idle() -> bool {
    fs::read_dir("/proc").into_iter().flatten().flatten().all(|entry| {
        fs::read_to_string(entry.path().join("comm"))
            .map_or(true, |name| name.trim() != "ffmpeg")
    })
}

fn wait_ffmpeg_idle(runtime: &tokio::runtime::Runtime) -> bool {
    // Keep the current-thread runtime driving task cancellation and child reaping.
    runtime.block_on(async {
        for _ in 0..60 {
            if ffmpeg_idle() { return true; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    })
}

fn verify_active_virtual(config: &DisplayConfig) -> Result<()> {
    let snapshot = xrandr(config, &["--query"])?;
    require(output_active(&snapshot, "DUMMY1"), "DUMMY1 is not active in the live Xorg session")?;
    require(output_primary_at_origin(&snapshot, "DUMMY1"), "DUMMY1 is not primary at the Xorg origin")?;
    Ok(())
}

fn poll_twice(manager: &VirtualDisplayManager) -> Result<Option<DisplayConfig>> {
    require(manager.poll()?.is_none(), "first connector sample changed the selected display")?;
    thread::sleep(Duration::from_millis(250));
    Ok(manager.poll()?)
}

fn run_hotplug(fixture: &Path, artifact_dir: &Path, config: &DisplayConfig) -> Result<serde_json::Value> {
    require(fixture_status(fixture)? == "connected", "hotplug fixture must start physically connected")?;
    let initial = xrandr(config, &["--query"])?;
    require(output_active(&initial, "DUMMY0"), "DUMMY0 must be active before the unplug transition")?;
    require(!output_active(&initial, "DUMMY1"), "DUMMY1 must be inactive before the unplug transition")?;

    let manager = VirtualDisplayManager::with_same_session_fixture(fixture, "DUMMY1");
    manager.prepare_environment()?;
    require(manager.poll()?.is_none(), "connected physical fixture activated a virtual output")?;
    require(manager.poll()?.is_none(), "connected physical fixture activated a virtual output")?;
    require(manager.selected_display().is_none(), "manager selected a virtual display while physical output was connected")?;
    require(std::env::var("DISPLAY").ok().as_deref() == Some(config.display.as_str()), "DISPLAY changed before unplug")?;
    let before = capture_png(config, &artifact_dir.join("hotplug-before.png"))?;

    xrandr(config, &["--output", "DUMMY0", "--off"])?;
    set_fixture_status(fixture, "disconnected")?;
    require(manager.poll()?.is_none(), "virtual output activated after only one disconnected sample")?;
    thread::sleep(Duration::from_millis(250));
    let selected = manager.poll()?.ok_or("DUMMY1 did not activate after two disconnected samples")?;
    selected.activate();
    require(selected == *config, "manager selected a different X display or authority")?;
    require(manager.selected_display().as_ref() == Some(&selected), "manager did not retain its selected display")?;
    require(std::env::var("DISPLAY").ok().as_deref() == Some(config.display.as_str()), "DISPLAY changed after unplug")?;
    verify_active_virtual(&selected)?;
    let _ = crate::capture::x11_resized();
    require(ffmpeg_idle(), "FFmpeg was active before a desktop probe")?;
    let after_switch = capture_png(&selected, &artifact_dir.join("hotplug-after-switch.png"))?;

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(crate::video::probe(after_switch, selected.display.clone(), None, None))?;
    require(wait_ffmpeg_idle(&runtime), "FFmpeg remained active after the first video probe")?;

    // Replug DUMMY0 on the same framebuffer. Both outputs overlap intentionally so the root
    // geometry and the window placement remain stable while DUMMY1 stays primary.
    xrandr(config, &[
        "--output", "DUMMY0", "--mode", "1024x768_60.00", "--pos", "0x0",
        "--output", "DUMMY1", "--primary", "--pos", "0x0",
        "--fb", "1024x768",
    ])?;
    set_fixture_status(fixture, "connected")?;
    let retained = manager.poll()?.ok_or("replug cleared the selected same-session display")?;
    require(retained == selected, "replug replaced the selected display")?;
    require(manager.selected_display().as_ref() == Some(&selected), "replug changed manager selection")?;
    verify_active_virtual(&selected)?;
    let _ = crate::capture::x11_resized();
    let after_replug = capture_png(&selected, &artifact_dir.join("hotplug-after-replug.png"))?;
    runtime.block_on(crate::video::probe(after_replug, selected.display.clone(), None, None))?;
    require(wait_ffmpeg_idle(&runtime), "FFmpeg remained active after the reconnect video probe")?;

    manager.shutdown();
    require(manager.selected_display().is_none(), "manager shutdown retained its virtual selection")?;
    require(std::env::var("DISPLAY").ok().as_deref() == Some(config.display.as_str()), "manager shutdown changed DISPLAY")?;
    require(xrandr(config, &["--query"]).is_ok(), "manager shutdown stopped the Xorg server")?;

    Ok(serde_json::json!({
        "mode": "hotplug",
        "physical_present_suppressed": true,
        "disconnected_debounced_twice": true,
        "same_session_display_preserved": true,
        "selected_output": "DUMMY1",
        "virtual_output_retained_after_replug": true,
        "ffmpeg_idle_before_probes": true,
        "ffmpeg_idle_after_probes": true,
        "manager_shutdown_kept_xorg": true,
        "probe_count": 2,
        "captures": ["hotplug-before.png", "hotplug-after-switch.png", "hotplug-after-replug.png"],
        "dimensions": {"before": before, "after_switch": after_switch, "after_replug": after_replug}
    }))
}

fn run_cold(fixture: &Path, artifact_dir: &Path, config: &DisplayConfig) -> Result<serde_json::Value> {
    require(fixture_status(fixture)? == "disconnected", "cold fixture must start with no physical connector")?;
    let initial = xrandr(config, &["--query"])?;
    require(!output_active(&initial, "DUMMY0"), "cold fixture unexpectedly has DUMMY0 active")?;
    require(!output_active(&initial, "DUMMY1"), "cold fixture must begin before DUMMY1 activation")?;

    let manager = VirtualDisplayManager::with_same_session_fixture(fixture, "DUMMY1");
    manager.prepare_environment()?;
    require(manager.poll()?.is_none(), "cold start bypassed the first connector debounce sample")?;
    thread::sleep(Duration::from_millis(250));
    let selected = manager.poll()?.ok_or("cold start did not activate DUMMY1 in the existing Xorg session")?;
    selected.activate();
    require(selected == *config, "cold start selected a different X display or authority")?;
    verify_active_virtual(&selected)?;
    let _ = crate::capture::x11_resized();
    require(!output_active(&xrandr(&selected, &["--query"])? , "DUMMY0"), "cold start activated physical DUMMY0")?;
    require(ffmpeg_idle(), "FFmpeg was active before a cold-start desktop probe")?;

    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let first = capture_png(&selected, &artifact_dir.join("cold-after-activation.png"))?;
    runtime.block_on(crate::video::probe(first, selected.display.clone(), None, None))?;
    require(wait_ffmpeg_idle(&runtime), "FFmpeg remained active after the cold-start video probe")?;
    let second = capture_png(&selected, &artifact_dir.join("cold-after-reconnect.png"))?;
    runtime.block_on(crate::video::probe(second, selected.display.clone(), None, None))?;
    require(wait_ffmpeg_idle(&runtime), "FFmpeg remained active after the cold reconnect probe")?;

    manager.shutdown();
    require(manager.selected_display().is_none(), "cold manager shutdown retained its virtual selection")?;
    require(xrandr(config, &["--query"]).is_ok(), "cold manager shutdown stopped the Xorg server")?;
    Ok(serde_json::json!({
        "mode": "cold",
        "started_without_physical_output": true,
        "cold_start_enabled_virtual_output": true,
        "full_session_capture_ready": true,
        "ffmpeg_idle_before_probes": true,
        "ffmpeg_idle_after_probes": true,
        "manager_shutdown_kept_xorg": true,
        "probe_count": 2,
        "captures": ["cold-after-activation.png", "cold-after-reconnect.png"],
        "dimensions": {"after_activation": first, "after_reconnect": second}
    }))
}

/// Run `uuremote-server same-session-acceptance <fixture-drm-dir> <artifact-dir> <hotplug|cold>`.
pub fn run() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(2).collect();
    require(args.len() == 3, "usage: same-session-acceptance <fixture-drm-dir> <artifact-dir> <hotplug|cold>")?;
    let fixture = canonical_fixture(Path::new(&args[0]))?;
    let artifact_dir = PathBuf::from(&args[1]);
    fs::create_dir_all(&artifact_dir)?;
    let config = x11_context()?;

    std::env::set_var("UU_PROBE_FRAMES", "5");
    std::env::set_var("UU_PROBE_FPS", "5");
    std::env::set_var("UU_VIDEO_ENCODER", "cpu");
    std::env::set_var("UU_VIDEO_CODEC", "h264");

    let report = match args[2].to_string_lossy().as_ref() {
        "hotplug" => run_hotplug(&fixture, &artifact_dir, &config)?,
        "cold" => run_cold(&fixture, &artifact_dir, &config)?,
        _ => return Err("mode must be hotplug or cold".into()),
    };
    println!("{report}");
    Ok(())
}
