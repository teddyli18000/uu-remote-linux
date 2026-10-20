//! Select and activate an Intel virtual RandR output on the existing Xorg server.
//!
//! This module never starts an X server or changes the caller's environment. Each external
//! command is bounded and receives DISPLAY/XAUTHORITY only in its child environment.

use std::{
    path::Path,
    process::{Command, Output, Stdio},
};

const QUERY_TIMEOUT: &str = "3s";
const ACCEPTANCE_OUTPUT: &str = "DUMMY1";

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
}

impl Snapshot {
    fn parse(text: &str) -> Self {
        let mut outputs = Vec::new();
        for line in text.lines() {
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
        Self { outputs }
    }

    fn output(&self, name: &str, acceptance_output: bool) -> Option<&OutputHeader> {
        self.outputs
            .iter()
            .find(|output| output.name == name && is_virtual_candidate(name, acceptance_output))
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

/// Ensure the requested size on an existing Intel virtual RandR output.
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
    if let Some(selected) = selected_output {
        if !is_virtual_candidate(selected, acceptance_output) {
            return Err("selected output is not an allowed virtual RandR output".into());
        }
    }

    let snapshot = query_snapshot(display, authority)?;

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

    let Some(target) = target else {
        return Err(
            "same-session virtual output unavailable; configure Intel VirtualHeads; Xvfb cannot preserve the desktop".into(),
        );
    };
    let current = snapshot.output(&target, acceptance_output);
    if let Some(current_size) = current
        .filter(|output| output.connected)
        .and_then(|output| output.active_size)
        .filter(|size| size.is_supported())
    {
        if current.is_some_and(|output| output.primary && output.at_origin) {
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
            return Ok(Some(target));
        }
        return Err("cannot keep the active virtual output and its current size as primary".into());
    }

    let preferred_mode = preferred.mode_name();
    let set_preferred = output_mode_args(&target, &preferred_mode);
    if run_xrandr(&set_preferred, display, authority).is_ok_and(|output| output.status.success())
        && verify_output(display, authority, &target, acceptance_output, preferred)?
    {
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
        Ok(Some(target))
    } else {
        Err(format!(
            "cannot activate {}x{} on the same-session virtual output; verify Intel VirtualHeads and XRandR mode support",
            preferred.width, preferred.height
        ))
    }
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
    Ok(Snapshot::parse(&String::from_utf8_lossy(&query.stdout)))
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
