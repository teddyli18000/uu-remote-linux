//! Native Linux X11 Capture Engine via XShm

use std::sync::Mutex;

#[cfg(target_os = "linux")]
extern "C" {
    fn x11_init(display_name: *const libc::c_char) -> libc::c_int;
    fn x11_switch_display(display_name: *const libc::c_char) -> libc::c_int;
    fn x11_get_dimensions(out_w: *mut libc::c_int, out_h: *mut libc::c_int) -> libc::c_int;
    fn x11_refresh_geometry() -> libc::c_int;
    fn x11_capture_frame(
        out_buffer: *mut libc::c_uchar,
        out_w: *mut libc::c_int,
        out_h: *mut libc::c_int,
    ) -> libc::c_int;
    fn x11_close();
    fn x11_wake_display() -> libc::c_int;
    fn x11_display_name(buffer: *mut libc::c_char, capacity: usize) -> libc::c_int;
}

#[cfg(not(target_os = "linux"))]
unsafe fn x11_init(_: *const libc::c_char) -> libc::c_int { 0 }
#[cfg(not(target_os = "linux"))]
unsafe fn x11_switch_display(_: *const libc::c_char) -> libc::c_int { 0 }
#[cfg(not(target_os = "linux"))]
unsafe fn x11_get_dimensions(out_w: *mut libc::c_int, out_h: *mut libc::c_int) -> libc::c_int {
    *out_w = 1920;
    *out_h = 1080;
    0
}
#[cfg(not(target_os = "linux"))]
unsafe fn x11_capture_frame(
    out_buffer: *mut libc::c_uchar,
    out_w: *mut libc::c_int,
    out_h: *mut libc::c_int,
) -> libc::c_int {
    *out_w = 1920;
    *out_h = 1080;
    std::ptr::write_bytes(out_buffer, 0, (1920 * 1080 * 4) as usize);
    0
}
#[cfg(not(target_os = "linux"))]
unsafe fn x11_close() {}
#[cfg(not(target_os = "linux"))]
unsafe fn x11_refresh_geometry() -> libc::c_int { 0 }

#[cfg(not(target_os = "linux"))]
unsafe fn x11_display_name(buffer: *mut libc::c_char, capacity: usize) -> libc::c_int {
    if capacity < 3 { return -1; }
    *buffer = b':' as libc::c_char;
    *buffer.add(1) = b'0' as libc::c_char;
    *buffer.add(2) = 0;
    0
}

/// The X11 root can be resized while the host runs (RandR, xrdp reconnects); the
/// geometry read at startup then makes x11grab reject every new connection.
pub fn x11_resized() -> bool {
    match unsafe { x11_refresh_geometry() } {
        1 => true,
        0 => false,
        code => { log::warn!("X11 geometry query failed with code {code}"); false }
    }
}

/// Cached native geometry, refreshed by the display watcher or desktop-offer path.
pub fn x11_dimensions() -> Option<(u32, u32)> {
    let (mut width, mut height) = (0, 0);
    (unsafe { x11_get_dimensions(&mut width, &mut height) } == 0 && width > 0 && height > 0)
        .then_some((width as u32, height as u32))
}

pub struct ScreenCapture {
    pub backend: crate::desktop_backend::DesktopBackend,
    pub init_error: Option<String>,
    pub portal: Option<std::sync::Arc<crate::portal::PortalSession>>,
    pub display: String,
    kms: Option<crate::kms::Capture>,
    pub width: u32,
    pub height: u32,
    buffer: Mutex<Vec<u8>>,
    x11_generation: Option<u64>,
}

impl ScreenCapture {
    pub fn open_portal(cancelled: &std::sync::atomic::AtomicBool) -> Result<Self, crate::portal::PortalError> {
        let session = std::sync::Arc::new(crate::portal::PortalSession::open_cancellable(cancelled)?);
        Ok(Self { backend: crate::desktop_backend::DesktopBackend::Portal, init_error: None,
            display: "wayland:portal".into(), width: session.width, height: session.height,
            portal: Some(session), kms: None, buffer: Mutex::new(Vec::new()), x11_generation: None })
    }
    pub fn new() -> Result<Self, String> {
        let backend=crate::desktop_backend::detect()?;
        if backend == crate::desktop_backend::DesktopBackend::Kms {
            let kms=crate::kms::Capture::open()?;
            let (width,height)=(kms.width,kms.height);
            return Ok(Self { backend,init_error:None,portal:None,display:"kms:linear".into(),width,height,
                buffer:Mutex::new(Vec::new()),kms:Some(kms),x11_generation:None });
        }
        if backend == crate::desktop_backend::DesktopBackend::Portal {
            return Self::open_portal(&std::sync::atomic::AtomicBool::new(false)).map_err(|error| error.to_string());
        }
        if backend == crate::desktop_backend::DesktopBackend::Wayland {
            let (width,height)=crate::wayland::init()?;
            return Ok(Self { backend, init_error:None, portal:None, kms:None, display:"wayland:native".into(), width,height,
                buffer:Mutex::new(Vec::new()), x11_generation:None });
        }
        #[cfg(target_os = "linux")]
        let (generation, display, w, h) = {
            let display_config = crate::virtual_display::display_config();
            let display_name = display_config.as_ref().map(|config| {
                std::ffi::CString::new(config.display.as_bytes())
                    .map_err(|_| "Selected X11 display contains a NUL byte".to_string())
            }).transpose()?;
            let mut leases = X11_LEASES.lock().unwrap();
            let requested_display = display_config.as_ref().map(|config| config.display.as_str());
            let switch = matches!((leases.display.as_deref(), requested_display),
                (Some(current), Some(requested)) if current != requested);
            let ret = if switch {
                let Some(display_name) = display_name.as_ref() else { unreachable!() };
                let ret = unsafe { x11_switch_display(display_name.as_ptr()) };
                if ret == 0 {
                    leases.generation = leases.generation.wrapping_add(1).max(1);
                    leases.users = 0;
                    leases.display = requested_display.map(str::to_owned);
                }
                ret
            } else if leases.display.is_none() {
                let display_ptr = display_name.as_ref().map_or(std::ptr::null(), |name| name.as_ptr());
                let ret = unsafe { x11_init(display_ptr) };
                if ret == 0 {
                    leases.display = requested_display.map(str::to_owned);
                }
                ret
            } else {
                unsafe { x11_init(std::ptr::null()) }
            };
            if ret != 0 {
                return Err(format!("x11_init failed with code {}", ret));
            }
            let mut w: libc::c_int = 0;
            let mut h: libc::c_int = 0;
            let ret = unsafe { x11_get_dimensions(&mut w, &mut h) };
            if ret != 0 { return Err(format!("x11_get_dimensions failed with code {}", ret)); }
            if w <= 0 || h <= 0 {
                return Err(format!("Invalid screen dimensions: {}x{}", w, h));
            }
            let display = if let Some(display) = requested_display {
                display.to_owned()
            } else {
                let mut display_buffer = [0 as libc::c_char; 512];
                let ret = unsafe { x11_display_name(display_buffer.as_mut_ptr(), display_buffer.len()) };
                if ret != 0 { return Err("Capture display name unavailable".into()); }
                let display = unsafe { std::ffi::CStr::from_ptr(display_buffer.as_ptr()) }
                    .to_string_lossy().into_owned();
                leases.display = Some(display.clone());
                display
            };
            leases.users += 1;
            (leases.generation, display, w, h)
        };
        #[cfg(not(target_os = "linux"))]
        let (_generation, display, w, h) = {
            let ret = unsafe { x11_init(std::ptr::null()) };
            if ret != 0 { return Err(format!("x11_init failed with code {}", ret)); }
            let mut w: libc::c_int = 0;
            let mut h: libc::c_int = 0;
            unsafe { x11_get_dimensions(&mut w, &mut h) };
            if w <= 0 || h <= 0 {
                return Err(format!("Invalid screen dimensions: {}x{}", w, h));
            }
            let mut display_buffer = [0 as libc::c_char; 512];
            if unsafe { x11_display_name(display_buffer.as_mut_ptr(), display_buffer.len()) } != 0 {
                return Err("Capture display name unavailable".into());
            }
            let display = unsafe { std::ffi::CStr::from_ptr(display_buffer.as_ptr()) }
                .to_string_lossy().into_owned();
            (0, display, w, h)
        };

        let buffer = Mutex::new(Vec::new());
        log::info!("ScreenCapture initialized: {}x{}", w, h);
        log::info!("Selected capture display: {}", display);
        Ok(Self {
            backend,
            init_error:None,
            portal:None,
            kms:None,
            display,
            width: w as u32,
            height: h as u32,
            buffer,
            #[cfg(target_os = "linux")]
            x11_generation: Some(generation),
            #[cfg(not(target_os = "linux"))]
            x11_generation: None,
        })
    }

    /// Metadata for non-desktop tools. Never advertise a usable fake screen.
    pub fn unavailable(error:String)->Self {
        Self { backend:crate::desktop_backend::detect().unwrap_or(crate::desktop_backend::DesktopBackend::X11),
            init_error:Some(error), portal:None, kms:None, display:String::new(), width:0,height:0,buffer:Mutex::new(Vec::new()),
            x11_generation:None }
    }

    /// 截取一帧 BGRA 图像
    pub fn capture_frame(&self) -> Result<Vec<u8>, String> {
        if let Some(error)=&self.init_error { return Err(error.clone()); }
        let mut buf = self.buffer.lock().unwrap();
        buf.resize(self.width as usize * self.height as usize * 4, 0);
        if let Some(kms)=&self.kms { kms.capture(&mut buf)?; return Ok(buf.clone()); }
        if let Some(session)=&self.portal { session.capture(&mut buf)?; return Ok(buf.clone()); }
        if self.backend == crate::desktop_backend::DesktopBackend::Wayland {
            crate::wayland::capture(&mut buf)?;
            return Ok(buf.clone());
        }
        let mut w: libc::c_int = self.width as libc::c_int;
        let mut h: libc::c_int = self.height as libc::c_int;

        let ret = unsafe { x11_capture_frame(buf.as_mut_ptr(), &mut w, &mut h) };
        if ret != 0 {
            if ret == -4 { return Err(format!("desktop resized to {w}x{h}; reconnect to capture the new size")); }
            return Err(format!("x11_capture_frame failed with code {}", ret));
        }

        Ok(buf.clone())
    }
}

/// X11 captures share one connection. A display switch advances the generation so late drops from
/// captures on the old display cannot close the new connection.
struct X11LeaseState { generation: u64, users: usize, display: Option<String> }
static X11_LEASES: Mutex<X11LeaseState> = Mutex::new(X11LeaseState { generation: 1, users: 0, display: None });

impl Drop for ScreenCapture {
    fn drop(&mut self) {
        if self.init_error.is_none() && self.backend==crate::desktop_backend::DesktopBackend::X11 {
            if let Some(generation) = self.x11_generation {
                let mut leases = X11_LEASES.lock().unwrap();
                if generation == leases.generation && leases.users > 0 {
                    leases.users -= 1;
                    if leases.users == 0 {
                        unsafe { x11_close() };
                        leases.display = None;
                    }
                }
            }
        }
    }
}

/// Keep the display visible only while a peer is streaming; never unlock it.
pub fn keep_display_awake() -> Result<(), String> {
    if crate::desktop_backend::detect()? == crate::desktop_backend::DesktopBackend::Kms { return Ok(()); }
    if crate::desktop_backend::detect()? == crate::desktop_backend::DesktopBackend::Portal {
        return crate::desktop_power::wake();
    }
    if crate::desktop_backend::detect()? == crate::desktop_backend::DesktopBackend::Wayland {
        return crate::wayland::wake();
    }
    #[cfg(target_os = "linux")]
    if unsafe { x11_wake_display() } != 0 { return Err("X11 display wake failed".into()); }
    Ok(())
}
