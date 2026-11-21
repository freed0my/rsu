//! Typed text from key codes, by the user's own keyboard layout. The
//! compositor sends its keymap to every client; libxkbcommon (loaded at run
//! time, so the game still starts without it) turns key codes plus the
//! modifier and layout state into characters. Gameplay never goes through
//! here: physical key positions stay what binds are made of.

use std::ffi::{c_char, c_int, c_void, CStr};
use std::os::fd::{AsRawFd, OwnedFd};

type NewContext = unsafe extern "C" fn(c_int) -> *mut c_void;
type NewKeymap = unsafe extern "C" fn(*mut c_void, *const c_char, c_int, c_int) -> *mut c_void;
type NewState = unsafe extern "C" fn(*mut c_void) -> *mut c_void;
type Unref = unsafe extern "C" fn(*mut c_void);
type UpdateMask = unsafe extern "C" fn(*mut c_void, u32, u32, u32, u32, u32, u32) -> c_int;
type KeyUtf8 = unsafe extern "C" fn(*mut c_void, u32, *mut c_char, usize) -> c_int;

/// `XKB_KEYMAP_FORMAT_TEXT_V1`.
const FORMAT_TEXT_V1: c_int = 1;

struct Lib {
    new_context: NewContext,
    new_keymap: NewKeymap,
    new_state: NewState,
    keymap_unref: Unref,
    state_unref: Unref,
    context_unref: Unref,
    update_mask: UpdateMask,
    key_utf8: KeyUtf8,
}

impl Lib {
    fn load() -> Option<Lib> {
        // SAFETY: dlopen/dlsym with NUL-terminated names; the transmutes give
        // each symbol the signature libxkbcommon documents for it.
        unsafe {
            let handle = ["libxkbcommon.so.0", "libxkbcommon.so"].iter().find_map(|name| {
                let name = std::ffi::CString::new(*name).ok()?;
                let h = libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL);
                (!h.is_null()).then_some(h)
            })?;
            let sym = |name: &CStr| {
                let p = libc::dlsym(handle, name.as_ptr());
                (!p.is_null()).then_some(p)
            };
            Some(Lib {
                new_context: std::mem::transmute::<*mut c_void, NewContext>(sym(c"xkb_context_new")?),
                new_keymap: std::mem::transmute::<*mut c_void, NewKeymap>(sym(c"xkb_keymap_new_from_string")?),
                new_state: std::mem::transmute::<*mut c_void, NewState>(sym(c"xkb_state_new")?),
                keymap_unref: std::mem::transmute::<*mut c_void, Unref>(sym(c"xkb_keymap_unref")?),
                state_unref: std::mem::transmute::<*mut c_void, Unref>(sym(c"xkb_state_unref")?),
                context_unref: std::mem::transmute::<*mut c_void, Unref>(sym(c"xkb_context_unref")?),
                update_mask: std::mem::transmute::<*mut c_void, UpdateMask>(sym(c"xkb_state_update_mask")?),
                key_utf8: std::mem::transmute::<*mut c_void, KeyUtf8>(sym(c"xkb_state_key_get_utf8")?),
            })
        }
    }
}

/// The current layout of the compositor's keyboard.
pub struct Typist {
    lib: Lib,
    context: *mut c_void,
    keymap: *mut c_void,
    state: *mut c_void,
}

impl Typist {
    /// Build one from the keymap file the compositor sent. `None` when
    /// libxkbcommon is missing or the keymap does not parse.
    pub fn from_keymap(fd: &OwnedFd, size: u32) -> Option<Typist> {
        let lib = Lib::load()?;
        let len = size as usize;
        // SAFETY: the fd is a readable keymap of `size` bytes (the protocol
        // says so); the mapping is private, read-only and dropped before
        // returning. xkbcommon copies what it needs from the string.
        unsafe {
            let map = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, fd.as_raw_fd(), 0);
            if map == libc::MAP_FAILED {
                return None;
            }
            let context = (lib.new_context)(0);
            let keymap = if context.is_null() {
                std::ptr::null_mut()
            } else {
                // The text is NUL-terminated inside the mapping.
                (lib.new_keymap)(context, map.cast(), FORMAT_TEXT_V1, 0)
            };
            libc::munmap(map, len);
            let state = if keymap.is_null() { std::ptr::null_mut() } else { (lib.new_state)(keymap) };
            if state.is_null() {
                if !keymap.is_null() {
                    (lib.keymap_unref)(keymap);
                }
                if !context.is_null() {
                    (lib.context_unref)(context);
                }
                return None;
            }
            Some(Typist { lib, context, keymap, state })
        }
    }

    /// Modifiers and the layout group, as `wl_keyboard.modifiers` reports them.
    pub fn modifiers(&mut self, depressed: u32, latched: u32, locked: u32, group: u32) {
        // SAFETY: `state` is live for the lifetime of self.
        unsafe {
            (self.lib.update_mask)(self.state, depressed, latched, locked, 0, 0, group);
        }
    }

    /// The text a key press types now. `evdev` is the evdev key code; xkb
    /// counts from 8. Control characters (Ctrl+letter) are not text.
    pub fn text(&self, evdev: u32) -> Vec<char> {
        let mut buf = [0 as c_char; 16];
        // SAFETY: `state` is live; the buffer length is passed along.
        let n = unsafe { (self.lib.key_utf8)(self.state, evdev + 8, buf.as_mut_ptr(), buf.len()) };
        if n <= 0 || n as usize >= buf.len() {
            return Vec::new();
        }
        let bytes: Vec<u8> = buf[..n as usize].iter().map(|&b| b as u8).collect();
        String::from_utf8(bytes).map(|s| s.chars().filter(|c| !c.is_control()).collect()).unwrap_or_default()
    }
}

impl Drop for Typist {
    fn drop(&mut self) {
        // SAFETY: each object is released once, state first.
        unsafe {
            (self.lib.state_unref)(self.state);
            (self.lib.keymap_unref)(self.keymap);
            (self.lib.context_unref)(self.context);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::FromRawFd;

    /// A keymap file of the compositor's kind, from the real xkb data.
    fn keymap_fd() -> Option<(OwnedFd, u32)> {
        let out = std::process::Command::new("xkbcli").args(["compile-keymap", "--layout", "us,ru"]).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let mut text = out.stdout;
        text.push(0);
        // SAFETY: memfd_create gives a fresh descriptor that nothing else owns.
        let fd = unsafe { libc::memfd_create(c"keymap".as_ptr(), 0) };
        if fd < 0 {
            return None;
        }
        // SAFETY: as above; the write is of a live buffer.
        unsafe {
            if libc::write(fd, text.as_ptr().cast(), text.len()) != text.len() as isize {
                return None;
            }
            Some((OwnedFd::from_raw_fd(fd), text.len() as u32))
        }
    }

    #[test]
    fn the_layout_group_decides_the_letter() {
        let Some((fd, size)) = keymap_fd() else { return };
        let Some(mut typist) = Typist::from_keymap(&fd, size) else { return };
        // KEY_Q = 16, KEY_A = 30.
        assert_eq!(typist.text(16), vec!['q']);
        typist.modifiers(0, 0, 0, 1);
        assert_eq!(typist.text(16), vec!['й']);
        assert_eq!(typist.text(30), vec!['ф']);
        // Shift (depressed bit 0) gives the capital; Ctrl gives no text.
        typist.modifiers(1, 0, 0, 1);
        assert_eq!(typist.text(16), vec!['Й']);
        typist.modifiers(4, 0, 0, 1);
        assert!(typist.text(16).is_empty());
    }
}
