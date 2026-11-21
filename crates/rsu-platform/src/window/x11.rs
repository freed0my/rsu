use super::{NativeSurface, WinEvent, WindowOptions};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, PropMode,
    Rectangle, WindowClass,
};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;
use x11rb::xcb_ffi::XCBConnection;

pub struct X11Window {
    conn: XCBConnection,
    window: u32,
    wm_delete: u32,
    size: (u32, u32),
    /// One event of look-ahead, for recognising auto-repeat.
    stash: Option<Event>,
}

impl X11Window {
    pub fn open(opts: &WindowOptions) -> Result<Self, String> {
        Self::open_inner(opts).map_err(|e| e.to_string())
    }

    fn open_inner(opts: &WindowOptions) -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen_num) = XCBConnection::connect(None)?;
        let screen = &conn.setup().roots[screen_num];
        let (root, black) = (screen.root, screen.black_pixel);
        let (w, h) =
            if opts.fullscreen { (screen.width_in_pixels as u32, screen.height_in_pixels as u32) } else { opts.size };
        let window = conn.generate_id()?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            w as u16,
            h as u16,
            0,
            WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().background_pixel(black).event_mask(
                EventMask::KEY_PRESS
                    | EventMask::KEY_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::LEAVE_WINDOW
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::FOCUS_CHANGE,
            ),
        )?;
        let atom = |name: &str| -> Result<u32, Box<dyn std::error::Error>> {
            Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
        };
        let wm_delete = atom("WM_DELETE_WINDOW")?;
        conn.change_property32(PropMode::REPLACE, window, atom("WM_PROTOCOLS")?, AtomEnum::ATOM, &[wm_delete])?;
        conn.change_property8(
            PropMode::REPLACE,
            window,
            atom("_NET_WM_NAME")?,
            atom("UTF8_STRING")?,
            opts.title.as_bytes(),
        )?;
        conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_CLASS, AtomEnum::STRING, b"rsu\0rsu\0")?;
        if opts.fullscreen {
            conn.change_property32(
                PropMode::REPLACE,
                window,
                atom("_NET_WM_STATE")?,
                AtomEnum::ATOM,
                &[atom("_NET_WM_STATE_FULLSCREEN")?],
            )?;
        }
        // Ask compositing managers to get out of the way: an unredirected
        // window is what lets the driver flip our frames straight to the screen.
        conn.change_property32(
            PropMode::REPLACE,
            window,
            atom("_NET_WM_BYPASS_COMPOSITOR")?,
            AtomEnum::CARDINAL,
            &[1],
        )?;

        // Invisible cursor: a 1x1 bitmap cleared to 0, used as its own mask.
        let pixmap = conn.generate_id()?;
        conn.create_pixmap(1, pixmap, window, 1, 1)?;
        let gc = conn.generate_id()?;
        conn.create_gc(gc, pixmap, &CreateGCAux::new().foreground(0))?;
        conn.poly_fill_rectangle(pixmap, gc, &[Rectangle { x: 0, y: 0, width: 1, height: 1 }])?;
        let cursor = conn.generate_id()?;
        conn.create_cursor(cursor, pixmap, pixmap, 0, 0, 0, 0, 0, 0, 0, 0)?;
        conn.change_window_attributes(window, &ChangeWindowAttributesAux::new().cursor(cursor))?;
        conn.free_gc(gc)?;
        conn.free_pixmap(pixmap)?;

        conn.map_window(window)?;
        conn.flush()?;
        Ok(X11Window { conn, window, wm_delete, size: (w, h), stash: None })
    }

    fn next(&mut self) -> Option<Event> {
        self.stash.take().or_else(|| self.conn.poll_for_event().ok().flatten())
    }

    pub fn pump(&mut self, out: &mut Vec<WinEvent>) {
        while let Some(ev) = self.next() {
            match ev {
                Event::KeyPress(e) => {
                    out.push(WinEvent::Key { code: (e.detail as u32).saturating_sub(8), pressed: true });
                }
                Event::KeyRelease(e) => {
                    // Auto-repeat arrives as release+press with one timestamp.
                    match self.conn.poll_for_event().ok().flatten() {
                        Some(Event::KeyPress(p)) if p.detail == e.detail && p.time == e.time => {}
                        other => {
                            self.stash = other;
                            out.push(WinEvent::Key { code: (e.detail as u32).saturating_sub(8), pressed: false });
                        }
                    }
                }
                Event::MotionNotify(e) => {
                    out.push(WinEvent::PointerMotion { x: e.event_x as f32, y: e.event_y as f32, time_ms: e.time })
                }
                Event::LeaveNotify(_) => out.push(WinEvent::PointerLeave),
                // Button 1 is the left button; 4 and 5 are the wheel.
                Event::ButtonPress(e) => match e.detail {
                    1 => out.push(WinEvent::PointerButton { pressed: true }),
                    4 => out.push(WinEvent::Scroll(-1.0)),
                    5 => out.push(WinEvent::Scroll(1.0)),
                    _ => {}
                },
                Event::ButtonRelease(e) if e.detail == 1 => out.push(WinEvent::PointerButton { pressed: false }),
                Event::FocusIn(_) => out.push(WinEvent::Focus(true)),
                Event::FocusOut(_) => out.push(WinEvent::Focus(false)),
                Event::ConfigureNotify(e) => {
                    let size = (e.width as u32, e.height as u32);
                    if size != self.size && size.0 > 0 && size.1 > 0 {
                        self.size = size;
                        out.push(WinEvent::Resized(size.0, size.1));
                    }
                }
                Event::ClientMessage(e) if e.data.as_data32()[0] == self.wm_delete => {
                    out.push(WinEvent::CloseRequested);
                }
                _ => {}
            }
        }
        let _ = self.conn.flush();
    }

    pub fn native(&self) -> NativeSurface {
        NativeSurface::Xcb { connection: self.conn.get_raw_xcb_connection(), window: self.window }
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }
}
