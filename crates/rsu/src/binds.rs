//! Which keys and buttons do what in play, as McOsu has them
//! (`OsuKeyBindings`). A binding is an evdev code: keyboard keys have the
//! small ones, pen buttons start at 0x100. Zero means not bound.

use crate::text::StackStr;
use std::fmt::Write;

pub struct Action {
    pub label: &'static str,
    /// The name in the settings file.
    pub key: &'static str,
    pub default: u32,
}

const fn action(label: &'static str, key: &'static str, default: u32) -> Action {
    Action { label, key, default }
}

pub const COUNT: usize = 9;

/// McOsu's keys, with its defaults; one that is not bound is 0.
pub const ACTIONS: [Action; COUNT] = [
    action("Left click", "key_left_click", 44),
    action("Right click", "key_right_click", 45),
    action("Left click 2", "key_left_click_2", 0),
    action("Right click 2", "key_right_click_2", 0),
    action("Smoke", "key_smoke", 46),
    action("Game pause", "key_game_pause", 1),
    action("Quick retry (hold)", "key_quick_retry", 14),
    action("Increase local offset", "key_increase_local_offset", 78),
    action("Decrease local offset", "key_decrease_local_offset", 74),
];

pub const LEFT_CLICK: usize = 0;
pub const RIGHT_CLICK: usize = 1;
pub const LEFT_CLICK_2: usize = 2;
pub const RIGHT_CLICK_2: usize = 3;
pub const SMOKE: usize = 4;
pub const GAME_PAUSE: usize = 5;
pub const QUICK_RETRY: usize = 6;
pub const INCREASE_OFFSET: usize = 7;
pub const DECREASE_OFFSET: usize = 8;

pub type Binds = [u32; COUNT];

pub fn defaults() -> Binds {
    ACTIONS.map(|a| a.default)
}

/// The pen tip and the left mouse button are what clicks the menus, so they
/// cannot be given another meaning.
pub const BTN_LEFT: u32 = 0x110;
pub const BTN_TOUCH: u32 = 0x14a;

/// Whether a code can be chosen in the options.
pub fn bindable(code: u32) -> bool {
    code != 0 && code != BTN_LEFT && code != BTN_TOUCH
}

/// Whether `code` is what `action` is bound to.
pub fn is(binds: &Binds, action: usize, code: u32) -> bool {
    code != 0 && binds[action] == code
}

/// A short name for the options panel.
pub fn name(code: u32) -> StackStr<24> {
    // Codes 1 to 54, in order.
    const KEYS: &str = "Esc 1 2 3 4 5 6 7 8 9 0 - = Backspace Tab Q W E R T Y U I O P [ ] Enter LCtrl \
A S D F G H J K L ; ' ` LShift \\ Z X C V B N M , . / RShift";
    let mut out = StackStr::<24>::new();
    let _ = match code {
        0 => write!(out, "None"),
        1..=54 => write!(out, "{}", KEYS.split(' ').nth(code as usize - 1).unwrap_or("?")),
        56 => write!(out, "LAlt"),
        57 => write!(out, "Space"),
        58 => write!(out, "Caps"),
        59..=68 => write!(out, "F{}", code - 58),
        74 => write!(out, "Num -"),
        78 => write!(out, "Num +"),
        87 => write!(out, "F11"),
        88 => write!(out, "F12"),
        97 => write!(out, "RCtrl"),
        100 => write!(out, "RAlt"),
        102 => write!(out, "Home"),
        103 => write!(out, "Up"),
        105 => write!(out, "Left"),
        106 => write!(out, "Right"),
        108 => write!(out, "Down"),
        0x110 => write!(out, "Left button"),
        0x111 => write!(out, "Right button"),
        0x112 => write!(out, "Middle button"),
        0x14a => write!(out, "Pen tip"),
        0x14b => write!(out, "Pen button 1"),
        0x14c => write!(out, "Pen button 2"),
        c if c >= 0x100 => write!(out, "Button {c:#x}"),
        c => write!(out, "Key {c}"),
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_match_the_evdev_layout_and_defaults_the_mcosu_ones() {
        assert_eq!(name(44).as_str(), "Z");
        assert_eq!(name(45).as_str(), "X");
        assert_eq!(name(46).as_str(), "C");
        assert_eq!(name(1).as_str(), "Esc");
        assert_eq!(name(30).as_str(), "A");
        assert_eq!(name(54).as_str(), "RShift");
        assert_eq!(name(59).as_str(), "F1");
        assert_eq!(name(78).as_str(), "Num +");
        assert_eq!(name(0).as_str(), "None");
        assert_eq!(name(0x14b).as_str(), "Pen button 1");
        assert!(!bindable(BTN_TOUCH) && !bindable(0) && bindable(44));
        let d = defaults();
        assert_eq!((d[LEFT_CLICK], d[RIGHT_CLICK], d[LEFT_CLICK_2]), (44, 45, 0));
        assert!(!is(&d, LEFT_CLICK_2, 0), "an unbound action matches nothing");
    }
}
