//! Window backends. The window exists to give Vulkan a surface and to report
//! focus, size and fallback input; gameplay input normally comes from evdev.

mod wayland;
mod x11;
mod xkb;

use std::ffi::c_void;

#[derive(Clone, Copy, Debug)]
pub enum WinEvent {
    Resized(u32, u32),
    CloseRequested,
    Focus(bool),
    /// `code` is an evdev key code on both backends.
    Key {
        code: u32,
        pressed: bool,
    },
    /// Surface-local pixels; `time_ms` is the windowing system's timestamp.
    PointerMotion {
        x: f32,
        y: f32,
        time_ms: u32,
    },
    PointerLeave,
    /// The primary (left) pointer button.
    PointerButton {
        pressed: bool,
    },
    /// A character typed by a key press in the user's layout (Wayland only).
    Text(char),
    /// Vertical scrolling in wheel notches; positive is down.
    Scroll(f32),
}

/// What the compositor said about one frame, matched by the tag the frame
/// was given in [`Window::request_feedback`].
#[derive(Clone, Copy, Debug, Default)]
pub struct FrameFeedback {
    pub tag: u64,
    /// The frame was replaced by a newer one before it reached the screen.
    pub discarded: bool,
    /// CLOCK_MONOTONIC time the frame started to show (scanout began), or 0
    /// when the compositor uses another clock.
    pub shown_ns: u64,
    /// Length of the refresh cycle the frame went out in; 0 if unknown.
    pub refresh_ns: u32,
    /// `FEEDBACK_*` bits.
    pub flags: u32,
}

/// The frame went out at a vertical blank (no tearing).
pub const FEEDBACK_VSYNC: u32 = 0x1;
/// The time comes from the display hardware, not a guess.
pub const FEEDBACK_HW_CLOCK: u32 = 0x2;
pub const FEEDBACK_HW_COMPLETION: u32 = 0x4;
/// The display read the game's own buffer (direct scanout, no composition).
pub const FEEDBACK_ZERO_COPY: u32 = 0x8;

/// Raw handles for creating a Vulkan surface.
#[derive(Clone, Copy, Debug)]
pub enum NativeSurface {
    Wayland { display: *mut c_void, surface: *mut c_void },
    Xcb { connection: *mut c_void, window: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Auto,
    Wayland,
    X11,
}

pub struct WindowOptions {
    pub title: String,
    pub fullscreen: bool,
    /// Used when windowed, and as a fallback until the first configure.
    pub size: (u32, u32),
    pub backend: Backend,
}

// One per program: the size of the variants does not matter.
#[allow(clippy::large_enum_variant)]
pub enum Window {
    Wayland(wayland::WaylandWindow),
    X11(x11::X11Window),
}

impl Window {
    pub fn open(opts: &WindowOptions) -> Result<Window, String> {
        let want_wayland = match opts.backend {
            Backend::Wayland => true,
            Backend::X11 => false,
            Backend::Auto => std::env::var_os("WAYLAND_DISPLAY").is_some(),
        };
        if want_wayland {
            match wayland::WaylandWindow::open(opts) {
                Ok(w) => return Ok(Window::Wayland(w)),
                Err(e) if opts.backend == Backend::Wayland => return Err(e),
                Err(e) => eprintln!("rsu: Wayland unavailable ({e}), trying X11"),
            }
        }
        x11::X11Window::open(opts).map(Window::X11)
    }

    /// Collect pending events without blocking. `out` is appended to.
    pub fn pump(&mut self, out: &mut Vec<WinEvent>) {
        match self {
            Window::Wayland(w) => w.pump(out),
            Window::X11(w) => w.pump(out),
        }
    }

    pub fn native(&self) -> NativeSurface {
        match self {
            Window::Wayland(w) => w.native(),
            Window::X11(w) => w.native(),
        }
    }

    pub fn size(&self) -> (u32, u32) {
        match self {
            Window::Wayland(w) => w.size(),
            Window::X11(w) => w.size(),
        }
    }

    /// The refresh rate of the display the window is on, where the
    /// platform says.
    pub fn refresh_hz(&self) -> Option<f32> {
        match self {
            Window::Wayland(w) => w.refresh_hz(),
            Window::X11(_) => None,
        }
    }

    /// Ask the compositor when the next frame shows. Call once per frame,
    /// before the frame is presented: the request attaches to the surface's
    /// next commit, which the Vulkan driver makes inside the present call.
    /// Returns false where the platform cannot tell (X11, old compositors).
    pub fn request_feedback(&mut self, tag: u64) -> bool {
        match self {
            Window::Wayland(w) => w.request_feedback(tag),
            Window::X11(_) => false,
        }
    }

    /// Answers to [`Window::request_feedback`] that arrived with the last
    /// `pump`. `out` is appended to.
    pub fn take_feedback(&mut self, out: &mut Vec<FrameFeedback>) {
        if let Window::Wayland(w) = self {
            w.take_feedback(out);
        }
    }

    /// Keep the screen from blanking or locking while true (a replay or
    /// autoplay gives no input for minutes).
    pub fn inhibit_idle(&mut self, on: bool) {
        if let Window::Wayland(w) = self {
            w.inhibit_idle(on);
        }
    }

    /// Whether the window reports typed characters as `WinEvent::Text`.
    /// Where it does not (X11), menus fall back to a US layout.
    pub fn has_text(&self) -> bool {
        match self {
            Window::Wayland(w) => w.has_text(),
            Window::X11(_) => false,
        }
    }

    pub fn backend_name(&self) -> &'static str {
        match self {
            Window::Wayland(_) => "WAYLAND",
            Window::X11(_) => "X11",
        }
    }
}
