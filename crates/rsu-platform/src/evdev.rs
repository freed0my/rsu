//! Direct evdev reader.
//!
//! A dedicated thread blocks in epoll on every usable device and forwards
//! events with the kernel's own CLOCK_MONOTONIC timestamps, bypassing the
//! compositor. It works with anything that shows up as an input device: a
//! kernel tablet driver, OpenTabletDriver's uinput devices, keyboards.
//!
//! Two outputs with different guarantees:
//! * an SPSC queue carrying every event in order (nothing dropped; time spent
//!   in the queue is harmless because each event carries its own timestamp);
//! * a seqlock cell with only the newest pointer sample, for the renderer to
//!   latch right before submitting a frame.
//!
//! Needs read access to /dev/input/event* (the `input` group on most
//! distributions). Without it `start` returns no devices and the caller
//! falls back to window-system events.

use std::ffi::CStr;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use rsu_rt::{seqlock, spsc};

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;
const ABS_X: u16 = 0;
const ABS_Y: u16 = 1;
const BTN_MISC: u16 = 0x100;
const BTN_LEFT: usize = 0x110;
const BTN_TOOL_PEN: usize = 0x140;
const BTN_TOOL_FINGER: usize = 0x145;
const BTN_TOUCH: usize = 0x14a;
const KEY_ESC: usize = 1;
const KEY_Z: usize = 44;
const KEY_X: usize = 45;

const fn ioc(dir: u64, nr: u64, size: u64) -> libc::c_ulong {
    ((dir << 30) | (size << 16) | ((b'E' as u64) << 8) | nr) as libc::c_ulong
}
const IOC_WRITE: u64 = 1;
const IOC_READ: u64 = 2;
const fn eviocgbit(ev: u64, len: u64) -> libc::c_ulong {
    ioc(IOC_READ, 0x20 + ev, len)
}
const fn eviocgabs(axis: u64) -> libc::c_ulong {
    ioc(IOC_READ, 0x40 + axis, std::mem::size_of::<libc::input_absinfo>() as u64)
}
const fn eviocgname(len: u64) -> libc::c_ulong {
    ioc(IOC_READ, 0x06, len)
}
const EVIOCSCLOCKID: libc::c_ulong = ioc(IOC_WRITE, 0xa0, 4);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputKind {
    /// Absolute pointer moved; `x`/`y` are raw device units.
    Motion,
    /// Keyboard key; `code` is the evdev key code.
    Key,
    /// Pointer button or pen tip; `code` is the evdev button code.
    Button,
}

#[derive(Clone, Copy, Debug)]
pub struct InputEvent {
    /// Kernel timestamp, CLOCK_MONOTONIC nanoseconds.
    pub t_ns: u64,
    pub kind: InputKind,
    /// Index into [`EvdevInput::devices`].
    pub dev: u8,
    pub pressed: bool,
    pub code: u16,
    pub x: i32,
    pub y: i32,
}

/// Newest absolute pointer sample. `t_ns == 0` means "none yet".
#[derive(Clone, Copy, Debug, Default)]
pub struct CursorSample {
    pub t_ns: u64,
    pub x: i32,
    pub y: i32,
    pub dev: u8,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    AbsPointer,
    Keyboard,
}

#[derive(Clone, Debug)]
pub struct DeviceInfo {
    pub name: String,
    pub kind: DeviceKind,
    /// Axis ranges for absolute pointers: (min, max) for X and Y.
    pub x_range: (i32, i32),
    pub y_range: (i32, i32),
}

#[derive(Default)]
pub struct Stats {
    /// Events lost because the game thread stopped draining the queue.
    pub queue_overflows: AtomicU64,
    /// SYN_DROPPED from the kernel: its per-client buffer overflowed.
    pub kernel_drops: AtomicU64,
    /// Whether the reader thread got SCHED_FIFO.
    pub realtime: AtomicBool,
}

pub struct EvdevInput {
    pub events: spsc::Consumer<InputEvent>,
    pub latest: seqlock::Reader<CursorSample>,
    pub devices: Vec<DeviceInfo>,
    pub stats: Arc<Stats>,
    /// Key events are forwarded only while this is set, so the game never
    /// sees what is typed into other windows.
    pub focused: Arc<AtomicBool>,
    stop_fd: RawFd,
    thread: Option<JoinHandle<()>>,
}

impl EvdevInput {
    pub fn has_keyboard(&self) -> bool {
        self.devices.iter().any(|d| d.kind == DeviceKind::Keyboard)
    }
    pub fn has_pointer(&self) -> bool {
        self.devices.iter().any(|d| d.kind == DeviceKind::AbsPointer)
    }
}

impl Drop for EvdevInput {
    fn drop(&mut self) {
        let one = 1u64.to_ne_bytes();
        // SAFETY: stop_fd is our eventfd; writing 8 bytes is its protocol.
        unsafe { libc::write(self.stop_fd, one.as_ptr().cast(), 8) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // SAFETY: the thread has exited and no longer uses the fd.
        unsafe { libc::close(self.stop_fd) };
    }
}

struct Device {
    fd: RawFd,
    kind: DeviceKind,
    x: i32,
    y: i32,
    moved: bool,
    alive: bool,
}

fn bit(bits: &[u8], n: usize) -> bool {
    bits.get(n / 8).is_some_and(|b| b >> (n % 8) & 1 == 1)
}

fn abs_info(fd: RawFd, axis: u16) -> Option<libc::input_absinfo> {
    // SAFETY: zeroed input_absinfo is a valid out buffer of the size encoded in the request.
    let mut info: libc::input_absinfo = unsafe { std::mem::zeroed() };
    let r = unsafe { libc::ioctl(fd, eviocgabs(axis as u64), &mut info) };
    (r >= 0).then_some(info)
}

/// Open one event node and decide whether it is useful to us.
fn probe(path: &CStr) -> Option<(Device, DeviceInfo)> {
    // SAFETY: path is NUL-terminated; flags are valid.
    let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
    if fd < 0 {
        return None;
    }
    let close = |fd| unsafe { libc::close(fd) };
    let mut ev = [0u8; 4];
    let mut keys = [0u8; 96];
    let mut abs = [0u8; 8];
    // SAFETY: each buffer length matches the length encoded in its request.
    unsafe {
        if libc::ioctl(fd, eviocgbit(0, ev.len() as u64), ev.as_mut_ptr()) < 0 {
            close(fd);
            return None;
        }
        libc::ioctl(fd, eviocgbit(EV_KEY as u64, keys.len() as u64), keys.as_mut_ptr());
        libc::ioctl(fd, eviocgbit(EV_ABS as u64, abs.len() as u64), abs.as_mut_ptr());
    }
    let has_abs_xy = bit(&ev, EV_ABS as usize) && bit(&abs, ABS_X as usize) && bit(&abs, ABS_Y as usize);
    // Tablets (pen tool) and absolute mice (OpenTabletDriver's virtual
    // tablet, VM pointers). Touchpads and touchscreens report a finger tool;
    // joysticks have neither a pen nor a left button.
    let pen = bit(&keys, BTN_TOOL_PEN);
    let abs_mouse = bit(&keys, BTN_LEFT) && !bit(&keys, BTN_TOOL_FINGER);
    let touch_only = bit(&keys, BTN_TOUCH) && !pen && !bit(&keys, BTN_LEFT);
    let kind = if has_abs_xy && (pen || abs_mouse) && !touch_only {
        DeviceKind::AbsPointer
    } else if bit(&keys, KEY_ESC) && bit(&keys, KEY_Z) && bit(&keys, KEY_X) {
        DeviceKind::Keyboard
    } else {
        close(fd);
        return None;
    };

    let mut name = [0u8; 128];
    // SAFETY: buffer length matches the request; the kernel NUL-terminates.
    unsafe { libc::ioctl(fd, eviocgname(name.len() as u64 - 1), name.as_mut_ptr()) };
    let name = CStr::from_bytes_until_nul(&name).map(|c| c.to_string_lossy().into_owned()).unwrap_or_default();

    // Timestamps in the same clock as everything else in the game.
    let clk: libc::c_int = libc::CLOCK_MONOTONIC;
    // SAFETY: the request takes a pointer to an int.
    if unsafe { libc::ioctl(fd, EVIOCSCLOCKID, &clk) } < 0 {
        close(fd);
        return None;
    }

    let (mut x_range, mut y_range, mut x, mut y) = ((0, 0), (0, 0), 0, 0);
    if kind == DeviceKind::AbsPointer {
        let (ax, ay) = (abs_info(fd, ABS_X)?, abs_info(fd, ABS_Y)?);
        if ax.maximum <= ax.minimum || ay.maximum <= ay.minimum {
            close(fd);
            return None;
        }
        x_range = (ax.minimum, ax.maximum);
        y_range = (ay.minimum, ay.maximum);
        x = ax.value;
        y = ay.value;
    }
    Some((Device { fd, kind, x, y, moved: false, alive: true }, DeviceInfo { name, kind, x_range, y_range }))
}

const STOP_TOKEN: u64 = u64::MAX;
const MAX_DEVICES: usize = 32;

/// Scan /dev/input and start the reader thread. Returns `Ok` with an empty
/// device list when nothing is readable (typically: not in the input group).
pub fn start() -> std::io::Result<EvdevInput> {
    let mut devs = Vec::new();
    let mut infos = Vec::new();
    let mut nodes: Vec<_> = std::fs::read_dir("/dev/input")?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("event"))
        .map(|e| e.path())
        .collect();
    nodes.sort();
    for node in nodes {
        if devs.len() == MAX_DEVICES {
            break;
        }
        let Ok(c) = std::ffi::CString::new(node.to_string_lossy().as_bytes()) else { continue };
        if let Some((d, i)) = probe(&c) {
            devs.push(d);
            infos.push(i);
        }
    }

    // SAFETY: plain fd-creating syscalls; results are checked.
    let (ep, stop_fd) =
        unsafe { (libc::epoll_create1(libc::EPOLL_CLOEXEC), libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK)) };
    if ep < 0 || stop_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let add = |fd: RawFd, token: u64| {
        let mut e = libc::epoll_event { events: libc::EPOLLIN as u32, u64: token };
        // SAFETY: ep and fd are valid descriptors, e is a valid event.
        unsafe { libc::epoll_ctl(ep, libc::EPOLL_CTL_ADD, fd, &mut e) };
    };
    add(stop_fd, STOP_TOKEN);
    for (i, d) in devs.iter().enumerate() {
        add(d.fd, i as u64);
    }

    // 4096 events is over three seconds of a 1000 Hz device; the game
    // thread drains every frame.
    let (tx, rx) = spsc::channel::<InputEvent>(4096);
    let (latest_tx, latest_rx) = seqlock::channel(CursorSample::default());
    let stats = Arc::new(Stats::default());
    let focused = Arc::new(AtomicBool::new(false));
    let thread = {
        let (stats, focused) = (stats.clone(), focused.clone());
        std::thread::Builder::new()
            .name("rsu-input".into())
            .spawn(move || reader(ep, devs, tx, latest_tx, stats, focused))?
    };
    Ok(EvdevInput { events: rx, latest: latest_rx, devices: infos, stats, focused, stop_fd, thread: Some(thread) })
}

fn reader(
    ep: RawFd,
    mut devs: Vec<Device>,
    mut tx: spsc::Producer<InputEvent>,
    mut latest: seqlock::Writer<CursorSample>,
    stats: Arc<Stats>,
    focused: Arc<AtomicBool>,
) {
    // Above normal threads, below the audio thread PipeWire runs at 88.
    stats.realtime.store(rsu_rt::thread::set_realtime(70).is_ok(), Ordering::Relaxed);

    let mut push = |ev: InputEvent| {
        if tx.push(ev).is_err() {
            stats.queue_overflows.fetch_add(1, Ordering::Relaxed);
        }
    };
    // SAFETY: zeroed epoll_event / input_event arrays are valid buffers.
    let mut ready: [libc::epoll_event; 16] = unsafe { std::mem::zeroed() };
    let mut buf: [libc::input_event; 64] = unsafe { std::mem::zeroed() };
    'outer: loop {
        // SAFETY: `ready` has room for the 16 events we ask for.
        let n = unsafe { libc::epoll_wait(ep, ready.as_mut_ptr(), ready.len() as i32, -1) };
        if n < 0 {
            if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            break;
        }
        for r in &ready[..n as usize] {
            let token = r.u64;
            if token == STOP_TOKEN {
                break 'outer;
            }
            let di = token as usize;
            let d = &mut devs[di];
            if !d.alive {
                continue;
            }
            loop {
                // SAFETY: buf is valid for size_of_val(&buf) bytes.
                let got = unsafe { libc::read(d.fd, buf.as_mut_ptr().cast(), std::mem::size_of_val(&buf)) };
                if got <= 0 {
                    let err = std::io::Error::last_os_error().raw_os_error();
                    if got < 0 && (err == Some(libc::EAGAIN) || err == Some(libc::EINTR)) {
                        break;
                    }
                    // Unplugged: stop watching it.
                    // SAFETY: ep and d.fd are valid; DEL ignores the event arg.
                    unsafe {
                        libc::epoll_ctl(ep, libc::EPOLL_CTL_DEL, d.fd, std::ptr::null_mut());
                        libc::close(d.fd);
                    }
                    d.alive = false;
                    break;
                }
                let count = got as usize / std::mem::size_of::<libc::input_event>();
                for e in &buf[..count] {
                    let t_ns = e.time.tv_sec as u64 * 1_000_000_000 + e.time.tv_usec as u64 * 1_000;
                    match (e.type_, e.code) {
                        (EV_ABS, ABS_X) if d.kind == DeviceKind::AbsPointer => {
                            d.x = e.value;
                            d.moved = true;
                        }
                        (EV_ABS, ABS_Y) if d.kind == DeviceKind::AbsPointer => {
                            d.y = e.value;
                            d.moved = true;
                        }
                        (EV_KEY, code) if e.value != 2 => {
                            // value 2 is keyboard auto-repeat.
                            let is_button = code >= BTN_MISC && d.kind == DeviceKind::AbsPointer;
                            if is_button || focused.load(Ordering::Relaxed) {
                                push(InputEvent {
                                    t_ns,
                                    kind: if is_button { InputKind::Button } else { InputKind::Key },
                                    dev: di as u8,
                                    pressed: e.value == 1,
                                    code,
                                    x: d.x,
                                    y: d.y,
                                });
                            }
                        }
                        (EV_SYN, SYN_REPORT) => {
                            if d.moved {
                                d.moved = false;
                                latest.write(CursorSample { t_ns, x: d.x, y: d.y, dev: di as u8 });
                                push(InputEvent {
                                    t_ns,
                                    kind: InputKind::Motion,
                                    dev: di as u8,
                                    pressed: false,
                                    code: 0,
                                    x: d.x,
                                    y: d.y,
                                });
                            }
                        }
                        (EV_SYN, SYN_DROPPED) => {
                            // The kernel discarded events; our cached state
                            // may be stale, so re-read the axes.
                            stats.kernel_drops.fetch_add(1, Ordering::Relaxed);
                            if d.kind == DeviceKind::AbsPointer {
                                if let (Some(ax), Some(ay)) = (abs_info(d.fd, ABS_X), abs_info(d.fd, ABS_Y)) {
                                    d.x = ax.value;
                                    d.y = ay.value;
                                    d.moved = true;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    for d in devs.iter().filter(|d| d.alive) {
        // SAFETY: fds are ours and still open.
        unsafe { libc::close(d.fd) };
    }
    // SAFETY: ep is ours.
    unsafe { libc::close(ep) };
}
