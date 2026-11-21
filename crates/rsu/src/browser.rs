//! The song browser, laid out as McOsu's (`OsuSongBrowser2`): beatmap sets
//! as a column of buttons hanging off the right edge, the selected set
//! opened into its difficulties; what is known about the selected map in
//! the top left, the scores set on it below that; a bar along the bottom
//! with the way back, mods, random and options.

use crate::library::{Library, MapInfo, Scan};
use crate::ranking::{grade_colour, grade_index, grade_letters, graded};
use crate::scores::{self, Score};
use crate::skin::Skin;
use crate::text::StackStr;
use crate::ui::{self, Backdrop, Input, Metrics, Rect, Scroll};
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use rsu_core::mods;
use rsu_formats::osu::PracticeDifficulty;
use rsu_gfx::Batch;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BrowserAction {
    None,
    /// Play the selected map.
    Start,
    /// Back to the main menu.
    Back,
    /// Open the mod selection.
    Mods,
    /// Open the options.
    Options,
    /// Show the result screen of a score (an index for `Browser::score`).
    ShowScore(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sort {
    Artist,
    Bpm,
    Creator,
    DateAdded,
    Difficulty,
    Length,
    Title,
}

impl Sort {
    pub const ALL: [Sort; 7] =
        [Sort::Artist, Sort::Bpm, Sort::Creator, Sort::DateAdded, Sort::Difficulty, Sort::Length, Sort::Title];

    pub fn label(self) -> &'static str {
        match self {
            Sort::Artist => "By Artist",
            Sort::Bpm => "By BPM",
            Sort::Creator => "By Creator",
            Sort::DateAdded => "By Date Added",
            Sort::Difficulty => "By Difficulty",
            Sort::Length => "By Length",
            Sort::Title => "By Title",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Sort::Artist => "artist",
            Sort::Bpm => "bpm",
            Sort::Creator => "creator",
            Sort::DateAdded => "date",
            Sort::Difficulty => "difficulty",
            Sort::Length => "length",
            Sort::Title => "title",
        }
    }

    pub fn named(name: &str) -> Option<Sort> {
        Sort::ALL.into_iter().find(|s| s.name() == name)
    }
}

/// How the list is divided under headings (McOsu's "Group").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Group {
    None,
    Artist,
    Creator,
    Title,
    Length,
    Difficulty,
    /// The player's collections in osu!.
    Collections,
}

impl Group {
    pub const ALL: [Group; 7] = [Group::None, Group::Artist, Group::Creator, Group::Difficulty, Group::Length, Group::Title, Group::Collections];
    const TABS: [Group; 4] = [Group::Collections, Group::Artist, Group::Difficulty, Group::None];

    pub fn label(self) -> &'static str {
        match self {
            Group::None => "No Grouping",
            Group::Artist => "By Artist",
            Group::Creator => "By Creator",
            Group::Title => "By Title",
            Group::Length => "By Length",
            Group::Difficulty => "By Difficulty",
            Group::Collections => "Collections",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Group::None => "none",
            Group::Artist => "artist",
            Group::Creator => "creator",
            Group::Title => "title",
            Group::Length => "length",
            Group::Difficulty => "difficulty",
            Group::Collections => "collections",
        }
    }

    pub fn named(name: &str) -> Option<Group> {
        Group::ALL.into_iter().find(|g| g.name() == name)
    }

    /// The heading a set belongs under, as a number: the order of the headings.
    fn of(self, face: &MapInfo) -> u32 {
        match self {
            Group::None | Group::Collections => 0,
            // Digits, then the letters, then everything else.
            Group::Artist | Group::Creator | Group::Title => match match self {
                Group::Artist => &face.artist,
                Group::Creator => &face.creator,
                _ => &face.title,
            }.chars().next().map(|c| c.to_ascii_uppercase()) {
                Some(c @ 'A'..='Z') => 1 + (c as u32 - 'A' as u32),
                Some('0'..='9') => 0,
                Some(_) => 27,
                None => NO_GROUP,
            },
            Group::Difficulty => (face.stars[0].max(0.0) as u32).min(11),
            Group::Length => [60_000, 120_000, 180_000, 240_000, 300_000, 600_000]
                .iter().position(|&limit| face.length_ms <= limit).unwrap_or(6) as u32,
        }
    }

    fn heading(self, g: u32, names: &[String], out: &mut StackStr<64>) {
        match self {
            Group::None => {}
            Group::Collections => {
                let _ = out.write_str(names.get(g as usize).map_or("", |n| n.as_str()));
            }
            Group::Artist | Group::Creator | Group::Title => {
                let _ = match g {
                    0 => out.write_str("0-9"),
                    27 => out.write_str("Other"),
                    g => out.write_char((b'A' + g as u8 - 1) as char),
                };
            }
            Group::Length => {
                let labels = ["1 minute or less", "2 minutes or less", "3 minutes or less", "4 minutes or less", "5 minutes or less", "10 minutes or less", "Over 10 minutes"];
                let _ = out.write_str(labels[g as usize]);
            }
            Group::Difficulty => {
                let _ = match g {
                    0 => write!(out, "Below 1 star"),
                    1 => write!(out, "1 star"),
                    11 => write!(out, "Above 10 stars"),
                    g => write!(out, "{g} stars"),
                };
            }
        }
    }
}

/// An entry of the list that is a heading rather than a set.
const HEADING: u32 = 1 << 31;
const GROUPS_MAX: usize = 256;
/// The heading of a set that is in no collection.
const NO_GROUP: u32 = u32::MAX;

const QUERY_MAX: usize = 64;
const KEY_A: u32 = 30;
/// How long a selection has to rest before its song starts: long enough
/// not to start every song that a held key runs past, short enough not to
/// be noticed. (Starting one costs a few milliseconds of decoding.)
const PREVIEW_DELAY_NS: u64 = 60_000_000;
/// Most scores listed for one beatmap.
const SCORES_SHOWN: usize = 64;

/// A button of the list as it stands in this frame.
#[derive(Clone, Copy, Debug, Default)]
struct Row {
    /// The set, when the button stands for a whole set; otherwise the map.
    set: Option<u32>,
    map: u32,
    selected: bool,
    /// Whether it is one of several difficulties of the opened set.
    child: bool,
    /// The heading, when the button is one.
    heading: Option<u32>,
    /// The visible body of the button, in window pixels.
    rect: Rect,
}

impl Row {
    fn key(self) -> u32 {
        match (self.heading, self.set) {
            (Some(g), _) => g | 1 << 30,
            (None, Some(set)) => set | 1 << 31,
            (None, None) => self.map,
        }
    }
}

/// What stays of a button from frame to frame while it is on screen.
#[derive(Clone, Copy, Default)]
struct RowAnim {
    key: u32,
    /// Whether the slot belongs to a button at all, and to one of this frame.
    used: bool,
    seen: bool,
    hover: f32,
    away: f32,
}

/// Where everything is, for the window as it is now.
#[derive(Clone, Copy, Debug, Default)]
struct Layout {
    margin: f32,
    /// Width of the column on the left (map details, scores).
    left_w: f32,
    /// Height of the map details.
    info_h: f32,
    /// The bar above the list.
    bar: Rect,
    bottom_h: f32,
    /// Where the list scrolls.
    view: Rect,
    /// Size of a button's image and of its visible body, and how far the
    /// body sits inside the image.
    image: (f32, f32),
    body: (f32, f32),
    inset: (f32, f32),
    /// Distance from one button to the next.
    pitch: f32,
    scores: Rect,
    score_h: f32,
    back: Rect,
    /// Mode, mods, random, options.
    nav: [Rect; 4],
    sort: Rect,
    group: Rect,
    /// The row of grouping choices under the bar.
    tabs: Rect,
}

impl Layout {
    fn tab(&self, i: usize) -> Rect {
        let w = self.tabs.w / Group::TABS.len() as f32;
        Rect::new(self.tabs.x + i as f32 * w, self.tabs.y, w, self.tabs.h)
    }

    fn group_item(&self, i: usize) -> Rect {
        Rect::new(self.group.x, self.group.y + (i + 1) as f32 * self.group.h, self.group.w, self.group.h)
    }
}

pub struct Browser {
    maps: Vec<MapInfo>,
    /// First map and number of maps of every set.
    sets: Vec<(u32, u32)>,
    /// Every set, in the chosen order.
    sorted: Vec<u32>,
    /// The sets with a map that matches the search, in that order (under
    /// each heading, when grouped).
    order: Vec<u32>,
    pub group: Group,
    /// Per set: the heading it is under.
    group_of: Vec<u32>,
    /// The collections (names, and which collection a map is in by its MD5).
    collection_names: Vec<String>,
    collection_of: std::collections::HashMap<[u8; 16], u32>,
    group_opened: [bool; GROUPS_MAX],
    group_counts: [u32; GROUPS_MAX],
    /// What the list is made of: sets and, when grouped, headings
    /// (`HEADING | n`) followed by the sets of the headings that are open.
    entries: Vec<u32>,
    group_menu: bool,
    /// Per map: whether it matches the search.
    matches: Vec<bool>,
    scores: Vec<Score>,
    /// The scores of the selected map, as indices into `scores`.
    shown_scores: Vec<u32>,
    query: String,
    pub sort: Sort,
    /// A sort that was asked for and is carried out between frames.
    resort: bool,
    /// Whether the list is made of single maps rather than sets.
    flat: bool,
    selected: Option<u32>,
    pub mods: u32,
    /// A playback rate of the player's own in percent, set in the mod
    /// selection (McOsu's speed multiplier); 0 leaves it to the mods.
    pub rate_pct: u16,
    pub practice: PracticeDifficulty,
    /// Let the game play by itself (McOsu's "Auto" mod).
    pub auto: bool,
    /// Whether Ctrl is held; the caller sets it before every `update`.
    pub ctrl: bool,
    /// Whether Shift is held; the caller sets it before every `update`.
    pub shift: bool,
    scan: Option<Scan>,
    scroll: Scroll,
    score_scroll: Scroll,
    changed_ns: u64,
    /// The set and audio file a preview was last asked for.
    previewing: Option<(u32, u32)>,
    /// The map whose background was last asked for.
    pictured: Option<u32>,
    /// Shown under the list: why something did not work.
    pub message: String,
    rows: [Row; 40],
    rows_len: usize,
    anims: [RowAnim; 48],
    layout: Layout,
    sort_open: bool,
    random: u64,
    /// Maps the player went on from with "next song", newest last.
    history: Vec<u32>,
    /// Maps selected before each random pick, newest last.
    random_history: Vec<u32>,
    /// Seconds the current background has been showing, for its fade.
    pub backdrop_age: f32,
    hovered_nav: [f32; 4],
    back_hover: f32,
    /// The button under the pointer, and how many times that or the
    /// selection has changed: each change makes a click.
    hovered_row: Option<u32>,
    pressed_row: Option<u32>,
    pub ticks: u32,
}

impl Browser {
    pub fn new(scan: Option<Scan>, sort: Sort) -> Browser {
        Browser {
            maps: Vec::new(),
            sets: Vec::new(),
            sorted: Vec::new(),
            order: Vec::new(),
            group: Group::None,
            group_of: Vec::new(),
            collection_names: Vec::new(),
            collection_of: std::collections::HashMap::new(),
            group_opened: [false; GROUPS_MAX],
            group_counts: [0; GROUPS_MAX],
            entries: Vec::new(),
            group_menu: false,
            matches: Vec::new(),
            scores: Vec::new(),
            shown_scores: Vec::with_capacity(SCORES_SHOWN),
            query: String::with_capacity(QUERY_MAX),
            sort,
            resort: false,
            flat: false,
            selected: None,
            mods: 0,
            rate_pct: 0,
            practice: PracticeDifficulty::default(),
            auto: false,
            ctrl: false,
            shift: false,
            scan,
            scroll: Scroll::default(),
            score_scroll: Scroll::default(),
            changed_ns: 0,
            previewing: None,
            pictured: None,
            message: String::new(),
            rows: [Row::default(); 40],
            rows_len: 0,
            anims: [RowAnim::default(); 48],
            layout: Layout::default(),
            sort_open: false,
            random: (0x9e37_79b9_7f4a_7c15 ^ rsu_rt::clock::now_ns()).max(1),
            history: Vec::new(),
            random_history: Vec::with_capacity(1000),
            backdrop_age: 0.0,
            hovered_nav: [0.0; 4],
            back_hover: 0.0,
            hovered_row: None,
            pressed_row: None,
            ticks: 0,
        }
    }

    /// Use this library. The maps must be grouped (`library::group`).
    pub fn set_library(&mut self, library: Library) {
        self.history.clear();
        self.random_history.clear();
        self.maps = library.maps;
        self.scores = library.scores;
        self.collection_names.clear();
        self.collection_of.clear();
        for (i, (name, md5s)) in library.collections.into_iter().take(GROUPS_MAX).enumerate() {
            self.collection_names.push(name);
            for md5 in md5s {
                // A map in several collections stays with the first.
                self.collection_of.entry(md5).or_insert(i as u32);
            }
        }
        scores::sort(&mut self.scores);
        self.flat = self.wants_flat();
        self.rebuild_sets();
        self.matches = vec![true; self.maps.len()];
        self.selected = None;
        self.apply_sort();
        self.refilter(0);
    }

    /// Take the library from the scanning thread when it is ready, and do
    /// whatever else was put off until the frame was out. Call outside
    /// the frame's hot section.
    pub fn between_frames(&mut self) {
        if let Some(library) = self.scan.as_ref().and_then(Scan::poll) {
            self.scan = None;
            self.set_library(library);
        }
        if std::mem::take(&mut self.resort) {
            self.apply_sort();
            if let (Some(map), true) = (self.selected, self.group != Group::None) {
                let g = self.group_of[self.set_of(map) as usize] as usize;
                if g < GROUPS_MAX {
                    self.group_opened[g] = true;
                }
            }
            self.refilter(self.changed_ns);
        }
    }

    /// Whether the difficulty is what the list goes by, sorted or grouped.
    fn wants_flat(&self) -> bool {
        self.sort == Sort::Difficulty || matches!(self.group, Group::Difficulty | Group::Length)
    }

    /// Divide the maps into the units the list is made of: sets, or single
    /// maps when sorting by difficulty (a set's maps lie far apart then).
    fn rebuild_sets(&mut self) {
        self.sets.clear();
        for (i, m) in self.maps.iter().enumerate() {
            match self.sets.last_mut() {
                Some((first, len)) if !self.flat && self.maps[*first as usize].set == m.set => *len += 1,
                _ => self.sets.push((i as u32, 1)),
            }
        }
        self.sorted = (0..self.sets.len() as u32).collect();
        self.order = Vec::with_capacity(self.sets.len());

        self.entries = Vec::with_capacity(self.sets.len() + GROUPS_MAX);
        self.group_of = vec![0; self.sets.len()];
    }

    /// The map a set is known by: its hardest.
    fn face(&self, set: u32) -> &MapInfo {
        let (first, len) = self.sets[set as usize];
        &self.maps[(first + len - 1) as usize]
    }

    fn apply_sort(&mut self) {
        if self.flat != self.wants_flat() {
            self.flat = !self.flat;
            self.rebuild_sets();
        }
        let lower = |s: &str| s.to_lowercase();
        let mut sorted = std::mem::take(&mut self.sorted);
        match self.sort {
            Sort::Artist => sorted.sort_by_cached_key(|&s| (lower(&self.face(s).artist), lower(&self.face(s).title))),
            Sort::Title => sorted.sort_by_cached_key(|&s| (lower(&self.face(s).title), lower(&self.face(s).artist))),
            Sort::Creator => sorted.sort_by_cached_key(|&s| (lower(&self.face(s).creator), lower(&self.face(s).title))),
            Sort::Bpm => sorted.sort_by(|&a, &b| self.face(a).bpm[2].total_cmp(&self.face(b).bpm[2])),
            Sort::Length => sorted.sort_by_key(|&s| self.face(s).length_ms),
            // Single maps, easiest first.
            Sort::Difficulty => sorted.sort_by(|&a, &b| {
                let key = |s: u32| (self.face(s).stars[0], self.face(s).objects);
                key(a).0.total_cmp(&key(b).0).then(key(a).1.cmp(&key(b).1))
            }),
            // Newest first.
            Sort::DateAdded => sorted.sort_by_key(|&s| {
                let (first, len) = self.sets[s as usize];
                std::cmp::Reverse(self.maps[first as usize..(first + len) as usize].iter().map(|m| m.added).max())
            }),
        }
        // Under headings: the order of the headings first, the chosen
        // order inside them (the sort is stable).
        if self.group != Group::None {
            for (s, g) in self.group_of.iter_mut().enumerate() {
                let (first, len) = (self.sets[s].0 as usize, self.sets[s].1 as usize);
                *g = match self.group {
                    // The first collection any map of the set is in.
                    Group::Collections => self.maps[first..first + len]
                        .iter()
                        .filter_map(|m| self.collection_of.get(&m.md5).copied())
                        .min()
                        .unwrap_or(NO_GROUP),
                    group => group.of(&self.maps[first + len - 1]),
                };
            }
            sorted.sort_by_key(|&s| self.group_of[s as usize]);
        }
        self.sorted = sorted;
    }

    /// Ask for another way of dividing the list.
    pub fn set_group(&mut self, group: Group) {
        self.resort |= group != self.group;
        self.group = group;
    }

    /// Make `entries` from `order`.
    fn rebuild_entries(&mut self) {
        self.entries.clear();
        self.group_counts = [0; GROUPS_MAX];
        if self.group == Group::None {
            self.entries.extend_from_slice(&self.order);
            return;
        }
        let searching = !self.query.is_empty();
        let mut last = None;
        let mut next_heading = 0;
        for &s in &self.order {
            let g = self.group_of[s as usize];
            if g == NO_GROUP {
                continue;
            }
            if last != Some(g) {
                last = Some(g);
                if !searching {
                    while next_heading < g {
                        self.entries.push(HEADING | next_heading);
                        next_heading += 1;
                    }
                    next_heading = g + 1;
                }
                self.entries.push(HEADING | g);
            }
            self.group_counts[g as usize] += 1;
            if searching || self.group_opened[g as usize] {
                self.entries.push(s);
            }
        }
        if !searching {
            let count = match self.group {
                Group::Artist | Group::Creator | Group::Title => 28,
                Group::Difficulty => 12,
                Group::Length => 7,
                Group::Collections => self.collection_names.len() as u32,
                _ => 0,
            };
            while next_heading < count {
                self.entries.push(HEADING | next_heading);
                next_heading += 1;
            }
        }
    }

    /// Ask for another order of the list.
    pub fn set_sort(&mut self, sort: Sort) {
        self.resort |= sort != self.sort;
        self.sort = sort;
    }

    fn refilter(&mut self, now_ns: u64) {
        // Every word of the query has to appear somewhere in the map's
        // text. Nothing here allocates: the vectors have room for all.
        for (m, hit) in self.maps.iter().zip(&mut self.matches) {
            *hit = self.query.split_whitespace().all(|w| m.haystack.contains(w));
        }
        self.order.clear();
        for &s in &self.sorted {
            let (first, len) = self.sets[s as usize];
            // A set without a heading is not visible in a grouped list.
            if self.group != Group::None && self.group_of[s as usize] == NO_GROUP {
                continue;
            }
            if self.matches[first as usize..(first + len) as usize].iter().any(|&hit| hit) {
                self.order.push(s);
            }
        }

        // A selection that fell out of the list gives way to the first
        // thing that is in it.
        let kept = self.selected.is_some_and(|m| self.matches[m as usize]);
        if !kept {
            match self.order.first() {
                Some(&set) => self.select_set(set, now_ns),
                None if self.maps.is_empty() => self.selected = None,
                None => {}
            }
        }
        self.rebuild_entries();
        self.centre_selection(true);
    }

    fn set_of(&self, map: u32) -> u32 {
        if self.flat {
            return map;
        }
        self.maps[map as usize].set
    }

    /// The maps of `set` that the list shows when it is open.
    fn children(&self, set: u32) -> impl DoubleEndedIterator<Item = u32> + '_ {
        let (first, len) = self.sets[set as usize];
        (first..first + len).filter(|&m| self.matches[m as usize] || self.selected == Some(m))
    }

    fn select_map(&mut self, map: u32, now_ns: u64) {
        if self.selected != Some(map) {
            self.ticks += u32::from(self.selected.is_some());
            self.selected = Some(map);
            // The heading of a chosen song opens.
            if self.group != Group::None {
                let g = self.group_of[self.set_of(map) as usize] as usize;
                if g < GROUPS_MAX && !std::mem::replace(&mut self.group_opened[g], true) {
                    self.rebuild_entries();
                }
            }
            self.changed_ns = now_ns;
            self.shown_scores.clear();
            let md5 = self.maps[map as usize].md5;
            let found = self.scores.iter().enumerate().filter(|(_, s)| s.md5 == md5).map(|(i, _)| i as u32);
            self.shown_scores.extend(found.take(SCORES_SHOWN));
            self.score_scroll.jump(0.0);
        }
    }

    /// Open a set on its hardest difficulty, as McOsu does.
    fn select_set(&mut self, set: u32, now_ns: u64) {
        let (first, len) = self.sets[set as usize];
        let hardest = (first..first + len).rev().find(|&m| self.matches[m as usize]).unwrap_or(first + len - 1);
        self.select_map(hardest, now_ns);
    }

    /// Where the opened set is in the list, and how many buttons it takes.
    fn opened(&self) -> Option<(usize, usize)> {
        let set = self.set_of(self.selected?);
        let at = self.entries.iter().position(|&s| s == set)?;
        Some((at, self.children(set).count()))
    }

    /// Distance from the top of the list to button `child` of entry `at`.
    fn row_y(&self, opened: Option<(usize, usize)>, at: usize, child: usize) -> f32 {
        let pitch = self.layout.pitch;
        match opened {
            Some((open, _)) if at == open => (at + child) as f32 * pitch + (child + 1) as f32 * 0.1 * pitch,
            Some((open, n)) if at > open => (at + n - 1) as f32 * pitch + (n + 1) as f32 * 0.1 * pitch,
            _ => at as f32 * pitch,
        }
    }

    fn content_height(&self, opened: Option<(usize, usize)>) -> f32 {
        let view = self.layout.view.h;
        view * 0.25 + self.row_y(opened, self.entries.len(), 0) + view * 0.5
    }

    /// Scroll so that the selected button is in the middle.
    fn centre_selection(&mut self, at_once: bool) {
        let opened = self.opened();
        let Some((at, _)) = opened else {
            if at_once {
                self.scroll.jump(0.0);
            }
            return;
        };
        let set = self.entries[at];
        let child = self.children(set).position(|m| Some(m) == self.selected).unwrap_or(0);
        let view = self.layout.view.h;
        let target = view * 0.25 + self.row_y(opened, at, child) + self.layout.pitch * 0.5 - view * 0.5;
        let target = target.clamp(0.0, (self.content_height(opened) - view).max(0.0));
        if at_once {
            self.scroll.jump(target);
        } else {
            self.scroll.to(target);
        }
    }

    pub fn selected_map(&self) -> Option<&MapInfo> {
        self.selected.map(|m| &self.maps[m as usize])
    }

    /// The first beat (milliseconds into the track) and the length of a
    /// beat of the selected map, for things that move to the music.
    pub fn beat(&self) -> Option<(f64, f64)> {
        self.selected_map().map(|m| m.beat).filter(|b| b.1 > 0.0)
    }

    /// The map to load a preview for, once per rested selection; a
    /// difficulty of the same song as before does not start it over.
    pub fn preview_due(&mut self, now_ns: u64) -> Option<&MapInfo> {
        let map = self.selected?;
        let m = &self.maps[map as usize];
        let mut first = map;
        while first > 0 && self.maps[first as usize - 1].set == m.set {
            first -= 1;
        }
        let same_audio = (first..=map).find(|&o| self.maps[o as usize].audio == m.audio).unwrap_or(map);
        let key = (m.set, same_audio);
        if self.previewing == Some(key) || now_ns.saturating_sub(self.changed_ns) < PREVIEW_DELAY_NS {
            return None;
        }
        self.previewing = Some(key);
        Some(m)
    }

    /// Forget which song is playing, so the preview starts again.
    pub fn restart_preview(&mut self, now_ns: u64) {
        self.previewing = None;
        self.changed_ns = now_ns;
    }

    /// The background image to load, once per selected map: its folder and
    /// the file name the map gives. `None` inside when it has none.
    pub fn picture_due(&mut self) -> Option<Option<(PathBuf, String)>> {
        let map = self.selected?;
        if self.pictured == Some(map) {
            return None;
        }
        self.pictured = Some(map);
        let m = &self.maps[map as usize];
        Some(
            (!m.background.is_empty())
                .then(|| (m.path.parent().unwrap_or(&m.path).to_path_buf(), m.background.clone())),
        )
    }

    pub fn score(&self, index: usize) -> Option<&Score> {
        self.scores.get(index)
    }

    /// A play just ended with this score. Call between frames.
    pub fn add_score(&mut self, score: Score) {
        self.scores.push(score);
        scores::sort(&mut self.scores);
        // The list of the selected map is made again.
        if let Some(map) = self.selected.take() {
            self.select_map(map, self.changed_ns);
        }
    }

    fn next_random(&mut self) -> u64 {
        self.random ^= self.random << 13;
        self.random ^= self.random >> 7;
        self.random ^= self.random << 17;
        self.random
    }

    fn select_random(&mut self, now_ns: u64) {
        if self.order.is_empty() {
            return;
        }
        if let Some(map) = self.selected {
            if self.random_history.len() == 1000 {
                self.random_history.remove(0);
            }
            self.random_history.push(map);
        }
        self.pick_random(now_ns);
    }

    fn previous_random(&mut self, now_ns: u64) {
        if let Some(map) = self.random_history.last().copied() {
            if !self.matches[map as usize] || !self.order.contains(&self.set_of(map)) {
                return;
            }
            self.random_history.pop();
            self.select_map(map, now_ns);
            self.centre_selection(false);
        }
    }

    fn pick_random(&mut self, now_ns: u64) {
        if !self.order.is_empty() {
            self.random ^= now_ns | 1;
            self.random = self.random.max(1);
            let pick = (self.next_random() % self.order.len() as u64) as usize;
            self.select_set(self.order[pick], now_ns);
            self.centre_selection(false);
        }
    }

    /// Whether there is a library to choose songs from.
    pub fn has_songs(&self) -> bool {
        !self.order.is_empty()
    }

    /// The music player's "next": another song, at random, remembering
    /// this one for "previous". Call between frames.
    pub fn next_song(&mut self, now_ns: u64) {
        if let Some(map) = self.selected {
            if self.history.len() >= 1000 {
                self.history.remove(0);
            }
            self.history.push(map);
        }
        let before = self.selected.map(|m| self.set_of(m));
        // Not the same song again, if there is any other.
        for _ in 0..4 {
            self.pick_random(now_ns);
            if self.order.len() < 2 || self.selected.map(|m| self.set_of(m)) != before {
                break;
            }
        }
    }

    /// The music player's "previous": the song played before the last "next".
    pub fn previous_song(&mut self, now_ns: u64) {
        while let Some(map) = self.history.pop() {
            if self.matches[map as usize] {
                self.select_map(map, now_ns);
                self.centre_selection(false);
                return;
            }
        }
    }

    /// Move the selection by `step` buttons (difficulties included) or by
    /// whole sets.
    fn step(&mut self, step: i32, whole_sets: bool, now_ns: u64) {
        let Some((at, _)) = self.opened() else {
            if let Some(&set) = self.entries.iter().find(|&&e| e & HEADING == 0) {
                self.select_set(set, now_ns);
            }
            return self.centre_selection(false);
        };
        let set = self.entries[at];
        if !whole_sets {
            let here = self.children(set).position(|m| Some(m) == self.selected).unwrap_or(0) as i32;
            if let Some(map) = (here + step >= 0).then(|| self.children(set).nth((here + step) as usize)).flatten() {
                self.select_map(map, now_ns);
                return self.centre_selection(false);
            }
        }
        // The next set in that direction; headings are passed over.
        let mut to = at as i64;
        loop {
            let next = to + step.signum() as i64;
            if next < 0 || next >= self.entries.len() as i64 {
                break;
            }
            to = next;
            // Walking into a shut heading opens it.
            let e = self.entries[to as usize];
            if e & HEADING != 0 && !self.group_opened[(e & !HEADING) as usize] {
                let g = (e & !HEADING) as usize;
                self.group_opened[g] = true;
                self.rebuild_entries();
                if step < 0 {
                    to += self.group_counts[g] as i64;
                }
            }
            if self.entries[to as usize] & HEADING == 0 {
                self.select_set(self.entries[to as usize], now_ns);
                break;
            }
        }
        self.centre_selection(false);
    }

    fn compute_layout(&self, m: &Metrics, skin: &Skin) -> Layout {
        let margin = m.px(5.0);
        // Keep the built-in row proportions; a skin paints this slot but
        // does not resize the list, its hitboxes, or the score rows.
        let (iw, ih) = (699.0, 103.0);
        let scale = m.image_scale(iw, ih, 64.0);
        let image = ((iw * scale).floor(), (ih * scale).floor());
        let inset = ((9.0 * scale).floor(), (9.0 * scale).floor());
        let body = (image.0 - 2.0 * inset.0, image.1 - 2.0 * inset.1);

        let px = m.px(ui::FONT_SUBTITLE);
        let info_h = (margin + ui::cap(px) * 1.25 + 4.0 * ui::cap(px * 0.78) * 1.9 + margin).max(m.h * 0.15);
        let left_w = (m.w * 0.265 + margin).floor();
        let bar = Rect::new(m.w * 0.378, 0.0, m.w * (1.0 - 0.378), (m.h * 0.075).floor());
        let bottom_h = (m.h * 0.116).floor();
        let view_x = left_w + 1.0 + margin;

        let score_h = (body.1 * 0.62).floor();
        let scores = Rect::new(0.0, info_h, left_w, (m.h - bottom_h - info_h).max(0.0));

        // The bottom bar: the way back on the left, then the buttons where
        // osu! has them (224 pixels in on a 1024x768 screen, 92 apart).
        let unit = m.h / 768.0;
        let back = match skin.menu.back.filter(|s| s.visible()) {
            Some(s) => {
                let h = (s.h * unit).min(bottom_h * 1.5);
                Rect::new(0.0, m.h - h, s.w * unit * h / (s.h * unit), h)
            }
            None => Rect::new(0.0, m.h - bottom_h * 0.72, m.osu(86.0), bottom_h * 0.72),
        };
        let mut nav = [Rect::default(); 4];
        for (i, r) in nav.iter_mut().enumerate().skip(1) {
            let x = m.osu(140.0 + 57.6 * (i - 1) as f32);
            *r = match skin.menu.selection[i].filter(|s| s.visible()) {
                Some(s) => Rect::new(x, m.h - s.h * unit, s.w * unit, s.h * unit),
                None => Rect::new(x, m.h - bottom_h * 0.8, m.osu(54.0), bottom_h * 0.8),
            };
        }
        let n = Group::TABS.len() as f32;
        let right = (m.w - m.px(10.0)).max(0.0);
        let tab_w = m.px(250.0).min(bar.w * 0.4).min(right / n);
        let tabs = Rect::new(right - n * tab_w, bar.h + 2.0, n * tab_w, m.px(30.0));
        // McOsu anchors Group over the second tab and Sort over the last.
        let dropdown_y = bar.h - m.px(34.0);
        let group = Rect::new(tabs.x + tab_w, dropdown_y, tab_w, tabs.h);
        let sort = Rect::new(tabs.x + (n - 1.0) * tab_w, dropdown_y, tab_w, tabs.h);
        // Like McOsu, start the list just under the tabs. Search is painted
        // over the list instead of reserving a second header.
        let view_y = tabs.y + tabs.h + 2.0;
        let view = Rect::new(view_x, view_y, m.w - view_x, (m.h - bottom_h - view_y).max(0.0));
        Layout {
            margin,
            left_w,
            info_h,
            bar,
            bottom_h,
            view,
            image,
            body,
            inset,
            pitch: body.1,
            scores,
            score_h,
            back,
            nav,
            sort,
            group,
            tabs,
        }
    }

    /// One frame of the browser: scrolling, the pointer, the keys.
    pub fn update(&mut self, input: &Input, m: &Metrics, skin: &Skin) -> BrowserAction {
        let before = self.layout.view;
        self.layout = self.compute_layout(m, skin);
        if before != self.layout.view {
            self.pressed_row = None;
            self.centre_selection(true);
        }
        let lay = self.layout;
        let now = input.now_ns;
        let mut action = BrowserAction::None;
        self.backdrop_age += input.dt;

        // The sort menu, while open, takes every click.
        let mut input = *input;
        if self.group_menu {
            if input.pressed {
                for (i, group) in Group::ALL.into_iter().enumerate() {
                    if input.over(lay.group_item(i)) {
                        self.set_group(group);
                    }
                }
                self.group_menu = false;
                input.pressed = false;
            }
            if input.key(ui::KEY_ESC) {
                self.group_menu = false;
                input.keys = &[];
            }
        } else if self.sort_open {
            if input.pressed {
                let item_h = lay.sort.h;
                for (i, sort) in Sort::ALL.into_iter().enumerate() {
                    let r = Rect::new(lay.sort.x, lay.sort.y + lay.sort.h + i as f32 * item_h, lay.sort.w, item_h);
                    if input.over(r) {
                        self.set_sort(sort);
                    }
                }
                self.sort_open = false;
                input.pressed = false;
            }
            if input.key(ui::KEY_ESC) {
                self.sort_open = false;
                input.keys = &[];
            }
        } else if input.clicked(lay.sort) {
            self.sort_open = true;
            input.pressed = false;
        } else if input.clicked(lay.group) {
            self.group_menu = true;
            input.pressed = false;
        } else if input.pressed {
            for (i, group) in Group::TABS.into_iter().enumerate() {
                if input.over(lay.tab(i)) {
                    self.set_group(group);
                    input.pressed = false;
                    break;
                }
            }
        }

        // The bars lie over the list and take the clicks that land on them.
        let on_bars = input.y >= m.h - lay.bottom_h || input.over(lay.bar) || input.x < lay.left_w;
        if input.pressed && input.y >= m.h - lay.bottom_h {
            if input.over(lay.back) {
                action = BrowserAction::Back;
            } else if input.over(lay.nav[1]) {
                action = BrowserAction::Mods;
            } else if input.over(lay.nav[2]) {
                if self.shift {
                    self.previous_random(now);
                } else {
                    self.select_random(now);
                }
            } else if input.over(lay.nav[3]) {
                action = BrowserAction::Options;
            }
        }
        ui::approach(&mut self.back_hover, input.over(lay.back) as u8 as f32, input.dt, 0.15);
        for (hover, r) in self.hovered_nav.iter_mut().zip(lay.nav) {
            ui::approach(hover, (input.y >= m.h - lay.bottom_h && input.over(r)) as u8 as f32, input.dt, 0.15);
        }

        // Scores.
        let score_content = self.shown_scores.len() as f32 * lay.score_h + m.px(10.0);
        let slop = m.px(12.0);
        if self.score_scroll.update(&input, lay.scores, score_content, lay.scores.h, lay.score_h, slop) {
            let row = ((input.y - lay.scores.y - m.px(5.0) + self.score_scroll.pos) / lay.score_h).floor();
            if row >= 0.0 && input.over(lay.scores) {
                if let Some(&score) = self.shown_scores.get(row as usize) {
                    action = BrowserAction::ShowScore(score as usize);
                }
            }
        }

        // The search overlay is display-only; pointer input in the viewport
        // goes to the list, including the rows underneath the search.
        let opened = self.opened();
        let content = self.content_height(opened);
        let list_area = if on_bars { Rect::default() } else { lay.view };
        let clicked = self.scroll.update(&input, list_area, content, lay.view.h, lay.pitch * 0.9, slop);
        let hit = self.place_rows(&input, opened, list_area);
        if input.pressed {
            self.pressed_row = hit.map(Row::key);
        }
        if !input.down {
            let pressed = self.pressed_row.take();
            if let Some(row) = hit.filter(|r| clicked && pressed == Some(r.key())) {
                match row.set {
                    _ if row.heading.is_some() => {
                        let g = row.heading.unwrap_or(0) as usize;
                        self.group_opened[g] ^= true;
                        self.rebuild_entries();
                    }
                    Some(set) => {
                        self.select_set(set, now);
                        self.centre_selection(false);
                    }
                    None if row.selected => {
                        // Ctrl + click plays it as Auto.
                        self.auto |= self.ctrl;
                        action = BrowserAction::Start;
                    }
                    None => {
                        self.select_map(row.map, now);
                        self.centre_selection(false);
                    }
                }
            }
        }

        for &code in input.keys {
            match code {
                // Ctrl+A: Auto on or off.
                KEY_A if self.ctrl => self.auto = !self.auto,
                // Ctrl+Backspace: the last word of the search.
                ui::KEY_BACKSPACE if self.ctrl => {
                    let kept = self.query.trim_end().rfind(' ').map_or(0, |i| i + 1);
                    self.query.truncate(kept);
                    self.refilter(now);
                }
                ui::KEY_UP => self.step(-1, false, now),
                ui::KEY_DOWN => self.step(1, false, now),
                ui::KEY_LEFT => self.step(-1, true, now),
                ui::KEY_RIGHT => self.step(1, true, now),
                ui::KEY_PAGE_UP => self.scroll.by(-lay.view.h),
                ui::KEY_PAGE_DOWN => self.scroll.by(lay.view.h),
                ui::KEY_ENTER if self.selected.is_some() => action = BrowserAction::Start,
                ui::KEY_F1 => action = BrowserAction::Mods,
                ui::KEY_F2 if self.shift => self.previous_random(now),
                ui::KEY_F2 => self.select_random(now),
                ui::KEY_F3 => action = BrowserAction::Options,
                ui::KEY_ESC if self.query.is_empty() => action = BrowserAction::Back,
                ui::KEY_ESC => {
                    self.query.clear();
                    self.refilter(now);
                }
                ui::KEY_BACKSPACE => {
                    if self.query.pop().is_some() {
                        self.refilter(now);
                    }
                }
                _ => {}
            }
        }
        // What was typed, in whatever layout the player has. Ctrl
        // combinations are shortcuts, not text.
        if !self.ctrl {
            let before = self.query.len();
            for c in input.text.chars() {
                // A search does not begin with a space.
                if self.query.len() < QUERY_MAX && !c.is_control() && (c != ' ' || !self.query.is_empty()) {
                    self.query.push(c);
                }
            }
            if self.query.len() != before {
                self.refilter(now);
            }
        }
        action
    }

    /// Work out which buttons are on screen and where, and move what is
    /// animated about them on by a frame.
    fn place_rows(&mut self, input: &Input, opened: Option<(usize, usize)>, area: Rect) -> Option<Row> {
        let lay = self.layout;
        let top = lay.view.y + lay.view.h * 0.25 - self.scroll.pos;
        self.rows_len = 0;
        for a in &mut self.anims {
            a.seen = false;
        }
        // The first entry that can be on screen: an opened set above it
        // only ever pushes things further down.
        let open_rows = opened.map_or(0, |(_, n)| n);
        let first = (((lay.view.y - top) / lay.pitch).floor() as i64 - open_rows as i64 - 2).max(0) as usize;

        'entries: for at in first..self.entries.len() {
            let set = self.entries[at];
            let heading = (set & HEADING != 0).then_some(set & !HEADING);
            if let Some(g) = heading {
                let y = top + self.row_y(opened, at, 0);
                if y > lay.view.y + lay.view.h + lay.pitch {
                    break;
                }
                if y + 2.0 * lay.pitch >= lay.view.y && self.rows_len < self.rows.len() {
                    let rect = Rect::new(0.0, y, lay.body.0, lay.body.1);
                    self.rows[self.rows_len] = Row { set: None, map: g, selected: false, child: false, heading, rect };
                    self.rows_len += 1;
                }
                continue;
            }
            let open = opened.is_some_and(|(o, _)| o == at);
            let (first_map, len) = self.sets[set as usize];
            let count = if open { opened.map_or(1, |(_, n)| n) } else { 1 };
            for child in 0..count {
                let y = top + self.row_y(opened, at, child);
                if y > lay.view.y + lay.view.h + lay.pitch {
                    break 'entries;
                }
                if y + 2.0 * lay.pitch < lay.view.y || self.rows_len == self.rows.len() {
                    continue;
                }
                let map = if open { self.children(set).nth(child).unwrap_or(first_map) } else { first_map + len - 1 };
                let selected = open && Some(map) == self.selected;
                let rect = Rect::new(0.0, y, lay.body.0, lay.body.1);
                self.rows[self.rows_len] =
                    Row { set: (!open).then_some(set), map, selected, child: open && len > 1, heading: None, rect };
                self.rows_len += 1;
            }
        }

        // Sideways: the buttons sit on a curve that bulges left in the
        // middle of the list, come out further under the pointer, and the
        // opened set stands out from the rest.
        let view = lay.view;
        let min_offset = view.w * (0.035 + 0.075) + (view.w - lay.body.0 * 0.85).max(0.0);
        let rect = |row: Row, anim: RowAnim| {
            let y = row.rect.y + lay.body.1 * 0.125 * anim.away;
            let middle = if view.h > 0.0 {
                1.0 - ((y + lay.body.1 * 0.5 - view.centre().1) / (view.h * 0.5)).abs().min(1.0)
            } else {
                0.0
            };
            // McOsu offsets every child of an expanded set, but only the
            // selected row when difficulties are listed independently.
            let pushed = if row.child || row.selected { 0.075 } else { 0.0 };
            let x = min_offset - view.w * (0.035 * middle + 0.075 * anim.hover + pushed);
            // The carousel positions the image canvas, not its hitbox.
            let x = x.clamp(0.0, (view.w - lay.body.0 * 0.15).max(0.0));
            Rect::new((view.x + x).floor() + lay.inset.0, y.floor(), lay.body.0, lay.body.1)
        };
        let rows = &mut self.rows[..self.rows_len];
        // Resolve input before applying the hover it causes. In an overlap,
        // the last drawn body is on top; the list scissor also gates input.
        let hovered = if input.over(area) && !self.scroll.dragging {
            rows.iter().rposition(|row| {
                let anim = self.anims.iter().find(|a| a.used && a.key == row.key()).copied().unwrap_or_default();
                input.over(rect(*row, anim))
            })
        } else {
            None
        };
        let hit = hovered.map(|i| rows[i]);
        let under = hit.map(Row::key);
        if under != self.hovered_row {
            self.ticks += u32::from(under.is_some());
            self.hovered_row = under;
        }
        for (i, row) in rows.iter_mut().enumerate() {
            let key = row.key();
            let slot = match self.anims.iter().position(|a| a.used && a.key == key) {
                Some(slot) => slot,
                None => {
                    // A button that just came into view takes the place of
                    // one that has left.
                    let free = self.anims.iter().position(|a| !a.used);
                    let slot = free.or(self.anims.iter().position(|a| !a.seen)).unwrap_or(0);
                    self.anims[slot] = RowAnim { key, used: true, seen: false, hover: 0.0, away: 0.0 };
                    slot
                }
            };
            let away = match hovered {
                Some(h) if i < h => -1.0,
                Some(h) if i > h => 1.0,
                _ => 0.0,
            };
            let anim = &mut self.anims[slot];
            anim.seen = true;
            ui::approach(&mut anim.hover, (hovered == Some(i)) as u8 as f32, input.dt, 0.35);
            ui::approach(&mut anim.away, away, input.dt, 0.5);
            row.rect = rect(*row, *anim);
        }
        for a in &mut self.anims {
            a.used &= a.seen;
        }
        hit
    }

    pub fn draw(&mut self, batch: &mut Batch, m: &Metrics, skin: &Skin, backdrop: &Backdrop, now_ns: u64, show_top: bool, show_bottom: bool) {
        let lay = self.layout;
        // The background of the selected map, a little darkened as McOsu
        // has it, fading in.
        let fade = 1.0 - (1.0 - (self.backdrop_age / 0.25).clamp(0.0, 1.0)).powi(2);
        backdrop.draw(batch, m, [0x99, 0x99, 0x99, (255.0 * fade) as u8]);

        let clip = |r: Rect| [r.x, r.y, r.x + r.w, r.y + r.h];
        batch.clipped(clip(lay.scores), |batch| self.draw_scores(batch, m, skin));
        batch.clipped(clip(lay.view), |batch| {
            for row in &self.rows[..self.rows_len] {
                self.draw_row(batch, m, skin, *row);
            }
            self.draw_search(batch, m);
        });

        // The top: a dark band behind the map details and the sort bar.
        if show_top {
            match skin.menu.top.filter(|s| s.visible()) {
                Some(top) => {
                    let scale = (m.w / top.w).max((lay.info_h * 1.05) / top.h);
                    top.draw_in(batch, 0.0, 0.0, top.w * scale, top.h * scale, ui::WHITE);
                }
                None if skin.menu.top.is_some() => {}
                None => {
                    batch.rect(0.0, 0.0, lay.left_w, lay.info_h, [0, 0, 0, 200]);
                    batch.rect(lay.left_w, 0.0, m.w - lay.left_w, lay.bar.h, [0, 0, 0, 200]);
                    batch.rect(0.0, lay.info_h, lay.left_w, m.px(2.0), [255, 255, 255, 40]);
                    batch.rect(lay.left_w, lay.bar.h, m.w - lay.left_w, m.px(2.0), [255, 255, 255, 40]);
                }
            }
        }
        self.draw_info(batch, m);
        self.draw_sort(batch, m);
        self.draw_group(batch, m);

        // The bottom bar.
        if show_bottom {
            batch.rect(0.0, m.h - lay.bottom_h + m.px(10.0), m.w, lay.bottom_h, ui::BLACK);
            match skin.menu.bottom.filter(|s| s.visible()) {
                Some(bottom) => bottom.draw_in(batch, 0.0, m.h - lay.bottom_h, m.w, lay.bottom_h, ui::WHITE),
                None if skin.menu.bottom.is_some() => {}
                None => {
                    batch.rect(0.0, m.h - lay.bottom_h, m.w, lay.bottom_h, ui::BLACK);
                    batch.rect(0.0, m.h - lay.bottom_h, m.w, m.px(2.0), [255, 255, 255, 60]);
                }
            }
        }
        draw_back(batch, m, skin, lay.back, self.back_hover);
        let labels = ["Mode", "Mods", "Random", "Options"];
        for i in 1..4 {
            let (r, hover) = (lay.nav[i], self.hovered_nav[i]);
            match skin.menu.selection[i].filter(|s| s.visible()) {
                Some(s) => {
                    s.draw_in(batch, r.x, r.y, r.w, r.h, ui::WHITE);
                    if let Some(over) = skin.menu.selection_over[i].filter(|s| s.visible()) {
                        let unit = m.h / 768.0;
                        let (w, h) = (over.w * unit, over.h * unit);
                        over.draw_in(batch, r.x, m.h - h, w, h, ui::alpha(ui::WHITE, hover));
                    }
                }
                None if skin.menu.selection[i].is_some() => {}
                None => {
                    let dim = 1.0;
                    let body = Rect::new(r.x + m.px(3.0), r.y + r.h * 0.2, r.w - m.px(6.0), r.h * 0.8 + m.px(4.0));
                    batch.rrect(body.x, body.y, body.w, body.h, m.px(4.0), [40, 40, 48, 255]);
                    let tint = [[150, 150, 160], [255, 102, 170], [102, 204, 102], [102, 170, 255]][i];
                    batch.rrect(body.x, body.y, body.w, m.px(4.0), m.px(2.0), ui::rgb(tint, dim * (0.6 + 0.4 * hover)));
                    let px = m.px(ui::FONT_DEFAULT) * 0.95;
                    let (cx, cy) = body.centre();
                    ui::centred(
                        batch,
                        cx,
                        cy - m.px(2.0),
                        px,
                        ui::alpha(ui::WHITE, dim * (0.75 + 0.25 * hover)),
                        labels[i],
                    );
                }
            }
        }
        // What is switched on, above the mods button.
        if self.mods != 0 || self.auto || self.rate_pct != 0 {
            let mut line = StackStr::<48>::new();
            let _ = write!(line, "{}{}", mods::name(self.mods), if self.auto { " AUTO" } else { "" });
            if self.rate_pct != 0 {
                let _ = write!(line, " {}x", self.rate_pct as f32 / 100.0);
            }
            let px = m.px(ui::FONT_DEFAULT);
            let (w, h) = (ui::text_width(px, line.as_str().trim()) + m.px(16.0), ui::cap(px) * 2.0);
            let (x, y) = (lay.nav[3].x + lay.nav[3].w + m.px(24.0), m.h - lay.bottom_h * 0.5 - h * 0.5);
            batch.rrect(x, y, w, h, h * 0.5, [255, 200, 60, 255]);
            ui::text(batch, x + m.px(8.0), y + (h - ui::cap(px)) * 0.5, px, ui::BLACK, line.as_str().trim());
        }
        if !self.message.is_empty() {
            let px = m.px(ui::FONT_DEFAULT);
            let y = m.h - lay.bottom_h - ui::cap(px) * 2.2;
            ui::shadowed(batch, lay.view.x + m.px(20.0), y, px, [255, 180, 70, 255], &self.message);
        }

        // Nothing to show: say why.
        let px = m.px(ui::FONT_SUBTITLE);
        if let Some(scan) = &self.scan {
            let mut line = StackStr::<96>::new();
            let (done, total) = (scan.done.load(Ordering::Relaxed), scan.total.load(Ordering::Relaxed));
            let _ = write!(line, "Loading beatmaps ... ({} %)", done * 100 / total.max(1));
            ui::centred(batch, m.w * 0.5, m.h - lay.bottom_h - ui::cap(px) * 2.0, px, ui::WHITE, line.as_str());
            // A wheel that turns while it loads.
            let turn = now_ns as f32 / 1e9 * 4.0;
            for k in 0..8 {
                let angle = turn + k as f32 * 0.6;
                let r = m.px(26.0);
                let a = (40 + 27 * k) as u8;
                batch.disc(m.w * 0.5 + angle.cos() * r, m.h * 0.5 + angle.sin() * r, m.px(5.0), [255, 255, 255, a], 0);
            }
        } else if self.maps.is_empty() {
            ui::centred(
                batch,
                m.w * 0.5,
                m.h * 0.5,
                px,
                [255, 60, 60, 255],
                "No beatmaps found (is the osu! folder right?)",
            );
            let hint = "Go to Options -> osu! folder, or start rsu with --songs DIR";
            ui::centred(batch, m.w * 0.5, m.h * 0.5 + ui::cap(px) * 2.5, px, [80, 255, 80, 255], hint);
        }
    }

    fn draw_row(&self, batch: &mut Batch, m: &Metrics, skin: &Skin, row: Row) {
        let lay = self.layout;
        let r = row.rect;
        if let Some(g) = row.heading {
            let back = [60, 170, 110, 240];
            match skin.menu.button.filter(|s| s.visible()) {
                Some(image) => {
                    image.draw_in(batch, r.x - lay.inset.0, r.y - lay.inset.1, lay.image.0, lay.image.1, back)
                }
                None if skin.menu.button.is_some() => {}
                None => {
                    batch.rrect(r.x, r.y + 1.0, r.w, r.h - 2.0, m.px(4.0), [back[0], back[1], back[2], back[3] - 20]);
                    batch.frame(r.x, r.y + 1.0, r.w, r.h - 2.0, m.px(4.0), (r.h * 0.02).max(1.0), [0, 0, 0, 70]);
                }
            }
            let ink = skin.menu.inactive_text;
            let x = r.x + r.w * 0.02 + m.px(4.0);
            let mut name = StackStr::<64>::new();
            self.group.heading(g, &self.collection_names, &mut name);
            let px = ui::em_for(r.h * 0.26);
            ui::text(batch, x, r.y + r.h * 0.2, px, ui::rgb(ink, 1.0), name.as_str());
            let mut line = StackStr::<32>::new();
            let n = self.group_counts[g as usize];
            let _ = write!(line, "{n} {}", if n == 1 { "song" } else { "songs" });
            let sub = ui::em_for(r.h * 0.15);
            ui::text(batch, x, r.y + r.h * 0.6, sub, ui::rgb(ink, 1.0), line.as_str());
            return;
        }
        let map = &self.maps[row.map as usize];
        // McOsu's colours: white when selected, pink for a set, blue for
        // the other difficulties of the opened set.
        let back = match (row.selected, row.child) {
            (true, _) => [255, 255, 255, 230],
            (false, true) => [0, 150, 236, 240],
            (false, false) => [235, 73, 153, 240],
        };
        match skin.menu.button.filter(|s| s.visible()) {
            Some(image) => image.draw_in(batch, r.x - lay.inset.0, r.y - lay.inset.1, lay.image.0, lay.image.1, back),
            None if skin.menu.button.is_some() => {}
            None => {
                batch.rrect(r.x, r.y + 1.0, r.w, r.h - 2.0, m.px(4.0), [back[0], back[1], back[2], back[3] - 20]);
                batch.frame(r.x, r.y + 1.0, r.w, r.h - 2.0, m.px(4.0), (r.h * 0.02).max(1.0), [0, 0, 0, 70]);
            }
        }
        let ink = if row.selected { skin.menu.active_text } else { skin.menu.inactive_text };
        // The names of the song are faint on the difficulties that are
        // not the selected one: there the difficulty's own name matters.
        let faint = if row.child && !row.selected { 0.2 } else { 1.0 };
        let x = r.x + r.w * 0.02 + m.px(4.0);
        let width = m.w - x - m.px(8.0);
        let title_px = ui::em_for(r.h * 0.20);
        let sub_px = ui::em_for(r.h * 0.125);
        ui::text(batch, x, r.y + r.h * 0.085, title_px, ui::rgb(ink, faint), ui::fit(title_px, &map.title, width));
        let mut line = StackStr::<160>::new();
        let _ = write!(line, "{} // {}", map.artist, map.creator);
        ui::text(batch, x, r.y + r.h * 0.375, sub_px, ui::rgb(ink, faint), ui::fit(sub_px, line.as_str(), width));
        if row.set.is_some() {
            return;
        }
        let diff_px = ui::em_for(r.h * 0.15);
        ui::text(batch, x, r.y + r.h * 0.58, diff_px, ui::rgb(ink, 1.0), ui::fit(diff_px, &map.version, width));

        // Stars: whole ones, a smaller last one for the fraction, and
        // faint ones up to ten.
        let stars = map.stars[0];
        if stars > 0.0 {
            let star_w = r.h * 0.2;
            let whole = (stars as usize).min(25);
            let part = (stars - whole as f32).clamp(0.0, 1.0).max(0.5);
            let cy = r.y + r.h * 0.85;
            for i in 0..10.max(whole + 1) {
                let (size, a) = match i {
                    i if i < whole => ((1.0 + i as f32 * 0.015).min(1.175), 1.0),
                    i if i == whole => (part, 1.0),
                    _ => (0.6, 0.12),
                };
                // Star images are mostly empty space around a small star,
                // so McOsu sets them wide apart; a drawn star needs less.
                match skin.menu.star.filter(|s| s.visible()) {
                    Some(s) => {
                        let cx = x + star_w * 0.5 + i as f32 * star_w * 1.75;
                        s.draw(batch, cx, cy, star_w / s.h * size, ui::rgb(ink, a));
                    }
                    None if skin.menu.star.is_some() => {}
                    None => {
                        let cx = x + star_w * 0.4 + i as f32 * star_w * 0.95;
                        batch.disc(cx, cy, star_w * 0.34 * size, ui::rgb(ink, a), 0);
                    }
                }
            }
        }
    }

    /// The details of the selected map, in the top left corner.
    fn draw_info(&self, batch: &mut Batch, m: &Metrics) {
        let lay = self.layout;
        let Some(map) = self.selected_map() else {
            return;
        };
        let px = m.px(ui::FONT_SUBTITLE);
        let small = px * 0.78;
        let x = lay.margin * 2.0;
        let mut y = lay.margin * 2.0;
        let mut line = StackStr::<240>::new();
        let _ = write!(line, "{} - {} [{}]", map.artist, map.title, map.version);
        ui::shadowed(batch, x, y, px, ui::WHITE, ui::fit(px, line.as_str(), m.w * 0.62));
        y += ui::cap(px) * 1.9;
        let step = ui::cap(small) * 1.9;
        let width = lay.left_w - 2.0 * x;
        let mut put = |batch: &mut Batch, colour: [u8; 4], line: &str| {
            ui::shadowed(batch, x, y, small, colour, ui::fit(small, line, width.max(m.w * 0.36)));
            y += step;
        };
        line.clear();
        let _ = write!(line, "Mapped by {}", map.creator);
        put(batch, ui::WHITE, line.as_str());

        // Speed mods change how long the map takes and how fast it is.
        let speed = mods::speed_with(self.mods, self.rate_pct);
        let changed = if speed > 1.0 { [255, 127, 127, 255] } else if speed < 1.0 { [173, 216, 230, 255] } else { ui::WHITE };
        let seconds = (map.length_ms as f64 / 1000.0 / speed) as u32;
        line.clear();
        let _ = write!(line, "Length: {}:{:02}  BPM: ", seconds / 60, seconds % 60);
        let [lo, hi, most] = map.bpm.map(|b| (b as f64 * speed).round() as u32);
        if lo == hi {
            let _ = write!(line, "{most}");
        } else {
            let _ = write!(line, "{lo}-{hi} ({most})");
        }
        let _ = write!(line, "  Objects: {}", map.objects);
        put(batch, changed, line.as_str());
        line.clear();
        let _ = write!(line, "Circles: {}  Sliders: {}  Spinners: {}", map.circles, map.sliders, map.spinners);
        put(batch, ui::WHITE, line.as_str());
        line.clear();
        // Hard rock and easy change the values; shown red and blue as in
        // McOsu.
        let trim = |v: f32| (v * 10.0).round() / 10.0;
        let base = rsu_core::Difficulty { cs: map.cs, od: map.od, ar: map.ar, hp: map.hp };
        let d = self.practice.apply(base, self.mods);
        let ar_lock = if self.practice.ar_lock { " (locked)" } else { "" };
        let od_lock = if self.practice.od_lock { " (locked)" } else { "" };
        let _ = write!(line, "CS:{}  AR:{}{ar_lock}  OD:{}{od_lock}  HP:{}", trim(d.cs), trim(d.ar), trim(d.od), trim(d.hp));
        let stars = if self.rate_pct == 0 {
            map.stars[rsu_formats::osudb::star_slot(self.mods)]
        } else {
            rsu_formats::osudb::stars_at(&map.stars, self.mods, mods::speed_with(self.mods, self.rate_pct))
        };
        if stars > 0.0 {
            // At a rate of the player's own it is an estimate.
            let _ = write!(line, "  Stars: {}{stars:.2}", if self.rate_pct == 0 { "" } else { "~" });
        }
        let colour = if self.mods & mods::EASY != 0 {
            [173, 216, 230, 255]
        } else if self.mods & mods::HARD_ROCK != 0 {
            [255, 127, 127, 255]
        } else {
            ui::WHITE
        };
        put(batch, colour, line.as_str());
    }

    /// The search, in the top right corner of the list.
    fn search_line_h(m: &Metrics) -> f32 {
        ui::cap(m.px(ui::FONT_SUBTITLE)) * 1.8
    }

    fn draw_search(&self, batch: &mut Batch, m: &Metrics) {
        let lay = self.layout;
        let px = m.px(ui::FONT_SUBTITLE);
        let line_h = Self::search_line_h(m);
        let typed = !self.query.is_empty();
        let hint = "Type to search!";
        let shown: &str = if typed { &self.query } else { hint };
        let label = "Search: ";
        let width = (ui::text_width(px, label) + ui::text_width(px, shown).max(ui::text_width(px, hint)))
            .max(ui::text_width(px, "No matches found. Hit ESC to reset."))
            + line_h;
        let width = width.min(lay.view.w.max(0.0));
        let right = lay.view.x + lay.view.w;
        let (x, y) = (right - width, lay.view.y);
        let lines = if typed { 2.6 } else { 1.7 };
        batch.rrect(x, y, width, line_h * lines, m.px(4.0), [0, 0, 0, if typed { 100 } else { 30 }]);
        let tx = x + line_h * 0.5;
        let end = ui::shadowed(batch, tx, y + line_h * 0.45, px, [0, 255, 0, 255], label);
        ui::shadowed(batch, end, y + line_h * 0.45, px, ui::WHITE, ui::fit(px, shown, right - end - line_h * 0.5));
        if typed {
            let mut line = StackStr::<64>::new();
            let found = self.matches.iter().filter(|&&hit| hit).count();
            match found {
                0 => {
                    let _ = write!(line, "No matches found. Hit ESC to reset.");
                }
                1 => {
                    let _ = write!(line, "1 match found!");
                }
                n => {
                    let _ = write!(line, "{n} matches found!");
                }
            }
            ui::shadowed(batch, tx, y + line_h * 1.45, px, ui::WHITE, line.as_str());
        }
    }

    fn draw_sort(&self, batch: &mut Batch, m: &Metrics) {
        let lay = self.layout;
        let px = m.px(ui::FONT_DEFAULT);
        let r = lay.sort;
        let label = "Sort:";
        ui::text(
            batch,
            r.x - ui::text_width(px, label) - m.px(8.0),
            r.y + (r.h - ui::cap(px)) * 0.5,
            px,
            [0, 200, 120, 255],
            label,
        );
        batch.rrect(r.x, r.y, r.w, r.h, m.px(4.0), [0, 0, 0, 160]);
        batch.frame(r.x, r.y, r.w, r.h, m.px(4.0), 1.0, [255, 255, 255, 150]);
        ui::centred(batch, r.x + r.w * 0.5, r.y + r.h * 0.5, px, ui::WHITE, self.sort.label());
        if self.sort_open {
            let n = Sort::ALL.len() as f32;
            batch.rrect(r.x, r.y + r.h, r.w, r.h * n, m.px(4.0), [20, 20, 24, 245]);
            batch.frame(r.x, r.y + r.h, r.w, r.h * n, m.px(4.0), 1.0, [255, 255, 255, 150]);
            for (i, sort) in Sort::ALL.into_iter().enumerate() {
                let y = r.y + r.h * (i + 1) as f32;
                let colour = if sort == self.sort { [0, 190, 255, 255] } else { ui::WHITE };
                ui::text(batch, r.x + m.px(12.0), y + (r.h - ui::cap(px)) * 0.5, px, colour, sort.label());
            }
        }
    }

    /// "Group:" and its choice in the bar; opened, the choices lie in a row
    /// under the bar, the current one green.
    fn draw_group(&self, batch: &mut Batch, m: &Metrics) {
        let lay = self.layout;
        let px = m.px(ui::FONT_DEFAULT);
        let r = lay.group;
        let label = "Group:";
        ui::text(
            batch,
            r.x - ui::text_width(px, label) - m.px(8.0),
            r.y + (r.h - ui::cap(px)) * 0.5,
            px,
            [0, 200, 120, 255],
            label,
        );
        batch.rrect(r.x, r.y, r.w, r.h, m.px(4.0), [0, 0, 0, 160]);
        batch.frame(r.x, r.y, r.w, r.h, m.px(4.0), 1.0, [255, 255, 255, 150]);
        let group_colour = if Group::TABS.contains(&self.group) { ui::WHITE } else { [0, 255, 0, 255] };
        ui::centred(batch, r.x + r.w * 0.5, r.y + r.h * 0.5, px, group_colour, self.group.label());
        for (i, group) in Group::TABS.into_iter().enumerate() {
            let t = lay.tab(i);
            batch.rrect(t.x, t.y, t.w, t.h, m.px(4.0), [20, 20, 24, 245]);
            batch.frame(t.x, t.y, t.w, t.h, m.px(4.0), 1.0, [255, 255, 255, 150]);
            let colour = if group == self.group { [0, 255, 0, 255] } else { ui::WHITE };
            ui::centred(batch, t.x + t.w * 0.5, t.y + t.h * 0.5, px, colour, group.label());
        }
        if self.group_menu {
            for (i, group) in Group::ALL.into_iter().enumerate() {
                let t = lay.group_item(i);
                batch.rrect(t.x, t.y, t.w, t.h, m.px(4.0), [20, 20, 24, 245]);
                batch.frame(t.x, t.y, t.w, t.h, m.px(4.0), 1.0, [255, 255, 255, 150]);
                let colour = if group == self.group { [0, 255, 0, 255] } else { ui::WHITE };
                ui::centred(batch, t.x + t.w * 0.5, t.y + t.h * 0.5, px, colour, group.label());
            }
        }
    }

    /// The scores set on the selected map, best first.
    fn draw_scores(&self, batch: &mut Batch, m: &Metrics, skin: &Skin) {
        let lay = self.layout;
        let area = lay.scores;
        if self.selected.is_none() {
            return;
        }
        if self.shown_scores.is_empty() {
            let px = m.px(ui::FONT_SUBTITLE);
            let (w, h) = (area.w * 0.9, lay.score_h * 0.9);
            let (x, y) = (area.x + (area.w - w) * 0.5, area.y + area.h * 0.3 - h * 0.5);
            batch.rrect(x, y, w, h, m.px(4.0), [0, 0, 0, 120]);
            ui::centred(batch, x + w * 0.5, y + h * 0.5, px, [220, 220, 220, 255], "No records set!");
            return;
        }
        let unix_now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let pad = m.px(5.0);
        let mut line = StackStr::<96>::new();
        for (i, &index) in self.shown_scores.iter().enumerate() {
            let y = area.y + pad + i as f32 * lay.score_h - self.score_scroll.pos;
            if y + lay.score_h < area.y || y > area.y + area.h {
                continue;
            }
            let score = &self.scores[index as usize];
            let s = &score.summary;
            let r = Rect::new(area.x + pad, y, area.w - pad, lay.score_h - m.px(3.0));
            batch.rrect(r.x, r.y, r.w, r.h, m.px(4.0), [0, 0, 0, 150]);
            // The grade, as the skin draws it or as a letter.
            let grade = graded(s, score.mods);
            let gx = r.x + r.h * 0.55;
            match skin.menu.grades_small[grade_index(grade)].filter(|g| g.visible()) {
                Some(g) => g.draw(batch, gx, r.y + r.h * 0.5, (r.h * 0.6 / g.h).min(r.h * 0.9 / g.w), ui::WHITE),
                None if skin.menu.grades_small[grade_index(grade)].is_some() => {}
                None => ui::centred(batch, gx, r.y + r.h * 0.5, r.h * 0.6, grade_colour(grade), grade_letters(grade)),
            }
            let tx = r.x + r.h * 1.15;
            let (big, small) = (ui::em_for(r.h * 0.26), ui::em_for(r.h * 0.2));
            line.clear();
            let name = if score.player.is_empty() { "Player" } else { score.player.as_str() };
            let _ = write!(line, "#{} {name}", i + 1);
            ui::shadowed(batch, tx, r.y + r.h * 0.14, big, ui::WHITE, ui::fit(big, line.as_str(), r.w * 0.5));
            line.clear();
            let _ = write!(line, "Score: ");
            grouped(&mut line, s.score);
            let _ = write!(line, " ({}x)", s.max_combo);
            ui::shadowed(batch, tx, r.y + r.h * 0.58, small, ui::WHITE, line.as_str());
            // On the right: when, the mods, the accuracy.
            let right = r.x + r.w - m.px(8.0);
            line.clear();
            scores::ago(score.unix_time, unix_now as i64, &mut line);
            ui::right_aligned(batch, right, r.y + r.h * 0.1, small * 0.85, [200, 200, 200, 255], line.as_str());
            if score.stable || score.mods != 0 || score.rate_pct != 0 {
                let mut names = rsu_formats::osr::mod_names(score.mods);
                if score.stable {
                    if !names.is_empty() { names.push(' '); }
                    names.push_str("Classic");
                }
                if score.rate_pct != 0 {
                    let _ = write!(names, " {}x", score.rate_pct as f32 / 100.0);
                }
                ui::right_aligned(batch, right, r.y + r.h * 0.4, small * 0.85, [255, 200, 80, 255], names.trim());
            }
            line.clear();
            let _ = write!(line, "{:.2}%", s.accuracy() * 100.0);
            ui::right_aligned(batch, right, r.y + r.h * 0.68, small * 0.85, [255, 230, 120, 255], line.as_str());
        }
    }
}

/// The way back, in the bottom left corner: the skin's image or a tab.
pub fn draw_back(batch: &mut Batch, m: &Metrics, skin: &Skin, r: Rect, hover: f32) {
    match skin.menu.back.filter(|s| s.visible()) {
        Some(s) => {
            s.draw_in(batch, r.x, r.y, r.w, r.h, ui::WHITE);
            if hover > 0.0 {
                batch.rect(r.x, r.y, r.w, r.h, [255, 255, 255, (38.0 * hover) as u8]);
            }
        }
        None => {
            // Back is navigation, not a decorative layer. Keep it visible
            // when a skin supplies an empty sprite, just as when it is missing.
            let grow = 1.0 + 0.12 * hover;
            batch.rrect(r.x - r.h, r.y, r.w * grow + r.h, r.h + r.h, r.h * 0.3, [238, 51, 153, 255]);
            batch.rrect(r.x - r.h, r.y, r.h * 1.75 * grow, r.h + r.h, r.h * 0.3, [0, 0, 0, 60]);
            let px = m.px(ui::FONT_SUBTITLE);
            ui::centred(batch, r.x + r.h * 0.38 * grow, r.y + r.h * 0.5, px, ui::WHITE, "<");
            ui::centred(batch, r.x + (r.h * 0.75 + r.w) * 0.5 * grow, r.y + r.h * 0.5, px * 0.9, ui::WHITE, "back");
        }
    }
}

/// Write `n` with its digits in groups of three.
pub fn grouped(out: &mut impl Write, n: u64) {
    let mut digits = StackStr::<24>::new();
    let _ = write!(digits, "{n}");
    let digits = digits.as_str();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            let _ = out.write_char(',');
        }
        let _ = out.write_char(c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library;

    fn map(folder: &str, artist: &str, title: &str, version: &str, objects: u32) -> MapInfo {
        MapInfo {
            path: PathBuf::from(format!("/songs/{folder}/{version}.osu")),
            artist: artist.into(),
            title: title.into(),
            version: version.into(),
            creator: "mapper".into(),
            objects,
            length_ms: 90_000 + objects,
            preview_ms: -1,
            audio: "audio.mp3".into(),
            md5: [objects as u8; 16],
            added: objects as u64,
            haystack: format!("{artist} {title} {version} mapper").to_lowercase(),
            ..MapInfo::default()
        }
    }

    fn browser() -> Browser {
        let mut maps = vec![
            map("1", "DECO*27", "Ghost Rule", "Insane", 300),
            map("1", "DECO*27", "Ghost Rule", "Hard", 200),
            map("2", "Phoneboy", "Nevermind", "Hayakou's Extreme", 500),
            map("3", "xi", "FREEDOM DiVE", "Another", 900),
            map("3", "xi", "FREEDOM DiVE", "FOUR DIMENSIONS", 1500),
            map("3", "xi", "FREEDOM DiVE", "Normal", 400),
        ];
        library::group(&mut maps);
        let mut b = Browser::new(None, Sort::Artist);
        b.layout = b.compute_layout(&Metrics::new(1920.0, 1080.0), &Skin::default());
        b.set_library(Library { maps, scores: Vec::new(), collections: Vec::new() });
        b
    }

    #[test]
    fn info_renders_practice_ar_after_hr_and_ez() {
        fn info(b: &Browser) -> String {
            let mut store = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 4096];
            let mut batch = unsafe { Batch::over(store.as_mut_ptr().cast(), store.len()) };
            b.draw_info(&mut batch, &Metrics::new(1920.0, 1080.0));
            format!("{:?}", unsafe { std::slice::from_raw_parts(store.as_ptr().cast::<rsu_gfx::Inst>(), batch.len()) })
        }
        for (game_mod, base) in [(mods::HARD_ROCK, 5.0), (mods::EASY, 14.0)] {
            let mut b = browser();
            b.mods = game_mod;
            let index = b.selected.unwrap() as usize;
            b.maps[index].ar = 4.0;
            let original = info(&b);
            b.practice.ar = Some(7.0);
            let actual = info(&b);
            b.practice.ar = None;
            b.maps[index].ar = base;
            assert_ne!(original, actual);
            assert_eq!(actual, info(&b));
        }
    }

    #[test]
    fn info_renders_practice_od_after_mods() {
        fn info(b: &Browser) -> String {
            let mut store = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 4096];
            let mut batch = unsafe { Batch::over(store.as_mut_ptr().cast(), store.len()) };
            b.draw_info(&mut batch, &Metrics::new(1920.0, 1080.0));
            let len = batch.len();
            format!("{:?}", unsafe { std::slice::from_raw_parts(store.as_ptr().cast::<rsu_gfx::Inst>(), len) })
        }
        let mut b = browser();
        b.mods = mods::HARD_ROCK;
        let index = b.selected.unwrap() as usize;
        b.maps[index].od = 4.0;
        let original = info(&b);
        b.practice.od = Some(7.0);
        let actual = info(&b);
        b.practice.od = None;
        b.maps[index].od = 5.0;
        assert_ne!(original, info(&b));
        assert_eq!(actual, info(&b));
    }

    #[test]
    fn sort_control_fill_and_frame_have_scaled_rounding() {
        let m = Metrics::new(1920.0, 1080.0);
        let b = browser();
        let mut store = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 4096];
        let mut batch = unsafe { Batch::over(store.as_mut_ptr().cast(), store.len()) };
        b.draw_sort(&mut batch, &m);
        let len = batch.len();
        let shapes = unsafe { std::slice::from_raw_parts(store.as_ptr().cast::<rsu_gfx::Inst>(), len) };
        for color in [[0, 0, 0, 160], [255, 255, 255, 150]] {
            let shape = shapes.iter().find(|s| s.color == color).unwrap();
            assert_eq!(shape.p0, m.px(4.0));
        }
    }

    #[test]
    fn list_starts_under_tabs_and_keeps_its_view_when_typing_or_clearing() {
        for (w, h) in [(960.0, 720.0), (1280.0, 720.0), (1920.0, 1080.0)] {
            let m = Metrics::new(w, h);
            let mut b = browser();
            let skin = Skin::default();
            b.update(&input(&[]), &m, &skin);
            let lay = b.layout;
            assert_eq!(lay.view.y, lay.tabs.y + lay.tabs.h + 2.0);
            assert!(lay.view.y + lay.view.h <= h - lay.bottom_h);
            let selected = b.selected;
            assert_eq!(b.selected_map().unwrap().title, "Ghost Rule");
            b.update(&Input { text: "ghost", ..input(&[]) }, &m, &skin);
            assert_eq!(b.query, "ghost");
            assert_eq!(b.layout.view, lay.view);
            assert_eq!(b.selected, selected);
            assert_eq!(b.update(&input(&[ui::KEY_ESC]), &m, &skin), BrowserAction::None);
            assert!(b.query.is_empty());
            assert_eq!(b.layout.view, lay.view);
            assert_eq!(b.selected, selected);
        }
    }

    #[test]
    fn song_select_dropdowns_line_up_with_the_reference_tabs() {
        let mut b = browser();
        for (w, h) in [(960.0, 720.0), (1280.0, 720.0), (1920.0, 1080.0)] {
            b.update(&input(&[]), &Metrics::new(w, h), &Skin::default());
            let lay = b.layout;
            for (dropdown, i) in [(lay.group, 1), (lay.sort, 3)] {
                let tab = lay.tab(i);
                assert!((dropdown.x - tab.x).abs() < 0.01, "{w}x{h}: dropdown must align with tab {i}");
                assert_eq!(dropdown.w, tab.w);
            }
        }
    }

    #[test]
    fn carousel_offsets_follow_mcosu_for_sets_and_difficulties_across_resize() {
        let mut b = browser();
        let skin = Skin::default();
        let idle = Input { dt: 0.0, ..input(&[]) };
        let selected = b.selected;
        for (w, h) in [(1280.0, 720.0), (960.0, 720.0), (1600.0, 900.0), (2560.0, 1080.0)] {
            let m = Metrics::new(w, h);
            b.update(&idle, &m, &skin);
            assert_eq!(b.selected, selected, "resize must keep the same difficulty");
            let view = b.layout.view;
            for row in &b.rows[..b.rows_len] {
                let fraction = (2.0 * (row.rect.centre().1 - view.centre().1).abs() / view.h).min(1.0);
                let forward = if row.child || row.selected { 0.075 } else { 0.0 };
                let x = (view.w - row.rect.w * 0.85).max(0.0) + view.w * (0.075 + 0.035 * fraction - forward);
                let expected = view.x + x.clamp(0.0, (view.w - row.rect.w * 0.15).max(0.0)) + b.layout.inset.0;
                assert!((row.rect.x - expected).abs() <= 1.1, "{w}x{h} {row:?}: expected x={expected}");
            }
            assert!(b.rows[..b.rows_len].iter().any(|r| r.child && !r.selected));
            assert!(b.rows[..b.rows_len].iter().any(|r| r.set.is_some()));
        }
    }

    #[test]
    fn carousel_margin_does_not_hover_or_arm_a_row() {
        let m = Metrics::new(1280.0, 720.0);
        let skin = Skin::default();
        let mut b = browser();
        let idle = Input { dt: 0.0, ..input(&[]) };
        b.update(&idle, &m, &skin);
        let row = b.rows[..b.rows_len].iter().find(|row| row.selected).unwrap().rect;
        let x = row.x - 1.0;
        let y = row.centre().1;
        b.update(&Input { x, y, dt: 1.0, ..idle }, &m, &skin);
        assert!(b.hovered_row.is_none(), "the transparent canvas margin is not a row hit");

        let press = Input { x, y, pressed: true, down: true, ..idle };
        b.update(&press, &m, &skin);
        let inside = Input { x: row.x + 4.0, y, ..idle };
        assert_eq!(b.update(&inside, &m, &skin), BrowserAction::None, "release alone must not activate a row");

        b.update(&Input { pressed: true, down: true, ..inside }, &m, &skin);
        assert_eq!(b.update(&inside, &m, &skin), BrowserAction::Start);
        assert_eq!(version(&b), "Insane");
    }

    #[test]
    fn search_overlay_passes_list_input_but_clipped_rows_do_not() {
        let m = Metrics::new(1280.0, 720.0);
        let skin = Skin::default();
        let mut b = browser();
        let idle = Input { dt: 0.0, ..input(&[]) };
        b.update(&idle, &m, &skin);
        b.update(&Input { text: "freedom", ..idle }, &m, &skin);
        let lay = b.layout;
        let top = lay.tabs.y + lay.tabs.h + 2.0;
        let x = m.w - m.px(12.0);
        let wheel = Input { x, y: top + 4.0, dt: 0.05, scroll: -1.0, ..idle };
        let resting = b.scroll.pos;
        b.update(&Input { y: top - 1.0, ..wheel }, &m, &skin);
        assert_eq!(b.scroll.pos, resting, "wheel above the list must not scroll it");
        b.update(&wheel, &m, &skin);
        assert!(b.scroll.pos < resting, "wheel over the search overlay scrolls the list");

        // Place the selected row across the top scissor, as during a drag.
        b.update(&idle, &m, &skin);
        let row = b.rows[..b.rows_len].iter().find(|row| row.selected).unwrap().rect;
        b.scroll.jump(b.scroll.pos + row.y - (top - 10.0));
        b.update(&idle, &m, &skin);
        let row = b.rows[..b.rows_len].iter().find(|row| row.selected).unwrap().rect;
        let selected = b.selected;
        for (y, expected) in [(top - 1.0, BrowserAction::None), (top + 4.0, BrowserAction::Start)] {
            let press = Input { x, y, pressed: true, down: true, ..idle };
            assert!(press.over(row), "the raw row spans both sides of the scissor");
            assert_eq!(b.update(&press, &m, &skin), BrowserAction::None);
            let release = Input { pressed: false, down: false, ..press };
            assert_eq!(b.update(&release, &m, &skin), expected);
            assert_eq!(b.selected, selected);
        }
    }

    #[test]
    fn local_scores_start_below_details_without_reserved_leaderboard_space() {
        let m = Metrics::new(1280.0, 720.0);
        let mut b = browser();
        let skin = Skin::default();
        b.update(&input(&[]), &m, &skin);
        assert_eq!(b.layout.scores.y, b.layout.info_h);
    }

    #[test]
    fn transparent_back_keeps_a_visible_song_select_control() {
        let m = Metrics::new(1280.0, 720.0);
        let mut b = browser();
        let mut skin = Skin::default();
        skin.menu.back = Some(crate::skin::Sprite::default());
        b.update(&input(&[]), &m, &skin);
        let mut store = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 16384];
        let mut batch = unsafe { Batch::over(store.as_mut_ptr().cast(), store.len()) };
        b.draw(&mut batch, &m, &skin, &Backdrop::default(), 0, true, true);
        let shapes = unsafe { std::slice::from_raw_parts(store.as_ptr().cast::<rsu_gfx::Inst>(), batch.len()) };
        let (x, y) = b.layout.back.centre();
        assert!(shapes.iter().any(|shape| shape.color == [238, 51, 153, 255]
            && (shape.center[0] - x).abs() < shape.half[0]
            && (shape.center[1] - y).abs() < shape.half[1]), "Back must remain visible");
        let press = Input { x, y, pressed: true, down: true, ..input(&[]) };
        assert_eq!(b.update(&press, &m, &skin), BrowserAction::Back);
    }

    #[test]
    fn mode_skin_sprite_does_not_change_songselect_rendering() {
        let m = Metrics::new(1920.0, 1080.0);
        let mut b = browser();
        let mut skin = Skin::default();
        let count = |b: &mut Browser, skin: &Skin| {
            let mut store = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 16384];
            let mut batch = unsafe { Batch::over(store.as_mut_ptr().cast(), store.len()) };
            b.draw(&mut batch, &m, skin, &Backdrop::default(), 0, true, true);
            batch.len()
        };
        let plain = count(&mut b, &skin);
        skin.menu.selection[0] = Some(crate::skin::Sprite { w: 100.0, h: 20.0, ..Default::default() });
        assert_eq!(count(&mut b, &skin), plain);
    }

    #[test]
    fn hidden_songselect_panels_remove_backgrounds_but_not_controls() {
        let m = Metrics::new(1920.0, 1080.0);
        let mut b = browser();
        let mut skin = Skin::default();
        skin.menu.top = Some(crate::skin::Sprite { w: 100.0, h: 20.0, ..Default::default() });
        skin.menu.bottom = Some(crate::skin::Sprite { w: 100.0, h: 20.0, ..Default::default() });
        let counts = |b: &mut Browser, skin: &Skin, top, bottom| {
            let mut store = vec![std::mem::MaybeUninit::<rsu_gfx::Inst>::uninit(); 16384];
            let mut batch = unsafe { Batch::over(store.as_mut_ptr().cast(), store.len()) };
            b.draw(&mut batch, &m, skin, &Backdrop::default(), 0, top, bottom);
            batch.len()
        };
        let both = counts(&mut b, &skin, true, true);
        let no_top = counts(&mut b, &skin, false, true);
        let no_bottom = counts(&mut b, &skin, true, false);
        let neither = counts(&mut b, &skin, false, false);
        assert_eq!(both - no_top, 1);
        assert_eq!(both - no_bottom, 2); // sprite and black underlay
        assert_eq!(both - neither, 3);
        assert!(neither > 0); // controls and other content still draw
        let plain = Skin::default();
        let both = counts(&mut b, &plain, true, true);
        assert_eq!(both - counts(&mut b, &plain, false, true), 4);
        assert_eq!(both - counts(&mut b, &plain, true, false), 3);
    }

    fn input(keys: &[u32]) -> Input<'_> {
        Input { x: -1.0, y: -1.0, pressed: false, down: false, scroll: 0.0, keys, text: "", dt: 0.01, now_ns: 0 }
    }

    fn type_text(b: &mut Browser, text: &str) -> BrowserAction {
        let typed = Input { text, ..input(&[]) };
        b.update(&typed, &Metrics::new(1920.0, 1080.0), &Skin::default())
    }

    #[test]
    fn typed_text_may_be_any_script() {
        let mut b = browser();
        type_text(&mut b, " Фу ");
        assert_eq!(b.query, "Фу ");
    }

    fn press(b: &mut Browser, key: u32) -> BrowserAction {
        b.update(&input(&[key]), &Metrics::new(1920.0, 1080.0), &Skin::default())
    }

    fn version(b: &Browser) -> &str {
        &b.selected_map().unwrap().version
    }

    #[test]
    fn changing_song_row_art_preserves_selection_geometry_and_click() {
        for m in [Metrics::new(1280.0, 720.0), Metrics::new(1920.0, 1080.0)] {
            let mut plain = browser();
            let idle = Input { dt: 0.0, ..input(&[]) };
            let default = Skin::default();
            plain.update(&idle, &m, &default);
            plain.update(&Input { text: "freedom", ..idle }, &m, &default);
            plain.centre_selection(true);
            plain.update(&idle, &m, &default);
            let selected_rect = |b: &Browser| b.rows[..b.rows_len].iter().find(|row| row.selected).unwrap().rect;
            let rect = selected_rect(&plain);
            let (x, y) = (rect.x + m.px(12.0), rect.y + rect.h * 0.5);

            for (w, h) in [(699.0, 103.0), (900.0, 40.0), (280.0, 180.0), (0.0, 0.0)] {
                let mut skin = Skin::default();
                skin.menu.button = Some(crate::skin::Sprite { w, h, ..Default::default() });
                assert_eq!(plain.update(&idle, &m, &skin), BrowserAction::None);
                let layout = plain.compute_layout(&m, &default);
                assert_eq!(plain.layout.pitch, layout.pitch, "asset size {w} x {h}");
                assert_eq!(plain.layout.score_h, layout.score_h);
                assert_eq!(selected_rect(&plain), rect);
                assert_eq!(version(&plain), "FOUR DIMENSIONS");
                assert_eq!(plain.query, "freedom");
                let press = Input { x, y, pressed: true, down: true, ..idle };
                assert_eq!(plain.update(&press, &m, &skin), BrowserAction::None);
                let release = Input { pressed: false, down: false, ..press };
                assert_eq!(plain.update(&release, &m, &skin), BrowserAction::Start);
                plain.update(&idle, &m, &skin);
            }
        }
    }

    #[test]
    fn shift_f2_returns_to_the_map_before_random() {
        let mut b = browser();
        press(&mut b, ui::KEY_UP);
        let original = b.selected;
        for _ in 0..20 {
            press(&mut b, ui::KEY_F2);
            if b.selected != original {
                break;
            }
        }
        assert_ne!(b.selected, original);
        b.shift = true;
        press(&mut b, ui::KEY_F2);
        assert_eq!(b.selected, original);
    }

    #[test]
    fn shift_f2_does_nothing_before_any_random_picks() {
        let mut b = browser();
        let original = b.selected;
        b.shift = true;
        press(&mut b, ui::KEY_F2);
        assert_eq!(b.selected, original);
    }

    #[test]
    fn three_random_picks_can_be_undone_but_no_further() {
        let mut b = browser();
        let original = b.selected;
        press(&mut b, ui::KEY_F2);
        let first = b.selected;
        press(&mut b, ui::KEY_F2);
        let second = b.selected;
        press(&mut b, ui::KEY_F2);
        b.shift = true;
        press(&mut b, ui::KEY_F2);
        assert_eq!(b.selected, second);
        press(&mut b, ui::KEY_F2);
        assert_eq!(b.selected, first);
        press(&mut b, ui::KEY_F2);
        assert_eq!(b.selected, original);
        press(&mut b, ui::KEY_F2);
        assert_eq!(b.selected, original, "no more random picks to undo");
        assert!(b.history.is_empty());
    }

    #[test]
    fn shift_f2_does_not_select_a_map_hidden_by_search() {
        let mut b = browser();
        type_text(&mut b, "freedom");
        let current = b.selected;
        b.shift = true;
        press(&mut b, ui::KEY_F2);
        assert_eq!(b.selected, current);
    }

    #[test]
    fn a_library_opens_on_the_hardest_map_of_its_first_set() {
        let b = browser();
        assert_eq!(b.sets, [(0, 2), (2, 1), (3, 3)]);
        assert_eq!(b.order, [0, 1, 2], "sorted by artist");
        assert_eq!(version(&b), "Insane", "the set's last map, they are kept easiest first");
        assert_eq!(b.opened(), Some((0, 2)));
    }

    #[test]
    fn keys_walk_through_difficulties_and_sets() {
        let mut b = browser();
        press(&mut b, ui::KEY_UP);
        assert_eq!(version(&b), "Hard");
        press(&mut b, ui::KEY_UP);
        assert_eq!(version(&b), "Hard", "nothing above the first");
        press(&mut b, ui::KEY_DOWN);
        press(&mut b, ui::KEY_DOWN);
        assert_eq!(version(&b), "Hayakou's Extreme", "past the last difficulty comes the next set");
        press(&mut b, ui::KEY_RIGHT);
        assert_eq!(version(&b), "FOUR DIMENSIONS", "a set opens on its hardest map");
        press(&mut b, ui::KEY_UP);
        assert_eq!(version(&b), "Another");
        press(&mut b, ui::KEY_LEFT);
        assert_eq!(version(&b), "Hayakou's Extreme");
        assert_eq!(press(&mut b, ui::KEY_ENTER), BrowserAction::Start);
        assert_eq!(press(&mut b, ui::KEY_F1), BrowserAction::Mods);
        assert_eq!(press(&mut b, ui::KEY_F3), BrowserAction::Options);
    }

    #[test]
    fn typing_filters_by_every_word_and_moves_the_selection_into_the_results() {
        let mut b = browser();
        type_text(&mut b, "freedom");
        assert_eq!(b.order, [2]);
        assert_eq!(version(&b), "FOUR DIMENSIONS");
        type_text(&mut b, " norm");
        assert_eq!(version(&b), "Normal");
        assert_eq!(b.opened(), Some((0, 1)), "only the matching difficulty is listed");
        // Backspace widens the search again; Esc clears it; Esc again leaves.
        for _ in 0..5 {
            press(&mut b, ui::KEY_BACKSPACE);
        }
        assert_eq!((version(&b), b.opened()), ("Normal", Some((0, 3))), "the selection stays where it was");
        assert_eq!(press(&mut b, ui::KEY_ESC), BrowserAction::None);
        assert_eq!(b.order.len(), 3);
        assert_eq!(press(&mut b, ui::KEY_ESC), BrowserAction::Back);
        type_text(&mut b, "zzz");
        type_text(&mut b, "");
        assert!(b.order.is_empty() && b.rows_len == 0);
        let capacity = b.query.capacity();
        type_text(&mut b, &"a".repeat(200));
        assert_eq!((b.query.len(), b.query.capacity()), (QUERY_MAX, capacity), "typing must not reallocate");
    }

    #[test]
    fn the_opened_set_takes_a_row_per_difficulty_with_a_little_air() {
        let mut b = browser();
        press(&mut b, ui::KEY_RIGHT);
        let (opened, pitch) = (b.opened(), b.layout.pitch);
        assert_eq!(opened, Some((1, 1)));
        assert_eq!(b.row_y(opened, 0, 0), 0.0);
        assert_eq!(b.row_y(opened, 1, 0), pitch * 1.1);
        assert_eq!(b.row_y(opened, 2, 0), pitch * 2.2);
    }

    #[test]
    fn the_selected_button_is_brought_to_the_middle_and_stands_out() {
        let mut maps: Vec<MapInfo> =
            (0..60).map(|i| map(&format!("{i:02}"), &format!("artist {i:02}"), "song", "Hard", 100 + i)).collect();
        library::group(&mut maps);
        let mut b = Browser::new(None, Sort::Artist);
        let m = Metrics::new(1920.0, 1080.0);
        b.layout = b.compute_layout(&m, &Skin::default());
        b.set_library(Library { maps, scores: Vec::new(), collections: Vec::new() });
        for _ in 0..30 {
            press(&mut b, ui::KEY_DOWN);
        }
        assert_eq!(b.selected_map().unwrap().artist, "artist 30");
        for _ in 0..500 {
            b.update(&input(&[]), &m, &Skin::default());
        }
        let rows = &b.rows[..b.rows_len];
        let row = rows.iter().find(|r| r.selected).unwrap();
        let middle = b.layout.view.y + b.layout.view.h * 0.5;
        assert!((row.rect.y + b.layout.pitch * 0.5 - middle).abs() < 2.0, "{row:?}");
        assert!(rows.iter().all(|r| r.selected || r.rect.x > row.rect.x), "it stands out to the left");
        assert!(rows.len() > 6 && rows.len() < 14, "{} buttons on a 1080 line screen", rows.len());
    }

    #[test]
    fn sorting_by_difficulty_lists_single_maps_by_stars() {
        let mut maps = vec![
            map("1", "A", "One", "Easy", 100),
            map("1", "A", "One", "Hard", 200),
            map("2", "B", "Two", "Mid", 150),
        ];
        let stars = [1.0, 6.0, 3.0];
        for (m, s) in maps.iter_mut().zip(stars) {
            m.stars[0] = s;
        }
        library::group(&mut maps);
        let mut b = Browser::new(None, Sort::Artist);
        b.layout = b.compute_layout(&Metrics::new(1920.0, 1080.0), &Skin::default());
        b.set_library(Library { maps, scores: Vec::new(), collections: Vec::new() });
        b.set_sort(Sort::Difficulty);
        b.between_frames();
        let names: Vec<_> = b.order.iter().map(|&s| b.face(s).version.clone()).collect();
        assert_eq!(names, ["Easy", "Mid", "Hard"]);
    }

    #[test]
    fn difficulty_group_has_separate_ten_and_above_ten_buckets() {
        let mut m = map("1", "A", "One", "Hard", 100);
        m.stars[0] = 10.9;
        assert_eq!(Group::Difficulty.of(&m), 10);
        m.stars[0] = 11.0;
        assert_eq!(Group::Difficulty.of(&m), 11);
        let mut label = StackStr::<64>::new();
        Group::Difficulty.heading(11, &[], &mut label);
        assert_eq!(label.as_str(), "Above 10 stars");
    }

    #[test]
    fn grouping_by_difficulty_does_not_tie_maps_of_a_set_together() {
        let mut maps = vec![
            map("1", "A", "One", "Easy", 100),
            map("1", "A", "One", "Hard", 200),
            map("2", "B", "Two", "Mid", 150),
        ];
        for (m, s) in maps.iter_mut().zip([1.2, 9.1, 5.3]) {
            m.stars[0] = s;
        }
        library::group(&mut maps);
        let mut b = Browser::new(None, Sort::Artist);
        b.layout = b.compute_layout(&Metrics::new(1920.0, 1080.0), &Skin::default());
        b.set_library(Library { maps, scores: Vec::new(), collections: Vec::new() });
        b.set_group(Group::Difficulty);
        b.between_frames();
        let names: Vec<_> = b.order.iter().map(|&s| (b.group_of[s as usize], b.face(s).version.clone())).collect();
        assert_eq!(names, [(1, "Easy".to_string()), (5, "Mid".to_string()), (9, "Hard".to_string())]);
    }

    #[test]
    fn sorting_reorders_sets_and_a_click_on_the_selected_map_starts_it() {
        let mut b = browser();
        b.set_sort(Sort::Length);
        b.between_frames();
        assert_eq!(b.order, [0, 1, 2]);
        b.set_sort(Sort::DateAdded);
        b.between_frames();
        assert_eq!(b.order, [2, 1, 0], "newest first");
        assert_eq!(version(&b), "Insane", "the selection survives a sort");
        let m = Metrics::new(1920.0, 1080.0);
        for _ in 0..500 {
            b.update(&input(&[]), &m, &Skin::default());
        }
        let row = *b.rows[..b.rows_len].iter().find(|r| r.selected).unwrap();
        let (x, y) = (row.rect.x + 30.0, row.rect.y + row.rect.h * 0.5);
        let click = |b: &mut Browser, pressed: bool| {
            let i = Input { x, y, pressed, down: pressed, ..input(&[]) };
            b.update(&i, &m, &Skin::default())
        };
        assert_eq!(click(&mut b, true), BrowserAction::None);
        assert_eq!(click(&mut b, false), BrowserAction::Start);
    }

    #[test]
    fn previews_restart_for_another_song_but_not_for_another_difficulty() {
        let mut b = browser();
        assert!(b.preview_due(30_000_000).is_none(), "still settling");
        assert!(b.preview_due(300_000_000).is_some());
        assert!(b.preview_due(900_000_000).is_none(), "already playing");
        b.select_map(0, 1_000_000_000);
        assert!(b.preview_due(2_000_000_000).is_none(), "the same song");
        b.select_set(2, 2_000_000_000);
        assert_eq!(b.preview_due(3_000_000_000).map(|m| m.title.clone()), Some("FREEDOM DiVE".into()));
        b.restart_preview(4_000_000_000);
        assert!(b.preview_due(4_300_000_000).is_some());
        assert_eq!(b.picture_due(), Some(None), "no background named");
        assert_eq!(b.picture_due(), None);
    }

    #[test]
    fn empty_creator_is_not_listed_in_any_group() {
        let mut b = browser();
        for m in &mut b.maps { m.creator.clear(); }
        b.set_group(Group::Creator);
        b.between_frames();
        assert!(b.order.is_empty());
        assert_eq!(b.entries.len(), 28);
    }

    #[test]
    fn group_tabs_are_always_clickable_and_dropdown_lists_extra_groups() {
        let mut b = browser();
        let m = Metrics::new(1920.0, 1080.0);
        let skin = Skin::default();
        let click = |b: &mut Browser, r: Rect| {
            let i = Input { x: r.x + r.w * 0.5, y: r.y + r.h * 0.5, pressed: true, ..input(&[]) };
            b.update(&i, &m, &skin);
        };
        let tab = b.layout.tab(1);
        click(&mut b, tab);
        assert_eq!(b.group, Group::Artist);
        let group = b.layout.group;
        click(&mut b, group);
        assert!(b.group_menu);
        let group = b.layout.group;
        let creator = Rect::new(group.x, group.y + group.h * 3.0, group.w, group.h);
        click(&mut b, creator);
        assert_eq!(b.group, Group::Creator);
        assert!(!b.group_menu);
    }

    #[test]
    fn length_groups_difficulties_at_inclusive_boundaries() {
        let group = Group::named("length").expect("length group available");
        let mut m = map("1", "A", "One", "Hard", 100);
        for (length, expected) in [(60_000, 0), (60_001, 1), (120_000, 1), (180_000, 2), (240_000, 3), (300_000, 4), (600_000, 5), (600_001, 6)] {
            m.length_ms = length;
            assert_eq!(group.of(&m), expected);
        }
        let mut maps = vec![map("1", "A", "One", "Easy", 100), map("1", "A", "One", "Hard", 200)];
        maps[0].length_ms = 60_000;
        maps[1].length_ms = 600_001;
        library::group(&mut maps);
        let mut b = browser();
        b.set_library(Library { maps, scores: Vec::new(), collections: Vec::new() });
        b.set_group(group);
        b.between_frames();
        assert_eq!((b.group_counts[0], b.group_counts[6]), (1, 1));
        assert_eq!(b.entries.iter().filter(|&&e| e & HEADING != 0).count(), 7);
    }

    #[test]
    fn creator_and_title_group_by_initial() {
        let mut b = browser();
        for (name, bucket) in [("creator", 13), ("title", 7)] {
            b.set_group(Group::named(name).expect("group available"));
            b.between_frames();
            assert!(b.group_counts[bucket] > 0);
            assert_eq!(b.entries.iter().filter(|&&e| e & HEADING != 0).count(), 28);
        }
    }

    #[test]
    fn collections_show_empty_headings_until_searching() {
        let mut b = browser();
        let maps = b.maps.clone();
        b.set_library(Library { maps, scores: Vec::new(), collections: vec![("Empty".into(), vec![])] });
        b.set_group(Group::Collections);
        b.between_frames();
        assert_eq!(b.entries, vec![HEADING]);
        type_text(&mut b, "ghost");
        assert!(b.entries.is_empty());
    }

    #[test]
    fn grouping_shows_empty_headings_until_searching() {
        let mut b = browser();
        b.set_group(Group::Artist);
        b.between_frames();
        assert_eq!(b.entries.iter().filter(|&&e| e & HEADING != 0).count(), 28);
        type_text(&mut b, "ghost");
        assert_eq!(b.entries.iter().filter(|&&e| e & HEADING != 0).count(), 1);
    }

    #[test]
    fn grouping_puts_headings_in_the_list_and_opens_the_one_of_the_selection() {
        let mut b = browser();
        b.set_group(Group::Artist);
        b.between_frames();
        // Phoneboy, xi and DECO*27 are under P, X and D.
        assert_eq!(b.entries.iter().filter(|&&e| e & HEADING != 0).count(), 28);
        assert_eq!(b.entries.len(), 29, "only the heading of the selected set is open");
        assert_eq!(b.group_counts[1 + 3], 1);
        press(&mut b, ui::KEY_RIGHT);
        assert_eq!(b.selected_map().unwrap().artist, "Phoneboy");
        assert_eq!(b.entries.len(), 30, "moving on opens the next heading");
        // Clicking a heading shuts it.
        let m = Metrics::new(1920.0, 1080.0);
        for _ in 0..500 {
            b.update(&input(&[]), &m, &Skin::default());
        }
        let row = *b.rows[..b.rows_len].iter().find(|r| r.heading == Some(1 + 15)).unwrap();
        let (x, y) = (row.rect.x + 30.0, row.rect.y + row.rect.h * 0.5);
        for pressed in [true, false] {
            let i = Input { x, y, pressed, down: pressed, ..input(&[]) };
            b.update(&i, &m, &Skin::default());
        }
        assert_eq!(b.entries.len(), 29);
        b.set_group(Group::None);
        b.between_frames();
        assert_eq!(b.entries, b.order);
    }

    #[test]
    fn ctrl_keys_are_not_typed_and_do_their_own_things() {
        let mut b = browser();
        b.ctrl = true;
        press(&mut b, KEY_A);
        assert!(b.auto, "Ctrl+A turns Auto on");
        press(&mut b, KEY_A);
        assert!(!b.auto);
        assert!(b.query.is_empty(), "nothing was typed");
        b.ctrl = false;
        type_text(&mut b, "ghost rule");
        b.ctrl = true;
        press(&mut b, ui::KEY_BACKSPACE);
        assert_eq!(b.query, "ghost ", "a word goes");
        press(&mut b, ui::KEY_BACKSPACE);
        assert_eq!(b.query, "");
    }

    #[test]
    fn numbers_are_grouped_in_threes() {
        let say = |n: u64| {
            let mut s = String::new();
            grouped(&mut s, n);
            s
        };
        assert_eq!(say(0), "0");
        assert_eq!(say(999), "999");
        assert_eq!(say(1000), "1,000");
        assert_eq!(say(12_345_678), "12,345,678");
    }
}
