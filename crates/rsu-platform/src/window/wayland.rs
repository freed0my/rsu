use super::xkb::Typist;
use super::{FrameFeedback, NativeSurface, WinEvent, WindowOptions};
use std::os::fd::AsRawFd;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_compositor, wl_keyboard, wl_output, wl_pointer, wl_registry, wl_seat, wl_surface};
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::wp::content_type::v1::client::{wp_content_type_manager_v1, wp_content_type_v1};
use wayland_protocols::wp::idle_inhibit::zv1::client::{zwp_idle_inhibit_manager_v1, zwp_idle_inhibitor_v1};
use wayland_protocols::wp::presentation_time::client::{wp_presentation, wp_presentation_feedback};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

struct State {
    events: Vec<WinEvent>,
    size: (u32, u32),
    pending_size: (u32, u32),
    configured: bool,
    pointer: Option<wl_pointer::WlPointer>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    /// Turns key codes into typed characters; set once the compositor has
    /// sent its keymap and libxkbcommon is there.
    typist: Option<Typist>,
    /// The refresh rate of every output in millihertz, and the one the
    /// window is on.
    refresh: Vec<(wl_output::WlOutput, i32)>,
    on_output: Option<wl_output::WlOutput>,
    /// Answers about presented frames, in arrival order.
    feedback: Vec<FrameFeedback>,
    /// The clock of presentation timestamps; only CLOCK_MONOTONIC is used.
    presentation_clock: Option<u32>,
}

pub struct WaylandWindow {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    surface: wl_surface::WlSurface,
    _outputs: Vec<wl_output::WlOutput>,
    qh: QueueHandle<State>,
    presentation: Option<wp_presentation::WpPresentation>,
    idle_manager: Option<zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1>,
    idle_inhibitor: Option<zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1>,
    // Kept alive for the lifetime of the window.
    _xdg_surface: xdg_surface::XdgSurface,
    _toplevel: xdg_toplevel::XdgToplevel,
}

impl WaylandWindow {
    pub fn open(opts: &WindowOptions) -> Result<Self, String> {
        let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let (globals, mut queue) = registry_queue_init::<State>(&conn).map_err(|e| e.to_string())?;
        let qh = queue.handle();
        let e = |what: &str| format!("compositor lacks {what}");

        let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 1..=4, ()).map_err(|_| e("wl_compositor"))?;
        let wm_base: xdg_wm_base::XdgWmBase = globals.bind(&qh, 1..=1, ()).map_err(|_| e("xdg_wm_base"))?;
        let _seat: wl_seat::WlSeat = globals.bind(&qh, 1..=5, ()).map_err(|_| e("wl_seat"))?;

        // Outputs, only for their refresh rates.
        let mut outputs = Vec::new();
        for g in globals.contents().clone_list() {
            if g.interface == "wl_output" {
                outputs.push(globals.registry().bind::<wl_output::WlOutput, _, _>(g.name, g.version.min(3), &qh, ()));
            }
        }
        let surface = compositor.create_surface(&qh, ());
        // Tells the compositor this is a game: several of them use the hint
        // to pick direct scanout, VRR or tearing policies on their own.
        if let Ok(mgr) = globals.bind::<wp_content_type_manager_v1::WpContentTypeManagerV1, _, _>(&qh, 1..=1, ()) {
            mgr.get_surface_content_type(&surface, &qh, ()).set_content_type(wp_content_type_v1::Type::Game);
        }
        // When frames really reach the screen. Not tearing-control: the
        // Vulkan driver may create that object itself, and a second one on
        // the same surface is a protocol error that ends the connection.
        let presentation = globals.bind::<wp_presentation::WpPresentation, _, _>(&qh, 1..=2, ()).ok();
        let idle_manager = globals.bind(&qh, 1..=1, ()).ok();
        let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
        let toplevel = xdg_surface.get_toplevel(&qh, ());
        toplevel.set_title(opts.title.clone());
        toplevel.set_app_id("rsu".into());
        if opts.fullscreen {
            toplevel.set_fullscreen(None);
        }
        surface.commit();

        let mut state = State {
            events: Vec::with_capacity(256),
            size: opts.size,
            pending_size: (0, 0),
            configured: false,
            pointer: None,
            keyboard: None,
            typist: None,
            refresh: Vec::new(),
            on_output: None,
            feedback: Vec::with_capacity(64),
            presentation_clock: None,
        };
        // The first buffer may only be attached after the initial configure.
        while !state.configured {
            queue.blocking_dispatch(&mut state).map_err(|e| e.to_string())?;
        }
        state.events.clear();
        // The outputs say their modes once; the window's first `enter` may
        // come with the next round trip.
        let _ = queue.roundtrip(&mut state);
        Ok(WaylandWindow {
            conn,
            queue,
            state,
            surface,
            _outputs: outputs,
            qh,
            presentation,
            idle_manager,
            idle_inhibitor: None,
            _xdg_surface: xdg_surface,
            _toplevel: toplevel,
        })
    }

    pub fn pump(&mut self, out: &mut Vec<WinEvent>) {
        let _ = self.queue.dispatch_pending(&mut self.state);
        let _ = self.conn.flush();
        // Non-blocking read. The Vulkan driver shares this connection from
        // its own queue; prepare_read is the protocol that keeps the two
        // readers from stealing each other's wake-ups.
        if let Some(guard) = self.conn.prepare_read() {
            let mut pfd = libc::pollfd { fd: guard.connection_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: pfd is a valid pollfd; zero timeout never blocks.
            let ready = unsafe { libc::poll(&mut pfd, 1, 0) } > 0;
            if ready {
                let _ = guard.read();
            }
        }
        let _ = self.queue.dispatch_pending(&mut self.state);
        out.append(&mut self.state.events);
    }

    pub fn native(&self) -> NativeSurface {
        NativeSurface::Wayland {
            display: self.conn.backend().display_ptr().cast(),
            surface: self.surface.id().as_ptr().cast(),
        }
    }

    pub fn size(&self) -> (u32, u32) {
        self.state.size
    }

    /// Whether `WinEvent::Text` is sent for key presses.
    pub fn has_text(&self) -> bool {
        self.state.typist.is_some()
    }

    pub fn request_feedback(&mut self, tag: u64) -> bool {
        match &self.presentation {
            Some(p) => {
                p.feedback(&self.surface, &self.qh, tag);
                true
            }
            None => false,
        }
    }

    pub fn take_feedback(&mut self, out: &mut Vec<FrameFeedback>) {
        out.append(&mut self.state.feedback);
    }

    pub fn inhibit_idle(&mut self, on: bool) {
        match (on, self.idle_inhibitor.take(), &self.idle_manager) {
            (true, None, Some(m)) => self.idle_inhibitor = Some(m.create_inhibitor(&self.surface, &self.qh, ())),
            (true, kept, _) => self.idle_inhibitor = kept,
            (false, Some(i), _) => i.destroy(),
            (false, None, _) => {}
        }
    }

    /// Refresh rate of the output the window is on, in hertz.
    pub fn refresh_hz(&self) -> Option<f32> {
        let on = self.state.on_output.as_ref();
        let found = self.state.refresh.iter().find(|(o, _)| Some(o) == on).or(self.state.refresh.first());
        found.map(|&(_, mhz)| mhz as f32 / 1000.0).filter(|&hz| hz > 0.0)
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xs: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xs.ack_configure(serial);
            state.configured = true;
            let (w, h) = state.pending_size;
            if w > 0 && h > 0 && (w, h) != state.size {
                state.size = (w, h);
                state.events.push(WinEvent::Resized(w, h));
            }
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                // 0x0 means "pick your own size": keep the current one.
                state.pending_size = (width.max(0) as u32, height.max(0) as u32);
            }
            xdg_toplevel::Event::Close => state.events.push(WinEvent::CloseRequested),
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = event {
            if caps.contains(wl_seat::Capability::Pointer) && state.pointer.is_none() {
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Keyboard) && state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        pointer: &wl_pointer::WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { serial, .. } => {
                // The game draws its own cursor; hide the system one.
                pointer.set_cursor(serial, None, 0, 0);
            }
            wl_pointer::Event::Motion { time, surface_x, surface_y } => {
                state.events.push(WinEvent::PointerMotion { x: surface_x as f32, y: surface_y as f32, time_ms: time });
            }
            wl_pointer::Event::Leave { .. } => state.events.push(WinEvent::PointerLeave),
            // 0x110 is BTN_LEFT.
            wl_pointer::Event::Button { button: 0x110, state: WEnum::Value(bs), .. } => {
                state.events.push(WinEvent::PointerButton { pressed: bs == wl_pointer::ButtonState::Pressed });
            }
            wl_pointer::Event::Axis { axis: WEnum::Value(wl_pointer::Axis::VerticalScroll), value, .. } => {
                // A wheel notch is reported as 10 to 15 units.
                state.events.push(WinEvent::Scroll(value as f32 / 12.0));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_keyboard::WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1), fd, size } => {
                state.typist = Typist::from_keymap(&fd, size);
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                if let Some(t) = &mut state.typist {
                    t.modifiers(mods_depressed, mods_latched, mods_locked, group);
                }
            }
            wl_keyboard::Event::Enter { .. } => state.events.push(WinEvent::Focus(true)),
            wl_keyboard::Event::Leave { .. } => state.events.push(WinEvent::Focus(false)),
            wl_keyboard::Event::Key { key, state: WEnum::Value(ks), .. } => {
                // `key` is already an evdev code. Physical positions are what
                // a rhythm game binds, so the code goes on as it is; the text
                // the key types in the current layout comes beside it.
                let pressed = ks == wl_keyboard::KeyState::Pressed;
                state.events.push(WinEvent::Key { code: key, pressed });
                if let (true, Some(t)) = (pressed, &state.typist) {
                    state.events.extend(t.text(key).into_iter().map(WinEvent::Text));
                }
            }
            _ => {}
        }
    }
}

delegate_noop!(State: wl_compositor::WlCompositor);

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(
        state: &mut Self,
        _: &wl_surface::WlSurface,
        event: wl_surface::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_surface::Event::Enter { output } = event {
            state.on_output = Some(output);
        }
    }
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        state: &mut Self,
        output: &wl_output::WlOutput,
        event: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Mode { flags, refresh, .. } = event {
            if matches!(flags, WEnum::Value(f) if f.contains(wl_output::Mode::Current)) {
                state.refresh.retain(|(o, _)| o != output);
                state.refresh.push((output.clone(), refresh));
            }
        }
    }
}
delegate_noop!(State: wp_content_type_manager_v1::WpContentTypeManagerV1);
delegate_noop!(State: wp_content_type_v1::WpContentTypeV1);
delegate_noop!(State: zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1);
delegate_noop!(State: zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1);

impl Dispatch<wp_presentation::WpPresentation, ()> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation::WpPresentation,
        event: wp_presentation::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wp_presentation::Event::ClockId { clk_id } = event {
            state.presentation_clock = Some(clk_id);
        }
    }
}

impl Dispatch<wp_presentation_feedback::WpPresentationFeedback, u64> for State {
    fn event(
        state: &mut Self,
        _: &wp_presentation_feedback::WpPresentationFeedback,
        event: wp_presentation_feedback::Event,
        &tag: &u64,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wp_presentation_feedback::Event::Presented { tv_sec_hi, tv_sec_lo, tv_nsec, refresh, flags, .. } => {
                let monotonic = state.presentation_clock == Some(libc::CLOCK_MONOTONIC as u32);
                let sec = (tv_sec_hi as u64) << 32 | tv_sec_lo as u64;
                let flags = match flags {
                    WEnum::Value(k) => k.bits(),
                    WEnum::Unknown(bits) => bits,
                };
                state.feedback.push(FrameFeedback {
                    tag,
                    discarded: false,
                    shown_ns: if monotonic { sec * 1_000_000_000 + tv_nsec as u64 } else { 0 },
                    refresh_ns: refresh,
                    flags,
                });
            }
            wp_presentation_feedback::Event::Discarded => {
                state.feedback.push(FrameFeedback { tag, discarded: true, ..FrameFeedback::default() });
            }
            _ => {} // sync_output
        }
    }
}
