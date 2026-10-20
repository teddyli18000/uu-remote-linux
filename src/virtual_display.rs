//! Persistent virtual output in the existing Xorg session; separate Xvfb is opt-in.
//!
//! Call `prepare_environment` and `prepare_startup` from the synchronous entry point before
//! starting Tokio or GTK. Runtime callers can poll `start_if_disconnected`; the selected display
//! is available through `display_config` and is retained after a monitor reconnects.

use std::{
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex, OnceLock, RwLock},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const SUPERVISOR: &str = include_str!("../contrib/virtual-desktop/uu-virtual-desktop");
const DISCONNECTED_SAMPLES: u8 = 2;
const STARTUP_SAMPLE_DELAY: Duration = Duration::from_millis(250);
const START_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayConfig {
    pub display: String,
    pub authority: PathBuf,
}

impl DisplayConfig {
    /// Publish this display only after desktop peers have been closed and the input lease released.
    pub fn activate(&self) {
        *GLOBAL_DISPLAY
            .get_or_init(|| RwLock::new(None))
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(self.clone());
    }

    /// Set a child command's X11 environment without changing this process's environment.
    pub fn apply_to_command<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        command
            .env("DISPLAY", &self.display)
            .env("XAUTHORITY", &self.authority)
            .env_remove("WAYLAND_DISPLAY")
            .env("XDG_SESSION_TYPE", "x11")
    }

    /// Apply the display to the current process. Only call from the synchronous startup path,
    /// before creating any threads or a Tokio runtime.
    pub fn apply_to_process_environment(&self) {
        std::env::set_var("DISPLAY", &self.display);
        std::env::set_var("XAUTHORITY", &self.authority);
        std::env::remove_var("WAYLAND_DISPLAY");
        std::env::set_var("XDG_SESSION_TYPE", "x11");
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MonitorStatus {
    Connected,
    Disconnected,
    Unknown,
}

struct Supervisor {
    child: Child,
    process_group: i32,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        // The shell supervisor, dbus-run-session, the desktop session, and Xvfb share this
        // dedicated process group. Stop the full tree so descendants cannot keep inherited
        // stdout/stderr pipes open or survive a cold-start helper/service restart.
        signal_process_group(self.process_group, libc::SIGTERM);
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            let child_exited = self.child.try_wait().ok().flatten().is_some();
            if child_exited && !process_group_exists(self.process_group) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
        signal_process_group(self.process_group, libc::SIGKILL);
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn signal_process_group(process_group: i32, signal: i32) {
    unsafe {
        libc::kill(-process_group, signal);
    }
}

fn process_group_exists(process_group: i32) -> bool {
    if unsafe { libc::kill(-process_group, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

struct State {
    disconnected_samples: u8,
    config: Option<DisplayConfig>,
    supervisor: Option<Supervisor>,
    supervisor_failure: Option<String>,
    runtime_dir: Option<PathBuf>,
    owned_runtime_dir: bool,
    authority: Option<PathBuf>,
    owned_authority: bool,
    output: Option<String>,
}

impl State {
    fn new() -> Self {
        Self {
            disconnected_samples: 0,
            config: None,
            supervisor: None,
            supervisor_failure: None,
            runtime_dir: None,
            owned_runtime_dir: false,
            authority: None,
            owned_authority: false,
            output: None,
        }
    }
}

impl Drop for State {
    fn drop(&mut self) {
        // Stop the supervisor before removing credentials it may still need during cleanup.
        drop(self.supervisor.take());
        if self.owned_authority {
            if let Some(path) = &self.authority {
                let _ = fs::remove_file(path);
            }
        }
        if self.owned_runtime_dir {
            if let Some(path) = &self.runtime_dir {
                let _ = fs::remove_dir(path);
            }
        }
    }
}

/// One automatic display lifecycle. `with_drm_root` is intended for the explicit acceptance
/// helper, which passes a fixture path as an argument; production always uses `/sys/class/drm`.
#[derive(Clone)]
pub struct VirtualDisplayManager {
    drm_root: PathBuf,
    state: Arc<Mutex<State>>,
    same_session: bool,
    fixture_output: Option<String>,
}

impl VirtualDisplayManager {
    pub fn new() -> Self {
        Self::with_options(PathBuf::from("/sys/class/drm"))
    }

    pub fn with_drm_root(path: impl Into<PathBuf>) -> Self {
        Self::with_options(path.into())
    }

    fn with_options(drm_root: PathBuf) -> Self {
        Self {
            drm_root,
            state: Arc::new(Mutex::new(State::new())),
            same_session: std::env::var("UU_VIRTUAL_DISPLAY_MODE").as_deref() != Ok("xvfb"),
            fixture_output: None,
        }
    }

    /// Only the explicit account-free acceptance command may supply a synthetic output.
    pub fn with_same_session_fixture(path: impl Into<PathBuf>, output: &str) -> Self {
        let mut manager = Self::with_options(path.into());
        manager.same_session = true;
        manager.fixture_output = Some(output.to_owned());
        manager
    }

    /// Make a private copy of the current Xauthority records and publish it before threads start.
    /// This lets the long-lived host use the same file after Xvfb adds its cookie.
    pub fn prepare_environment(&self) -> Result<(), String> {
        if !backend_eligible() {
            return Ok(());
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.authority.is_some() {
            return Ok(());
        }

        if self.same_session {
            // Keep the real GDM authority path: it may be created/refreshed after host startup.
            state.authority = Some(
                std::env::var_os("XAUTHORITY")
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".Xauthority"))
                    })
                    .unwrap_or_default(),
            );
            return Ok(());
        }

        let (runtime_dir, owned_runtime_dir) = runtime_directory()?;
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let authority = runtime_dir.join(format!(
            "uu-remote-host-{}-{nonce}.Xauthority",
            std::process::id()
        ));
        let source = std::env::var_os("XAUTHORITY")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".Xauthority"))
            });
        let contents = match source.as_deref() {
            Some(path) if path != authority && path.is_file() => {
                fs::read(path).map_err(|e| format!("cannot read existing Xauthority file: {e}"))?
            }
            _ => Vec::new(),
        };
        write_private_file(&authority, &contents)?;

        // This function is called before the runtime and graphical threads are created.
        std::env::set_var("XAUTHORITY", &authority);
        state.runtime_dir = Some(runtime_dir);
        state.owned_runtime_dir = owned_runtime_dir;
        state.authority = Some(authority);
        state.owned_authority = true;
        Ok(())
    }

    /// Synchronous startup pass. Two separated reads debounce transient connector changes.
    /// If a virtual X server is already selected, it is adopted and left running.
    pub fn prepare_startup(&self) -> Result<Option<DisplayConfig>, String> {
        self.prepare_environment()?;
        let first = self.poll()?;
        if first.is_some() {
            if let Some(config) = &first {
                config.apply_to_process_environment();
                config.activate();
            }
            return Ok(first);
        }
        thread::sleep(STARTUP_SAMPLE_DELAY);
        let second = self.poll()?;
        if let Some(config) = &second {
            config.apply_to_process_environment();
            config.activate();
        }
        Ok(second)
    }

    /// One synchronous sample, also used by the acceptance helper with its explicit DRM fixture.
    pub fn poll(&self) -> Result<Option<DisplayConfig>, String> {
        if backend_eligible()
            && self
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .authority
                .is_none()
        {
            return Err("call prepare_environment before polling the virtual display".into());
        }
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.poll_locked(&mut state)
    }

    /// Runtime-safe poll. Potentially blocking process and filesystem work runs on Tokio's
    /// blocking pool; it never mutates this process's environment.
    pub async fn start_if_disconnected(&self) -> Result<Option<DisplayConfig>, String> {
        let manager = self.clone();
        tokio::task::spawn_blocking(move || manager.poll())
            .await
            .map_err(|e| format!("virtual display poll failed: {e}"))?
    }

    pub fn selected_display(&self) -> Option<DisplayConfig> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .config
            .clone()
    }

    /// Release the supervisor and private files when the owning host loop ends.
    pub fn shutdown(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let previous = state.config.clone();
        state.config = None;
        state.output = None;
        state.disconnected_samples = 0;
        state.supervisor_failure = None;
        drop(state.supervisor.take());
        if state.owned_authority {
            if let Some(path) = state.authority.take() {
                let _ = fs::remove_file(path);
            }
            state.owned_authority = false;
        }
        if state.owned_runtime_dir {
            if let Some(path) = state.runtime_dir.take() {
                let _ = fs::remove_dir(path);
            }
            state.owned_runtime_dir = false;
        }
        state.authority = None;
        state.runtime_dir = None;
        if let (Some(previous), Some(active)) = (previous, GLOBAL_DISPLAY.get()) {
            let mut active = active.write().unwrap_or_else(|e| e.into_inner());
            if active.as_ref() == Some(&previous) {
                *active = None;
            }
        }
    }

    fn poll_locked(&self, state: &mut State) -> Result<Option<DisplayConfig>, String> {
        if !backend_eligible() {
            state.disconnected_samples = 0;
            return Ok(None);
        }
        if state.authority.is_none() {
            return Err("call prepare_environment before polling the virtual display".into());
        }
        if self.same_session {
            return self.poll_same_session(state);
        }
        if let Some(error) = &state.supervisor_failure {
            return Err(error.clone());
        }
        let child_status = state
            .supervisor
            .as_mut()
            .map(|supervisor| supervisor.child.try_wait())
            .transpose()
            .map_err(|e| format!("cannot inspect Xvfb supervisor: {e}"))?
            .flatten();
        if let Some(status) = child_status {
            let error = format!("virtual display supervisor exited with {status}");
            // Keep both the selected config and the exited Child handle until host shutdown.
            // Existing Xlib objects may still hold this display; silently recreating it would
            // give new operations a different server while those objects remain bound to the old one.
            state.supervisor_failure = Some(error.clone());
            return Err(error);
        }
        if let Some(config) = &state.config {
            return Ok(Some(config.clone()));
        }

        if let Some(config) = self.current_virtual_display(state)? {
            state.disconnected_samples = DISCONNECTED_SAMPLES;
            state.config = Some(config.clone());
            return Ok(Some(config));
        }

        match physical_monitor_status(&self.drm_root) {
            MonitorStatus::Connected | MonitorStatus::Unknown => {
                state.disconnected_samples = 0;
                Ok(None)
            }
            MonitorStatus::Disconnected => {
                state.disconnected_samples = state.disconnected_samples.saturating_add(1);
                if state.disconnected_samples < DISCONNECTED_SAMPLES {
                    return Ok(None);
                }
                let config = self.start_supervisor(state)?;
                state.config = Some(config.clone());
                Ok(Some(config))
            }
        }
    }

    fn poll_same_session(&self, state: &mut State) -> Result<Option<DisplayConfig>, String> {
        if state.output.is_none() {
            let persistent = self.fixture_output.is_none()
                && crate::same_session_display::has_persistent_kernel_output()?;
            let status = physical_monitor_status(&self.drm_root);
            if !persistent && status != MonitorStatus::Disconnected {
                state.disconnected_samples = 0;
                return Ok(None);
            }
            state.disconnected_samples = state.disconnected_samples.saturating_add(1);
            if state.disconnected_samples < DISCONNECTED_SAMPLES {
                return Ok(None);
            }
        }
        let authority = state.authority.as_ref().unwrap();
        let displays: Vec<String> = std::env::var("DISPLAY")
            .ok()
            .filter(|s| !s.is_empty())
            .map(|display| vec![display])
            .unwrap_or_else(|| vec![":0".into(), ":1".into()]);
        let size = std::env::var("UU_VIRTUAL_SIZE").unwrap_or_else(|_| "1024x768".into());
        let mut failure = String::new();
        for display in displays {
            match crate::same_session_display::ensure(
                &display,
                Some(authority),
                &size,
                state.output.as_deref(),
                self.fixture_output.as_deref(),
            ) {
                Ok(Some(output)) => {
                    state.output = Some(output);
                    let config = DisplayConfig {
                        display,
                        authority: authority.clone(),
                    };
                    state.config = Some(config.clone());
                    return Ok(Some(config));
                }
                Ok(None) => return Ok(None),
                Err(error) => failure = error,
            }
        }
        Err(failure)
    }

    fn current_virtual_display(&self, state: &State) -> Result<Option<DisplayConfig>, String> {
        let Some(display) = std::env::var_os("DISPLAY") else {
            return Ok(None);
        };
        let display = display.to_string_lossy().into_owned();
        let Some(number) = local_display_number(&display) else {
            return Ok(None);
        };
        if !xvfb_owns_display(number) {
            return Ok(None);
        }
        let authority = state
            .authority
            .clone()
            .or_else(|| std::env::var_os("XAUTHORITY").map(PathBuf::from))
            .ok_or_else(|| "existing Xvfb display has no Xauthority path".to_owned())?;
        // The capture engine needs an actual MIT cookie, so do not adopt an unauthenticated
        // server even when xdpyinfo happens to connect to it with access control disabled.
        if !authority_has_cookie(&authority, number) || !display_ready(&display, &authority) {
            return Ok(None);
        }
        Ok(Some(DisplayConfig { display, authority }))
    }

    fn start_supervisor(&self, state: &mut State) -> Result<DisplayConfig, String> {
        let runtime_dir = state
            .runtime_dir
            .clone()
            .ok_or("virtual display runtime directory is unavailable")?;
        let authority = state
            .authority
            .clone()
            .ok_or("private Xauthority file is unavailable")?;
        check_dependencies()?;
        let display = choose_display(std::env::var_os("UU_VIRTUAL_DISPLAY"))?;
        let number = local_display_number(&display).ok_or("invalid virtual display number")?;
        let supervisor_authority = runtime_dir.join(format!("uu-virtual-{number}.Xauthority"));

        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(SUPERVISOR)
            .env("UU_VIRTUAL_DISPLAY", &display)
            .env("UU_VIRTUAL_BUS", "private")
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env_remove("NOTIFY_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        if std::env::var_os("UU_VIRTUAL_SESSION").is_none() && program_available("openbox-session")
        {
            command.env("UU_VIRTUAL_SESSION", "openbox-session");
        }
        let child = command.spawn().map_err(|e| {
            format!("cannot start embedded Xvfb supervisor ({e}); install xvfb, xauth, x11-utils, dbus-x11, and openbox")
        })?;
        let process_group = child.id() as i32;
        let mut supervisor = Supervisor {
            child,
            process_group,
        };
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Some(status) = supervisor
                .child
                .try_wait()
                .map_err(|e| format!("cannot inspect Xvfb supervisor: {e}"))?
            {
                return Err(format!("embedded Xvfb supervisor exited with {status}; install xvfb, xauth, x11-utils, dbus-x11, and openbox"));
            }
            if display_ready(&display, &supervisor_authority) {
                merge_authority(&authority, &supervisor_authority, &display)?;
                state.supervisor = Some(supervisor);
                return Ok(DisplayConfig { display, authority });
            }
            if Instant::now() >= deadline {
                return Err("embedded Xvfb supervisor did not create a ready display; install xvfb, xauth, x11-utils, dbus-x11, and openbox".into());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

static GLOBAL_MANAGER: OnceLock<VirtualDisplayManager> = OnceLock::new();
static GLOBAL_DISPLAY: OnceLock<RwLock<Option<DisplayConfig>>> = OnceLock::new();

fn global_manager() -> &'static VirtualDisplayManager {
    GLOBAL_MANAGER
        .get_or_init(|| VirtualDisplayManager::with_options(PathBuf::from("/sys/class/drm")))
}

/// Process-wide accessor consumed by capture, cursor, and subprocess launch paths.
pub fn display_config() -> Option<DisplayConfig> {
    GLOBAL_DISPLAY
        .get()
        .and_then(|v| v.read().ok().and_then(|config| config.clone()))
}

pub fn same_session_mode() -> bool {
    global_manager().same_session
}

/// Prepare the private authority file from the synchronous process entry point.
pub fn prepare_environment() -> Result<(), String> {
    global_manager().prepare_environment()
}

/// Check for an unplugged monitor before the runtime and GTK threads start.
pub fn prepare_startup() -> Result<Option<DisplayConfig>, String> {
    global_manager().prepare_startup()
}

/// Runtime poll for the production `/sys/class/drm` tree.
pub async fn start_if_disconnected() -> Result<Option<DisplayConfig>, String> {
    global_manager().start_if_disconnected().await
}

/// Stop the host-owned display and remove its private credentials on normal shutdown.
pub fn shutdown() {
    global_manager().shutdown();
}

fn backend_eligible() -> bool {
    backend_eligible_for(
        std::env::var("UU_DESKTOP_BACKEND").ok().as_deref(),
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
        std::env::var_os("WAYLAND_DISPLAY").is_some(),
        std::env::var("UU_AUTO_VIRTUAL_DISPLAY").ok().as_deref() != Some("0"),
    )
}

fn backend_eligible_for(
    explicit: Option<&str>,
    session: Option<&str>,
    wayland_socket: bool,
    enabled: bool,
) -> bool {
    if !enabled {
        return false;
    }
    match explicit {
        Some("x11") => true,
        None | Some("auto") => session != Some("wayland") && !wayland_socket,
        _ => false,
    }
}

fn physical_monitor_status(drm_root: &Path) -> MonitorStatus {
    if let Some(connected) = probe_drm(drm_root) {
        return if connected {
            MonitorStatus::Connected
        } else {
            MonitorStatus::Disconnected
        };
    }
    probe_xrandr().unwrap_or(MonitorStatus::Unknown)
}

/// `Some(true/false)` means every physical connector status was readable and conclusive;
/// `None` asks the caller to try xrandr. DRM `status` is a hotplug state, independent of DPMS.
fn probe_drm(root: &Path) -> Option<bool> {
    let entries = fs::read_dir(root).ok()?;
    let mut found = false;
    let mut connected = false;
    let mut uncertain = false;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        match drm_connector_kind(&name) {
            ConnectorKind::Physical => {
                found = true;
                match fs::read_to_string(entry.path().join("status")) {
                    Ok(status) if status.trim() == "connected" => connected = true,
                    Ok(status) if status.trim() == "disconnected" => {}
                    _ => uncertain = true,
                }
            }
            ConnectorKind::Unknown => uncertain = true,
            ConnectorKind::Virtual => {}
        }
    }
    if connected {
        Some(true)
    } else if found && !uncertain {
        Some(false)
    } else {
        None
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectorKind {
    Physical,
    Virtual,
    Unknown,
}

fn drm_connector_kind(name: &str) -> ConnectorKind {
    let Some(rest) = name.strip_prefix("card") else {
        return ConnectorKind::Virtual;
    };
    let Some((card, connector)) = rest.split_once('-') else {
        return if !rest.is_empty() && rest.bytes().all(|c| c.is_ascii_digit()) {
            ConnectorKind::Virtual
        } else {
            ConnectorKind::Unknown
        };
    };
    if card.is_empty() || !card.bytes().all(|c| c.is_ascii_digit()) || connector.is_empty() {
        return ConnectorKind::Unknown;
    }
    classify_connector(connector)
}

fn classify_connector(name: &str) -> ConnectorKind {
    let lower = name.to_ascii_lowercase();
    if lower
        .strip_prefix("virtual")
        .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
    {
        return ConnectorKind::Virtual;
    }
    if ["virtual-", "writeback-"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
    {
        return ConnectorKind::Virtual;
    }
    if [
        "vga-",
        "dvi-",
        "composite-",
        "svideo-",
        "lvds-",
        "component-",
        "din-",
        "dp-",
        "hdmi-",
        "tv-",
        "edp-",
        "dsi-",
        "dpi-",
        "usb-",
    ]
    .iter()
    .any(|prefix| lower.starts_with(prefix))
    {
        ConnectorKind::Physical
    } else {
        ConnectorKind::Unknown
    }
}

fn probe_xrandr() -> Option<MonitorStatus> {
    let display = std::env::var_os("DISPLAY")?;
    let mut command = Command::new("timeout");
    command
        .args(["2s", "xrandr", "--query"])
        .env("DISPLAY", display)
        .env("LC_ALL", "C");
    if let Some(authority) = std::env::var_os("XAUTHORITY") {
        command.env("XAUTHORITY", authority);
    }
    let result = command.output().ok()?;
    if !result.status.success() {
        return None;
    }
    parse_xrandr(&String::from_utf8_lossy(&result.stdout))
}

fn parse_xrandr(output: &str) -> Option<MonitorStatus> {
    let mut found = false;
    let mut connected = false;
    let mut uncertain = false;
    for line in output.lines() {
        let mut words = line.split_whitespace();
        let Some(name) = words.next() else { continue };
        let Some(status) = words.next() else { continue };
        match classify_connector(name) {
            ConnectorKind::Physical => {
                found = true;
                match status {
                    "connected" => connected = true,
                    "disconnected" => {}
                    _ => uncertain = true,
                }
            }
            ConnectorKind::Unknown => {
                // xrandr metadata lines do not use these status tokens. An unfamiliar output
                // may be a physical connector, so do not conclude that all monitors are absent.
                if matches!(status, "connected" | "disconnected") {
                    uncertain = true;
                }
            }
            ConnectorKind::Virtual => {}
        }
    }
    if connected {
        Some(MonitorStatus::Connected)
    } else if found && !uncertain {
        Some(MonitorStatus::Disconnected)
    } else {
        None
    }
}

fn local_display_number(display: &str) -> Option<u32> {
    let value = display
        .strip_prefix("unix:")
        .or_else(|| display.strip_prefix(':'))?;
    let digits = value.split('.').next()?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

fn xvfb_owns_display(number: u32) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Ok(processes) = fs::read_dir("/proc") else {
        return false;
    };
    for process in processes.flatten() {
        let Some(pid) = process.file_name().to_string_lossy().parse::<u32>().ok() else {
            continue;
        };
        let Ok(cmdline) = fs::read(format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        let args: Vec<_> = cmdline
            .split(|b| *b == 0)
            .filter(|arg| !arg.is_empty())
            .collect();
        let executable = args.first().map(|s| {
            Path::new(std::ffi::OsStr::from_bytes(s))
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        });
        if executable.as_deref() != Some("Xvfb") {
            continue;
        }
        if args
            .iter()
            .skip(1)
            .any(|arg| std::str::from_utf8(arg).ok().and_then(local_display_number) == Some(number))
        {
            return true;
        }
    }
    false
}

fn runtime_directory() -> Result<(PathBuf, bool), String> {
    if let Some(path) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        if path.is_dir() {
            return Ok((path, false));
        }
    }
    let uid = unsafe { libc::geteuid() };
    let system = PathBuf::from(format!("/run/user/{uid}"));
    if system.is_dir() {
        return Ok((system, false));
    }
    let fallback = std::env::temp_dir().join(format!("uuremote-runtime-{uid}"));
    match fs::create_dir(&fallback) {
        Ok(()) => {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fallback, fs::Permissions::from_mode(0o700))
                .map_err(|e| format!("cannot secure virtual display runtime directory: {e}"))?;
            Ok((fallback, true))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::metadata(&fallback)
                .map_err(|e| format!("cannot inspect virtual display runtime directory: {e}"))?;
            use std::os::unix::fs::MetadataExt;
            use std::os::unix::fs::PermissionsExt;
            if metadata.uid() != uid || metadata.permissions().mode() & 0o077 != 0 {
                return Err(
                    "virtual display runtime directory must be owned by this user and private"
                        .into(),
                );
            }
            Ok((fallback, false))
        }
        Err(e) => Err(format!(
            "cannot create virtual display runtime directory: {e}"
        )),
    }
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options
        .open(path)
        .map_err(|e| format!("cannot create private Xauthority file: {e}"))?;
    std::io::Write::write_all(&mut file, bytes)
        .map_err(|e| format!("cannot write private Xauthority file: {e}"))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot secure private Xauthority file: {e}"))?;
    Ok(())
}

fn choose_display(override_display: Option<OsString>) -> Result<String, String> {
    let candidates: Vec<String> = if let Some(value) = override_display {
        let value = value.to_string_lossy().into_owned();
        if local_display_number(&value).is_none() {
            return Err("UU_VIRTUAL_DISPLAY must be a local display such as :99".into());
        }
        vec![value]
    } else {
        std::iter::once(99)
            .chain((90..=119).filter(|n| *n != 99))
            .map(|n| format!(":{n}"))
            .collect()
    };
    for display in candidates {
        let number = local_display_number(&display).expect("candidate is numeric");
        let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
        let lock = PathBuf::from(format!("/tmp/.X{number}-lock"));
        if !socket.exists() && !lock.exists() {
            return Ok(display);
        }
    }
    Err("no free X display is available for automatic fallback".into())
}

fn check_dependencies() -> Result<(), String> {
    let required = ["Xvfb", "xauth", "xdpyinfo", "dbus-run-session"];
    let missing: Vec<_> = required
        .iter()
        .filter(|program| !program_available(program))
        .copied()
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "automatic virtual display needs {}; install xvfb, xauth, x11-utils, and dbus-x11",
            missing.join(", ")
        ));
    }
    if std::env::var_os("UU_VIRTUAL_SESSION").is_none()
        && ![
            "openbox-session",
            "startxfce4",
            "mate-session",
            "startlxqt",
            "xterm",
        ]
        .iter()
        .any(|p| program_available(p))
    {
        return Err("automatic virtual display needs a desktop session; install openbox (openbox-session) or set UU_VIRTUAL_SESSION".into());
    }
    Ok(())
}

fn program_available(program: &str) -> bool {
    Command::new("sh")
        .args(["-c", "command -v \"$1\" >/dev/null 2>&1", "sh", program])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn display_ready(display: &str, authority: &Path) -> bool {
    Command::new("timeout")
        .args(["2s", "xdpyinfo", "-display", display])
        .env("XAUTHORITY", authority)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn authority_has_cookie(authority: &Path, display_number: u32) -> bool {
    fn field<'a>(bytes: &'a [u8], offset: &mut usize) -> Option<&'a [u8]> {
        let length = u16::from_be_bytes(bytes.get(*offset..*offset + 2)?.try_into().ok()?) as usize;
        *offset += 2;
        let end = (*offset).checked_add(length)?;
        let field = bytes.get(*offset..end)?;
        *offset = end;
        Some(field)
    }

    let Ok(bytes) = fs::read(authority) else {
        return false;
    };
    let display_number = display_number.to_string();
    let mut offset = 0;
    let mut best: Option<(u8, bool)> = None;
    while offset + 2 <= bytes.len() {
        let family = u16::from_be_bytes([bytes[offset], bytes[offset + 1]]);
        offset += 2;
        let Some(_address) = field(&bytes, &mut offset) else {
            break;
        };
        let Some(number) = field(&bytes, &mut offset) else {
            break;
        };
        let Some(name) = field(&bytes, &mut offset) else {
            break;
        };
        let Some(data) = field(&bytes, &mut offset) else {
            break;
        };
        let priority = match family {
            0xffff => 2,
            256 => 1,
            _ => 0,
        };
        if priority > 0
            && number == display_number.as_bytes()
            && name == b"MIT-MAGIC-COOKIE-1"
            && best.map_or(true, |(old_priority, _)| priority > old_priority)
        {
            best = Some((priority, !data.is_empty()));
        }
    }
    best.map_or(false, |(_, has_cookie)| has_cookie)
}

fn merge_authority(target: &Path, generated: &Path, display: &str) -> Result<(), String> {
    let output = Command::new("xauth")
        .arg("-f")
        .arg(generated)
        .arg("nlist")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("cannot read generated Xvfb authority: {e}"))?;
    if !output.status.success() || output.stdout.is_empty() {
        return Err("Xvfb did not create an Xauthority cookie".into());
    }
    let removed = Command::new("xauth")
        .arg("-f")
        .arg(target)
        .arg("remove")
        .arg(display)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("cannot update private Xauthority file: {e}"))?;
    if !removed.success() {
        return Err("cannot replace a stale Xauthority cookie for the virtual display".into());
    }
    let mut merge = Command::new("xauth")
        .arg("-f")
        .arg(target)
        .args(["nmerge", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot merge Xvfb authority cookie: {e}"))?;
    if let Some(mut stdin) = merge.stdin.take() {
        std::io::Write::write_all(&mut stdin, &output.stdout)
            .map_err(|e| format!("cannot pass Xvfb authority cookie: {e}"))?;
    }
    let status = merge
        .wait()
        .map_err(|e| format!("cannot finish Xauthority merge: {e}"))?;
    if !status.success() {
        return Err("cannot merge Xvfb authority cookie".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    fn fixture() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "uu-drm-fixture-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn connector(root: &Path, name: &str, status: &str) {
        let path = root.join(name);
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("status"), status).unwrap();
    }

    #[test]
    fn drm_reports_connected_physical_connector_even_when_dpms_is_irrelevant() {
        let root = fixture();
        connector(&root, "card0-DP-1", "connected\n");
        assert_eq!(probe_drm(&root), Some(true));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn disconnected_physical_connector_is_conclusive() {
        let root = fixture();
        fs::create_dir_all(root.join("card0")).unwrap();
        fs::create_dir_all(root.join("renderD128")).unwrap();
        connector(&root, "card0-HDMI-A-1", "disconnected\n");
        assert_eq!(probe_drm(&root), Some(false));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn virtual_and_writeback_connectors_do_not_count_as_physical_monitors() {
        let root = fixture();
        connector(&root, "card0-Virtual-1", "connected\n");
        connector(&root, "card0-Writeback-1", "connected\n");
        assert_eq!(probe_drm(&root), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn unreadable_or_unknown_drm_state_is_not_treated_as_unplugged() {
        let root = fixture();
        connector(&root, "card0-DP-1", "unknown\n");
        assert_eq!(probe_drm(&root), None);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn xrandr_fallback_recognizes_physical_status_and_ignores_virtual_outputs() {
        assert_eq!(
            parse_xrandr("Screen 0: minimum 8 x 8\nDP-1 connected primary 1920x1080+0+0\n"),
            Some(MonitorStatus::Connected)
        );
        assert_eq!(
            parse_xrandr("HDMI-1 disconnected\n"),
            Some(MonitorStatus::Disconnected)
        );
        assert_eq!(
            parse_xrandr("DVI-1 connected primary 1920x1080+0+0\n"),
            Some(MonitorStatus::Connected)
        );
        assert_eq!(parse_xrandr("Virtual-1 connected 1920x1080+0+0\n"), None);
        assert_eq!(
            parse_xrandr("DP-1 disconnected\nMystery-1 connected 1280x720+0+0\n"),
            None
        );
        assert_eq!(
            parse_xrandr("HDMI-1 disconnected\nMystery-1 disconnected\n"),
            None
        );
        assert_eq!(parse_xrandr("Screen 0: minimum 8 x 8\n"), None);
    }

    #[test]
    fn display_number_parser_accepts_local_display_and_rejects_remote_names() {
        assert_eq!(local_display_number(":99"), Some(99));
        assert_eq!(local_display_number(":99.0"), Some(99));
        assert_eq!(local_display_number("unix:12"), Some(12));
        assert_eq!(local_display_number("host:12"), None);
        assert_eq!(local_display_number(":abc"), None);
    }

    #[test]
    fn authority_cookie_must_match_the_display_and_use_mit_magic_cookie() {
        fn record(family: u16, number: &[u8], name: &[u8], data: &[u8]) -> Vec<u8> {
            let mut bytes = family.to_be_bytes().to_vec();
            for field in [&b"localhost"[..], number, name, data] {
                bytes.extend_from_slice(&(field.len() as u16).to_be_bytes());
                bytes.extend_from_slice(field);
            }
            bytes
        }

        let path = fixture().join("auth");
        let _ = fs::create_dir_all(path.parent().unwrap());
        fs::write(
            &path,
            record(1, b"98", b"MIT-MAGIC-COOKIE-1", b"cookie")
                .into_iter()
                .chain(record(0xffff, b"97", b"MIT-MAGIC-COOKIE-1", b"cookie"))
                .chain(record(256, b"99", b"MIT-MAGIC-COOKIE-1", b"cookie"))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(!authority_has_cookie(&path, 98));
        assert!(authority_has_cookie(&path, 97));
        assert!(authority_has_cookie(&path, 99));
        let _ = fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn backend_policy_only_enables_auto_x11_and_explicit_x11() {
        assert!(backend_eligible_for(None, None, false, true));
        assert!(backend_eligible_for(Some("auto"), Some("x11"), false, true));
        assert!(backend_eligible_for(
            Some("x11"),
            Some("wayland"),
            true,
            true
        ));
        assert!(!backend_eligible_for(
            Some("auto"),
            Some("wayland"),
            false,
            true
        ));
        assert!(!backend_eligible_for(Some("auto"), None, true, true));
        for backend in ["none", "wayland", "portal", "kms"] {
            assert!(!backend_eligible_for(Some(backend), None, false, true));
        }
        assert!(!backend_eligible_for(Some("x11"), None, false, false));
    }
}
