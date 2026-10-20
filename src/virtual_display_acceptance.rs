//! Account-free CI fixture. Only this command accepts a synthetic DRM tree.
use std::{error::Error, path::{Path, PathBuf}, process::{Child, Command, Stdio}, time::Duration};
use crate::{capture::ScreenCapture, virtual_display::VirtualDisplayManager};

struct Window(Child);
impl Drop for Window { fn drop(&mut self) { let _=self.0.kill(); let _=self.0.wait(); } }

fn require(value: bool, message: &'static str) -> Result<(), Box<dyn Error>> {
    if value { Ok(()) } else { Err(message.into()) }
}
fn status(root: &Path, state: &str) -> Result<(), Box<dyn Error>> {
    std::fs::write(root.join("card0-HDMI-A-1/status"), state)?;
    Ok(())
}
fn ffmpeg_idle() -> bool {
    !std::fs::read_dir("/proc").into_iter().flatten().flatten().any(|entry|
        std::fs::read_to_string(entry.path().join("comm")).is_ok_and(|name| name.trim()=="ffmpeg"))
}
fn wait_encoder_idle(runtime: &tokio::runtime::Runtime) -> bool {
    runtime.block_on(async {
        for _ in 0..50 {
            if ffmpeg_idle() { return true; }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    })
}
fn display_pid(display: &str) -> Result<u32, Box<dyn Error>> {
    Ok(std::fs::read_to_string(format!("/tmp/.X{}-lock", display.trim_start_matches(':')))?.trim().parse()?)
}
fn window_ids(config: &crate::virtual_display::DisplayConfig) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut command=Command::new("xdotool");
    config.apply_to_command(&mut command);
    let output=command.args(["search", "--onlyvisible", "--name", "^UU Virtual Display CI$"]).output()?;
    require(output.status.success(), "synthetic window not mapped")?;
    Ok(output.stdout)
}
fn capture(path: &Path) -> Result<(), Box<dyn Error>> {
    let screen=ScreenCapture::new()?;
    let mut rgba=screen.capture_frame()?;
    for pixel in rgba.chunks_exact_mut(4) { pixel.swap(0,2); pixel[3]=255; }
    std::fs::write(path, crate::cursor::encode_png(screen.width, screen.height, &rgba))?;
    Ok(())
}

pub fn run() -> Result<(), Box<dyn Error>> {
    let args: Vec<_>=std::env::args_os().skip(2).collect();
    require(matches!(args.len(), 2|3), "usage: virtual-display-acceptance <fixture-drm-dir> <output-dir> [--cold]")?;
    require(std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none(),
        "virtual display acceptance requires an isolated environment with no selected display")?;
    let fixture=PathBuf::from(&args[0]);
    let output=PathBuf::from(&args[1]);
    // Never accept the actual connector tree as an acceptance fixture.
    let fixture=std::fs::canonicalize(fixture)?;
    require(!fixture.starts_with("/sys") && fixture.join("card0-HDMI-A-1/status").is_file(), "an explicit synthetic DRM fixture is required")?;
    std::fs::create_dir_all(&output)?;
    let manager=VirtualDisplayManager::with_drm_root(&fixture);
    manager.prepare_environment()?;
    if args.len()==3 {
        require(args[2]=="--cold", "unknown virtual display acceptance mode")?;
        status(&fixture, "disconnected\n")?;
        let config=manager.prepare_startup()?.ok_or("cold boot did not create fallback")?;
        config.activate();
        let screen=ScreenCapture::new()?;
        require(screen.width>0 && screen.height>0, "cold boot capture unavailable")?;
        println!("{}",serde_json::json!({"cold_boot_started":true}));
        return Ok(());
    }
    // Separate process: an actual cold start, not a reused manager or inherited X display.
    let cold=Command::new(std::env::current_exe()?).arg("virtual-display-acceptance")
        .arg(&fixture).arg(&output).arg("--cold").env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null()).output()?;
    if !cold.status.success() {
        // This child has no account or cloud path; diagnostics contain only the synthetic fixture.
        eprintln!("Cold boot fixture failed: {}", String::from_utf8_lossy(&cold.stderr));
        return Err("cold boot subprocess failed".into());
    }
    require(cold.stdout.split(|byte| *byte==b'\n').filter_map(|line| serde_json::from_slice::<serde_json::Value>(line).ok())
        .any(|report|report["cold_boot_started"]==true), "cold boot report missing")?;
    // A second X server models the original physical desktop, so unplug acceptance exercises
    // a real native connection replacement and a late Drop of the previous capture lease.
    let physical=(120..140).map(|number|format!(":{number}")).find(|display|
        !Path::new(&format!("/tmp/.X{}-lock",display.trim_start_matches(':'))).exists()
        && !Path::new(&format!("/tmp/.X11-unix/X{}",display.trim_start_matches(':'))).exists())
        .ok_or("no isolated display for physical-desktop fixture")?;
    let _physical=Window(Command::new("Xvfb").args([&physical,"-screen","0","800x600x24","-nolisten","tcp","-ac","-noreset"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?);
    let mut ready=false;
    for _ in 0..50 {
        if Command::new("xdpyinfo").args(["-display", &physical]).stdout(Stdio::null()).stderr(Stdio::null()).status()?.success() { ready=true; break; }
        std::thread::sleep(Duration::from_millis(100));
    }
    require(ready,"physical-desktop fixture did not start")?;
    std::env::set_var("DISPLAY", &physical);
    let old_screen=ScreenCapture::new()?;
    status(&fixture, "connected\n")?;
    require(manager.poll()?.is_none() && manager.poll()?.is_none(), "fallback started with physical monitor connected")?;
    std::env::remove_var("DISPLAY");
    status(&fixture, "disconnected\n")?;
    require(manager.poll()?.is_none(), "unplug detection did not debounce")?;
    let config=manager.poll()?.ok_or("fallback did not start after unplug")?;
    config.activate();
    let new_screen=ScreenCapture::new()?;
    require(new_screen.display!=old_screen.display,"native capture did not change display")?;
    drop(old_screen);
    require(!new_screen.capture_frame()?.is_empty(),"old capture Drop closed the replacement display")?;
    drop(new_screen);
    let pid=display_pid(&config.display)?;
    let mut command=Command::new("xmessage");
    config.apply_to_command(&mut command);
    let _window=Window(command.args(["-title", "UU Virtual Display CI", "-bg", "#ff00ff", "-geometry", "320x200+20+20", "UU synthetic CI desktop"])
        .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?);
    let mut ids=Vec::new();
    for _ in 0..50 {
        if let Ok(mapped)=window_ids(&config) { ids=mapped; break; }
        std::thread::sleep(Duration::from_millis(100));
    }
    require(!ids.is_empty(), "synthetic window did not map")?;
    std::thread::sleep(Duration::from_millis(300));
    require(ffmpeg_idle(), "encoder running without desktop request")?;
    std::env::set_var("UU_PROBE_FRAMES", "5");
    std::env::set_var("UU_PROBE_FPS", "5");
    std::env::set_var("UU_VIDEO_ENCODER", "cpu");
    std::env::set_var("UU_VIDEO_CODEC", "h264");
    let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let captures=["virtual-before.png", "virtual-after.png"];
    for (index,name) in captures.iter().enumerate() {
        capture(&output.join(name))?;
        let screen=ScreenCapture::new()?;
        runtime.block_on(crate::video::probe((screen.width, screen.height),screen.display.clone(),None,
            Some(output.join(format!("virtual-{index}.h264")).to_string_lossy().into_owned())))?;
        require(wait_encoder_idle(&runtime), "encoder survived completed desktop probe")?;
        // No viewer or capture is kept alive in this interval.
        std::thread::sleep(Duration::from_millis(500));
        require(display_pid(&config.display)?==pid, "virtual display changed across probes")?;
        require(window_ids(&config)?==ids, "desktop window changed across probes")?;
    }
    status(&fixture, "connected\n")?;
    require(manager.poll()?.as_ref()==Some(&config), "replug replaced virtual desktop")?;
    require(display_pid(&config.display)?==pid && window_ids(&config)?==ids, "replug destroyed desktop state")?;
    println!("{}",serde_json::json!({
        "physical_present_suppressed":true,"fallback_started_after_disconnect":true,
        "fallback_kept_after_reconnect":true,"display_pid_stable":true,"window_id_stable":true,
        "cold_boot_started":true,"ffmpeg_idle_before_probes":true,"ffmpeg_idle_after_probes":true,
        "capture_count":2,"captures":captures
    }));
    Ok(())
}
