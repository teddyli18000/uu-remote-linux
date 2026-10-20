//! Activate an existing RandR output without replacing the Xorg desktop or its driver.
//!
//! This module never starts an X server or changes the caller's environment. Each external
//! command is bounded and receives DISPLAY/XAUTHORITY only in its child environment.

use std::{
    path::Path,
    process::{Command, Output, Stdio},
};

const QUERY_TIMEOUT: &str = "3s";
const ACCEPTANCE_OUTPUT: &str = "DUMMY1";
const KERNEL_HELPER: &str = "/usr/local/libexec/uuremote-display-helper";
static ROOT_GEOMETRY: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static OUTPUT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn output_generation() -> u64 {
    OUTPUT_GENERATION.load(std::sync::atomic::Ordering::Acquire)
}
fn output_changed() {
    OUTPUT_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Release);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Size {
    width: u32,
    height: u32,
}

impl Size {
    fn is_supported(self) -> bool {
        (320..=8192).contains(&self.width) && (200..=8192).contains(&self.height)
    }

    fn parse(value: &str) -> Result<Self, String> {
        let Some((width, height)) = value.split_once('x') else {
            return Err("preferred size must be WIDTHxHEIGHT".into());
        };
        if width.is_empty()
            || height.is_empty()
            || !width.bytes().all(|b| b.is_ascii_digit())
            || !height.bytes().all(|b| b.is_ascii_digit())
        {
            return Err("preferred size must contain only numeric WIDTHxHEIGHT".into());
        }
        let width = width
            .parse::<u32>()
            .map_err(|_| "preferred width is outside the supported range")?;
        let height = height
            .parse::<u32>()
            .map_err(|_| "preferred height is outside the supported range")?;
        let size = Self { width, height };
        if !size.is_supported() {
            return Err("preferred size must be between 320x200 and 8192x8192".into());
        }
        Ok(size)
    }

    fn mode_name(self) -> String {
        format!("{}x{}", self.width, self.height)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OutputHeader {
    name: String,
    connected: bool,
    active_size: Option<Size>,
    primary: bool,
    at_origin: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Snapshot {
    outputs: Vec<OutputHeader>,
    root_size: Option<Size>,
}

impl Snapshot {
    fn parse(text: &str) -> Self {
        let mut outputs = Vec::new();
        let mut root_size = None;
        for line in text.lines() {
            if line.starts_with("Screen ") {
                if let Some((_, current)) = line.split_once("current ") {
                    let parts: Vec<_> = current.split_whitespace().collect();
                    if parts.len() >= 3 && parts[1] == "x" {
                        root_size = Size::parse(&format!(
                            "{}x{}",
                            parts[0],
                            parts[2].trim_end_matches(',')
                        ))
                        .ok();
                    }
                }
            }
            let mut words = line.split_whitespace();
            let Some(name) = words.next() else { continue };
            let Some(status) = words.next() else { continue };
            if status != "connected" && status != "disconnected" {
                continue;
            }

            let rest: Vec<_> = words.collect();
            let geometry = rest.iter().find_map(|word| parse_geometry(word));
            outputs.push(OutputHeader {
                name: name.to_owned(),
                connected: status == "connected",
                active_size: geometry.map(|g| g.0),
                primary: rest.contains(&"primary"),
                at_origin: geometry.is_some_and(|g| g.1),
            });
        }
        Self { outputs, root_size }
    }

    fn output(&self, name: &str, acceptance_output: bool) -> Option<&OutputHeader> {
        self.outputs.iter().find(|output| {
            output.name == name
                && (is_virtual_candidate(name, acceptance_output) || hdmi_number(name).is_some())
        })
    }

    fn active_virtual(&self, acceptance_output: bool) -> Option<&OutputHeader> {
        self.outputs.iter().find(|output| {
            output.connected
                && output.active_size.is_some_and(Size::is_supported)
                && is_virtual_candidate(&output.name, acceptance_output)
        })
    }

    fn virtual_candidate(&self, acceptance_output: bool) -> Option<&OutputHeader> {
        self.outputs
            .iter()
            .find(|output| is_virtual_candidate(&output.name, acceptance_output))
    }

    fn has_connected_nonvirtual(&self, acceptance_output: bool) -> bool {
        self.outputs.iter().any(|output| {
            output.connected && !is_virtual_candidate(&output.name, acceptance_output)
        })
    }

    fn has_active_nonvirtual(&self, acceptance_output: bool) -> bool {
        self.outputs.iter().any(|output| {
            output.connected
                && output.active_size.is_some()
                && !is_virtual_candidate(&output.name, acceptance_output)
        })
    }
}

/// Keep an existing virtual output or helper-owned HDMI output usable on the original Xorg.
///
/// When `selected_output` is `None`, a connected non-virtual output makes this a no-op (`Ok(None)`)
/// in production so a replug race cannot trigger first-time virtual activation. In acceptance mode,
/// only a non-virtual output with active geometry blocks activation because Xorg's dummy driver
/// keeps powered-off outputs marked connected. An existing active virtual output is retained
/// across replugging. The caller is responsible for the DRM connector decision.
pub fn ensure(
    display: &str,
    authority: Option<&Path>,
    preferred_size: &str,
    selected_output: Option<&str>,
    fixture_output: Option<&str>,
) -> Result<Option<String>, String> {
    if fixture_output.is_some_and(|output| output != ACCEPTANCE_OUTPUT) {
        return Err("fixture output must be the explicit DUMMY1 fixture".into());
    }
    let acceptance_output = fixture_output.is_some();
    validate_display(display)?;
    let preferred = Size::parse(preferred_size)?;
    let mut snapshot = query_snapshot(display, authority)?;
    let owned = if !acceptance_output
        && (selected_output.is_some_and(|name| !is_virtual_candidate(name, false))
            || snapshot.virtual_candidate(false).is_none())
    {
        helper_connector("status")?
    } else {
        None
    };
    let mut owned_output = owned
        .as_deref()
        .and_then(|connector| map_connector(&snapshot, connector));
    if owned_output.as_deref().is_some_and(|name| {
        snapshot
            .output(name, false)
            .is_some_and(|output| !output.connected)
    }) {
        // Recover only the previously recorded connector, never select another port.
        helper_connector("ensure")?;
        snapshot = query_snapshot(display, authority)?;
        owned_output = owned
            .as_deref()
            .and_then(|connector| map_connector(&snapshot, connector));
    }
    if let Some(selected) = selected_output {
        if !is_virtual_candidate(selected, acceptance_output)
            && owned_output.as_deref() != Some(selected)
        {
            return Err("selected HDMI output is not owned by the runtime display helper".into());
        }
    }

    let target = if let Some(selected) = selected_output {
        snapshot
            .output(selected, acceptance_output)
            .filter(|output| output.connected && output.active_size.is_some_and(Size::is_supported))
            .or_else(|| snapshot.active_virtual(acceptance_output))
            .or_else(|| {
                snapshot
                    .output(selected, acceptance_output)
                    .filter(|output| output.connected)
            })
            .or_else(|| snapshot.output(selected, acceptance_output))
            .or_else(|| snapshot.virtual_candidate(acceptance_output))
            .map(|output| output.name.clone())
    } else if let Some(owned) = owned_output {
        Some(owned)
    } else if let Some(active) = snapshot.active_virtual(acceptance_output) {
        Some(active.name.clone())
    } else if if acceptance_output {
        snapshot.has_active_nonvirtual(acceptance_output)
    } else {
        snapshot.has_connected_nonvirtual(acceptance_output)
    } {
        return Ok(None);
    } else {
        snapshot
            .virtual_candidate(acceptance_output)
            .map(|output| output.name.clone())
    };

    let (target, newly_forced) = if let Some(target) = target {
        (target, false)
    } else {
        if acceptance_output
            || !snapshot
                .outputs
                .iter()
                .any(|output| hdmi_number(&output.name).is_some())
        {
            return Err(
                "no existing virtual or HDMI output is available on this Xorg desktop".into(),
            );
        }
        let connector =
            helper_connector("ensure")?.ok_or("physical monitor appeared; no port was forced")?;
        snapshot = match query_snapshot(display, authority) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let _ = helper_connector("release");
                return Err(error);
            }
        };
        let Some(target) = map_connector(&snapshot, &connector) else {
            let _ = helper_connector("release");
            return Err("forced HDMI connector is not exposed by the original Xorg desktop".into());
        };
        (target, true)
    };
    let mut rollback = KernelRollback(newly_forced);
    let current = snapshot.output(&target, acceptance_output);
    if let Some(current_size) = current
        .filter(|output| output.connected)
        .and_then(|output| output.active_size)
        .filter(|size| size.is_supported())
    {
        if current.is_some_and(|output| output.primary && output.at_origin) {
            if newly_forced {
                output_changed();
            }
            rollback.0 = false;
            return Ok(Some(target));
        }
        // A replug may deliberately change the user's layout. Keep an already active owned
        // HDMI output in that layout rather than making it primary again on every poll.
        if !newly_forced && hdmi_number(&target).is_some() {
            rollback.0 = false;
            return Ok(Some(target));
        }
        let reposition = [
            "--output".into(),
            target.clone(),
            "--primary".into(),
            "--pos".into(),
            "0x0".into(),
        ];
        if run_xrandr(&reposition, display, authority).is_ok_and(|output| output.status.success())
            && verify_output(display, authority, &target, acceptance_output, current_size)?
        {
            output_changed();
            rollback.0 = false;
            return Ok(Some(target));
        }
        return Err("cannot keep the active virtual output and its current size as primary".into());
    }

    // Use the connector's existing advertised modes. The target i915 kernel supplies
    // 1024x768 without EDID; arbitrary CVT timings can exceed a sinkless HDMI clock limit.
    if hdmi_number(&target).is_some() {
        let args = vec![
            "--output".into(),
            target.clone(),
            "--auto".into(),
            "--primary".into(),
            "--pos".into(),
            "0x0".into(),
        ];
        if run_xrandr(&args, display, authority).is_ok_and(|output| output.status.success()) {
            let activated = query_snapshot(display, authority)?;
            if activated
                .output(&target, acceptance_output)
                .is_some_and(|output| {
                    output.connected
                        && output.primary
                        && output.at_origin
                        && output.active_size.is_some_and(Size::is_supported)
                })
            {
                output_changed();
                rollback.0 = false;
                return Ok(Some(target));
            }
        }
        return Err("cannot activate an advertised mode on the helper-owned HDMI output".into());
    }

    let preferred_mode = preferred.mode_name();
    let set_preferred = output_mode_args(&target, &preferred_mode);
    if run_xrandr(&set_preferred, display, authority).is_ok_and(|output| output.status.success())
        && verify_output(display, authority, &target, acceptance_output, preferred)?
    {
        output_changed();
        rollback.0 = false;
        return Ok(Some(target));
    }

    let modeline = query_cvt(preferred)?;
    let custom_mode = format!("uu-remote-{}x{}-60", preferred.width, preferred.height);
    let mut new_mode = vec!["--newmode".into(), custom_mode.clone()];
    new_mode.extend(modeline);
    // The deterministic mode name can already exist after a prior run. Let the final mode set
    // decide whether the existing server-side mode is usable.
    let _ = run_xrandr(&new_mode, display, authority);

    let add_mode = ["--addmode".into(), target.clone(), custom_mode.clone()];
    let _ = run_xrandr(&add_mode, display, authority);
    let set_custom = output_mode_args(&target, &custom_mode);
    if run_xrandr(&set_custom, display, authority).is_ok_and(|output| output.status.success())
        && verify_output(display, authority, &target, acceptance_output, preferred)?
    {
        output_changed();
        rollback.0 = false;
        Ok(Some(target))
    } else {
        Err(format!(
            "cannot activate {}x{} on the original Xorg output; runtime HDMI force or XRandR mode activation is unsupported",
            preferred.width, preferred.height
        ))
    }
}

// Restore only a port newly forced by this activation attempt if Xorg cannot use it.
struct KernelRollback(bool);
impl Drop for KernelRollback {
    fn drop(&mut self) {
        if self.0 {
            let _ = helper_connector("release");
        }
    }
}

/// Adopt only a connector recorded by the privileged helper, including after a host restart.
pub fn has_persistent_kernel_output() -> Result<bool, String> {
    Ok(helper_connector("status")?.is_some())
}

fn hdmi_number(name: &str) -> Option<&str> {
    let number = name
        .strip_prefix("HDMI-A-")
        .or_else(|| name.strip_prefix("HDMI-"))
        .or_else(|| name.strip_prefix("HDMI"))?;
    (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())).then_some(number)
}

fn connector_number(connector: &str) -> Option<&str> {
    let (card, output) = connector.split_once('-')?;
    let card = card.strip_prefix("card")?;
    if card.is_empty() || !card.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = output.strip_prefix("HDMI-A-")?;
    (!number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())).then_some(number)
}

fn map_connector(snapshot: &Snapshot, connector: &str) -> Option<String> {
    let number = connector_number(connector)?;
    let mut matching = snapshot
        .outputs
        .iter()
        .filter(|output| hdmi_number(&output.name) == Some(number));
    let name = matching.next()?.name.clone();
    matching.next().is_none().then_some(name)
}

fn helper_connector(action: &str) -> Result<Option<String>, String> {
    if !Path::new(KERNEL_HELPER).is_file() {
        return if action == "status" {
            Ok(None)
        } else {
            Err("runtime display helper is missing; install the scoped helper from contrib/virtual-desktop (no driver changes)".into())
        };
    }
    let output = run_bounded(KERNEL_HELPER, &[action.into()], None)
        .map_err(|_| "runtime display helper could not run or timed out".to_owned())?;
    if !output.status.success() {
        return Err("runtime display helper refused the operation; check its scoped installation and connector support".into());
    }
    let text = std::str::from_utf8(&output.stdout)
        .map_err(|_| "invalid runtime display helper reply")?
        .trim();
    if text.is_empty() {
        return Ok(None);
    }
    if connector_number(text).is_none() || text.len() > 80 {
        return Err("invalid runtime display helper connector".into());
    }
    Ok(Some(text.to_owned()))
}

fn query_snapshot(display: &str, authority: Option<&Path>) -> Result<Snapshot, String> {
    let query = run_xrandr(&["--query".into()], display, authority).map_err(|_| {
        "cannot query same-session XRandR outputs; check the display and Xauthority".to_string()
    })?;
    if !query.status.success() {
        return Err(
            "cannot query same-session XRandR outputs; check the display and Xauthority".into(),
        );
    }
    let snapshot = Snapshot::parse(&String::from_utf8_lossy(&query.stdout));
    if let Some(size) = snapshot.root_size {
        let geometry = (u64::from(size.width) << 32) | u64::from(size.height);
        let previous = ROOT_GEOMETRY.swap(geometry, std::sync::atomic::Ordering::AcqRel);
        if previous != 0 && previous != geometry {
            output_changed();
        }
    }
    Ok(snapshot)
}

fn verify_output(
    display: &str,
    authority: Option<&Path>,
    target: &str,
    acceptance_output: bool,
    expected_size: Size,
) -> Result<bool, String> {
    let snapshot = query_snapshot(display, authority)?;
    Ok(snapshot
        .output(target, acceptance_output)
        .is_some_and(|output| {
            output.connected
                && output.active_size == Some(expected_size)
                && expected_size.is_supported()
                && output.primary
                && output.at_origin
        }))
}

fn output_mode_args(output: &str, mode: &str) -> Vec<String> {
    vec![
        "--output".into(),
        output.into(),
        "--mode".into(),
        mode.into(),
        "--primary".into(),
        "--pos".into(),
        "0x0".into(),
    ]
}

fn query_cvt(size: Size) -> Result<Vec<String>, String> {
    let output = run_bounded(
        "cvt",
        &[size.width.to_string(), size.height.to_string(), "60".into()],
        None,
    )
    .map_err(|_| "cannot run bounded cvt to prepare the requested RandR mode".to_string())?;
    if !output.status.success() {
        return Err("cvt could not prepare the requested RandR mode".into());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("Modeline "))
        .ok_or_else(|| "cvt returned no usable RandR modeline".to_string())?;
    parse_modeline(line)
}

/// Return only the validated clock/timing/flag arguments; the caller supplies the mode name.
fn parse_modeline(line: &str) -> Result<Vec<String>, String> {
    let mut words = line.split_whitespace();
    if words.next() != Some("Modeline") {
        return Err("invalid cvt modeline".into());
    }
    let name = words
        .next()
        .ok_or_else(|| "invalid cvt modeline name".to_string())?;
    let name = name.trim_matches('"');
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return Err("invalid cvt modeline name".into());
    }
    let clock = words
        .next()
        .ok_or_else(|| "invalid cvt modeline clock".to_string())?;
    if clock.is_empty()
        || clock.bytes().filter(|byte| *byte == b'.').count() > 1
        || !clock
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return Err("invalid cvt modeline clock".into());
    }
    let clock_value = clock
        .parse::<f64>()
        .map_err(|_| "invalid cvt modeline clock".to_string())?;
    if !clock_value.is_finite() || !(1.0..=1000.0).contains(&clock_value) {
        return Err("cvt modeline clock is outside the supported range".into());
    }

    let mut args = vec![clock.to_owned()];
    let mut timings = Vec::with_capacity(8);
    for _ in 0..8 {
        let timing = words
            .next()
            .ok_or_else(|| "incomplete cvt modeline timings".to_string())?;
        let value = timing
            .parse::<u32>()
            .map_err(|_| "invalid cvt modeline timing".to_string())?;
        if value == 0 || value > 32768 {
            return Err("cvt modeline timing is outside the supported range".into());
        }
        timings.push(value);
        args.push(timing.to_owned());
    }
    if !(timings[0] < timings[1]
        && timings[1] < timings[2]
        && timings[2] < timings[3]
        && timings[4] < timings[5]
        && timings[5] < timings[6]
        && timings[6] < timings[7])
    {
        return Err("invalid cvt modeline timing order".into());
    }
    for flag in words {
        if !matches!(
            flag,
            "+hsync" | "-hsync" | "+vsync" | "-vsync" | "interlace" | "doublescan"
        ) {
            return Err("invalid cvt modeline flag".into());
        }
        args.push(flag.to_owned());
    }
    Ok(args)
}

fn parse_geometry(token: &str) -> Option<(Size, bool)> {
    let (width, rest) = token.split_once('x')?;
    let height_end = rest.find(['+', '-']).unwrap_or(rest.len());
    let height = &rest[..height_end];
    if width.is_empty()
        || height.is_empty()
        || !width.bytes().all(|b| b.is_ascii_digit())
        || !height.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let size = Size {
        width: width.parse().ok()?,
        height: height.parse().ok()?,
    };
    if size.width == 0 || size.height == 0 {
        return None;
    }
    let origin = &rest[height_end..];
    let at_origin = origin.is_empty() || matches!(origin, "+0+0" | "+0-0" | "-0+0" | "-0-0");
    Some((size, at_origin))
}

fn is_virtual_candidate(name: &str, acceptance_output: bool) -> bool {
    name == "VIRTUAL1"
        || name == "VIRTUAL2"
        || name
            .strip_prefix("VIRTUAL-")
            .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
        || (acceptance_output && name == ACCEPTANCE_OUTPUT)
}

fn validate_display(display: &str) -> Result<(), String> {
    if display.is_empty()
        || display.len() > 255
        || display.starts_with('-')
        || !display
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b':' | b'.' | b'_' | b'-' | b'/'))
    {
        return Err("invalid X display name".into());
    }
    Ok(())
}

fn run_xrandr(args: &[String], display: &str, authority: Option<&Path>) -> std::io::Result<Output> {
    let args = xrandr_args(display, args);
    run_bounded("xrandr", &args, Some((display, authority)))
}

fn xrandr_args(display: &str, args: &[String]) -> Vec<String> {
    let mut full_args = Vec::with_capacity(args.len() + 2);
    full_args.extend(["--display".into(), display.into()]);
    full_args.extend_from_slice(args);
    full_args
}

fn run_bounded(
    program: &str,
    args: &[String],
    x11: Option<(&str, Option<&Path>)>,
) -> std::io::Result<Output> {
    let mut command = Command::new("timeout");
    command
        .args(["--kill-after=1s", QUERY_TIMEOUT, program])
        .args(args)
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    if let Some((display, authority)) = x11 {
        command.env("DISPLAY", display);
        if let Some(authority) = authority {
            command.env("XAUTHORITY", authority);
        }
    }
    command.output()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_randr_output_headers_and_active_geometry() {
        let parsed = Snapshot::parse(
            "Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 8192 x 8192\n\
             eDP-1 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis)\n\
               1920x1080 60.00*+\n\
             VIRTUAL1 connected 1920x1080-1920+0 (normal left inverted right x axis y axis)\n\
               1920x1080 60.00*+\n\
             VIRTUAL2 disconnected (normal left inverted right x axis y axis)\n",
        );

        assert_eq!(
            parsed.root_size,
            Some(Size {
                width: 1920,
                height: 1080
            })
        );
        assert_eq!(parsed.outputs.len(), 3);
        assert_eq!(parsed.outputs[0].name, "eDP-1");
        assert!(parsed.outputs[0].primary);
        assert_eq!(
            parsed.outputs[0].active_size,
            Some(Size {
                width: 1920,
                height: 1080
            })
        );
        assert!(parsed.outputs[0].at_origin);
        assert_eq!(parsed.outputs[1].name, "VIRTUAL1");
        assert!(parsed.outputs[1].connected);
        assert!(!parsed.outputs[1].at_origin);
        assert!(!parsed.outputs[2].connected);
        assert_eq!(parsed.virtual_candidate(false).unwrap().name, "VIRTUAL1");
        assert_eq!(parsed.outputs[2].name, "VIRTUAL2");
    }

    #[test]
    fn kernel_connector_mapping_requires_one_existing_hdmi_output() {
        let snapshot = Snapshot::parse("DP-1 disconnected\nHDMI-1 disconnected\n");
        assert_eq!(
            map_connector(&snapshot, "card0-HDMI-A-1").as_deref(),
            Some("HDMI-1")
        );
        assert!(map_connector(&snapshot, "card0-DP-1").is_none());
        assert!(map_connector(&snapshot, "card0-HDMI-A-1\nother").is_none());
        assert!(map_connector(&snapshot, "../../HDMI-A-1").is_none());
        assert!(map_connector(&snapshot, "card0-HDMI-A-2").is_none());
        let ambiguous = Snapshot::parse("HDMI-1 disconnected\nHDMI1 disconnected\n");
        assert!(map_connector(&ambiguous, "card0-HDMI-A-1").is_none());
    }

    #[test]
    fn production_policy_accepts_only_intel_virtual_names() {
        assert!(is_virtual_candidate("VIRTUAL1", false));
        assert!(is_virtual_candidate("VIRTUAL2", false));
        assert!(is_virtual_candidate("VIRTUAL-1", false));
        assert!(is_virtual_candidate("VIRTUAL-12", false));
        assert!(!is_virtual_candidate("DUMMY1", false));
        assert!(!is_virtual_candidate("HDMI-1", false));
        assert!(!is_virtual_candidate("VIRTUAL1-extra", false));
        assert!(is_virtual_candidate("DUMMY1", true));
        assert!(!is_virtual_candidate("DUMMY2", true));
    }

    #[test]
    fn validates_requested_dimensions_and_modeline_arguments() {
        assert_eq!(
            Size::parse("1920x1080").unwrap(),
            Size {
                width: 1920,
                height: 1080
            }
        );
        assert!(Size::parse("1920x1080;rm").is_err());
        assert!(Size::parse("0x0").is_err());
        assert_eq!(
            Size::parse("1024x768").unwrap(),
            Size {
                width: 1024,
                height: 768
            }
        );
        assert!(Size::parse("9000x1080").is_err());

        let args = parse_modeline(
            "Modeline \"1920x1080_60.00\" 173.00 1920 2048 2248 2576 1080 1083 1088 1120 +hsync -vsync",
        )
        .unwrap();
        assert_eq!(args[0], "173.00");
        assert_eq!(args.len(), 11);
        assert_eq!(args[9], "+hsync");
        assert_eq!(args[10], "-vsync");
        assert!(parse_modeline("Modeline \"bad\" 1.0 1 2 3 4 1 2 3 4;rm").is_err());
    }

    #[test]
    fn preserves_active_virtual_output_even_with_physical_connector() {
        let snapshot = Snapshot::parse(
            "HDMI-1 connected primary 1920x1080+0+0\nVIRTUAL1 connected 1920x1080+0+0\n",
        );
        assert_eq!(snapshot.active_virtual(false).unwrap().name, "VIRTUAL1");
        assert!(snapshot.has_connected_nonvirtual(false));

        let no_active_virtual =
            Snapshot::parse("HDMI-1 connected primary 1920x1080+0+0\nVIRTUAL1 connected\n");
        assert!(no_active_virtual.active_virtual(false).is_none());
        assert!(no_active_virtual.has_connected_nonvirtual(false));
    }

    #[test]
    fn discovers_initially_disconnected_virtual_candidate() {
        let snapshot = Snapshot::parse(
            "eDP-1 disconnected\nVIRTUAL1 disconnected (normal left inverted right x axis y axis)\n",
        );
        assert!(snapshot.active_virtual(false).is_none());
        assert_eq!(snapshot.virtual_candidate(false).unwrap().name, "VIRTUAL1");
        assert!(!snapshot.virtual_candidate(false).unwrap().connected);
    }
}
