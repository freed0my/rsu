//! osu! skins: the images of a skin folder packed into the atlas, and what
//! `skin.ini` says about colours and number fonts.
//!
//! Every element is optional. One the skin does not have is drawn from
//! plain shapes, as rsu does without a skin; one the skin has but left
//! empty (the usual way of hiding something) is drawn as nothing.

use crate::atlas::Atlas;
use crate::image;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use rsu_gfx::{Batch, PAGE_ATLAS, PAGE_UI};

/// A skin image in the atlas.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Sprite {
    pub uv: [u16; 4],
    /// Size in osu!'s own pixels: those of the file, or half of them for
    /// an `@2x` file. Zero for an image with nothing in it.
    pub w: f32,
    pub h: f32,
    /// Which texture it is in: `PAGE_ATLAS` or `PAGE_UI`.
    pub page: u32,
}

impl Sprite {
    /// Draw centred on (x, y); `unit` is the size of one of its pixels.
    pub fn draw(&self, batch: &mut Batch, x: f32, y: f32, unit: f32, colour: [u8; 4]) {
        self.draw_turned(batch, x, y, unit, colour, 0.0, 0);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draw_turned(&self, batch: &mut Batch, x: f32, y: f32, unit: f32, colour: [u8; 4], angle: f32, flags: u32) {
        if self.w > 0.0 && colour[3] > 0 {
            let half = [self.w * unit * 0.5, self.h * unit * 0.5];
            match self.page {
                PAGE_ATLAS => batch.sprite(x, y, half, self.uv, colour, angle, flags),
                page => batch.sprite_on(page, x, y, half, self.uv, colour, angle),
            }
        }
    }

    /// Draw the left `part` (0..1) of the image with its top-left corner at
    /// (x, y), `unit` being the size of one of its pixels.
    pub fn draw_part(&self, batch: &mut Batch, x: f32, y: f32, unit: f32, colour: [u8; 4], part: f32) {
        let part = part.clamp(0.0, 1.0);
        if self.w > 0.0 && colour[3] > 0 && part > 0.0 {
            let [u0, v0, u1, v1] = self.uv;
            let u1 = u0 + ((u1 - u0) as f32 * part) as u16;
            let (w, h) = (self.w * unit * part, self.h * unit);
            batch.sprite_on(self.page, x + w * 0.5, y + h * 0.5, [w * 0.5, h * 0.5], [u0, v0, u1, v1], colour, 0.0);
        }
    }

    /// Draw stretched over the rectangle with its top-left corner at (x, y).
    pub fn draw_in(&self, batch: &mut Batch, x: f32, y: f32, w: f32, h: f32, colour: [u8; 4]) {
        if self.w > 0.0 && colour[3] > 0 {
            batch.sprite_on(self.page, x + w * 0.5, y + h * 0.5, [w * 0.5, h * 0.5], self.uv, colour, 0.0);
        }
    }

    /// Whether there is anything to see.
    pub fn visible(&self) -> bool {
        self.w > 0.0
    }
}

/// Digits and the signs that go with them, as osu! skins draw numbers.
#[derive(Clone, Copy, Debug, Default)]
pub struct Digits {
    digits: [Sprite; 10],
    dot: Option<Sprite>,
    comma: Option<Sprite>,
    percent: Option<Sprite>,
    x: Option<Sprite>,
    /// Pixels neighbouring characters are pushed into each other.
    overlap: f32,
}

impl Digits {
    fn sprite(&self, c: u8) -> Option<&Sprite> {
        match c {
            b'0'..=b'9' => Some(&self.digits[(c - b'0') as usize]),
            b'.' => self.dot.as_ref(),
            b',' => self.comma.as_ref(),
            b'%' => self.percent.as_ref(),
            b'x' | b'X' => self.x.as_ref(),
            _ => None,
        }
    }

    pub fn height(&self, unit: f32) -> f32 {
        self.digits[0].h * unit
    }

    pub fn width(&self, text: &str, unit: f32) -> f32 {
        let known = text.bytes().filter_map(|c| self.sprite(c));
        let (n, w) = known.fold((0, 0.0), |(n, w), s| (n + 1, w + s.w));
        (w - self.overlap * (n as f32 - 1.0).max(0.0)) * unit
    }

    /// Draw `text` with its left edge at `x`, centred vertically on `y`.
    pub fn draw(&self, batch: &mut Batch, x: f32, y: f32, unit: f32, colour: [u8; 4], text: &str) {
        let mut pen = x;
        for s in text.bytes().filter_map(|c| self.sprite(c)) {
            s.draw(batch, pen + s.w * unit * 0.5, y, unit, colour);
            pen += (s.w - self.overlap) * unit;
        }
    }
}

#[derive(Clone, Copy, Default)]
pub struct Scorebar {
    pub bg: Option<Sprite>,
    pub colour: Option<Sprite>,
    /// The newer kind of bar has a marker; the older one a face.
    pub marker: Option<Sprite>,
    /// Face: normal, in danger (below half), in great danger (below a fifth).
    pub ki: [Option<Sprite>; 3],
}

pub struct Skin {
    pub name: String,
    /// Combo colours in the order osu! uses them.
    pub combo: Vec<[u8; 3]>,
    pub slider_border: [u8; 3],
    pub slider_track: Option<[u8; 3]>,
    /// Whether the circle's overlay goes over its number.
    pub overlay_above_number: bool,
    pub cursor_centred: bool,
    pub hitcircle: Option<Sprite>,
    pub hitcircle_overlay: Option<Sprite>,
    pub approach: Option<Sprite>,
    pub numbers: Option<Digits>,
    pub slider_ball: Option<Sprite>,
    pub follow_circle: Option<Sprite>,
    pub reverse_arrow: Option<Sprite>,
    pub score_point: Option<Sprite>,
    /// Frames of the line of dots between two objects of one combo.
    pub follow_point: Vec<Sprite>,
    /// Judgements: miss, 50, 100, 300.
    pub hit: [Option<Sprite>; 4],
    /// Spinner layers bottom to top, each with how much of the player's
    /// rotation it follows.
    pub spinner: Vec<(Sprite, f32)>,
    pub spinner_approach: Option<Sprite>,
    pub cursor: Option<Sprite>,
    pub cursor_middle: Option<Sprite>,
    pub cursor_smoke: Option<Sprite>,
    pub play_skip: Option<Sprite>,
    pub circular_metre: Option<Sprite>,
    /// The health bar: background, fill, marker, and the three faces.
    pub scorebar: Scorebar,
    pub cursor_trail: Option<Sprite>,
    pub score: Option<Digits>,
    /// What the menus are made of.
    pub menu: Menu,
    /// Number of images taken from the skin.
    pub images: u32,
}

/// Grades in the order their images are kept: SS, S, A, B, C, D.
/// The last two are the silver grades of hidden.
pub const GRADES: [&str; 8] = ["SS", "S", "A", "B", "C", "D", "SSH", "SH"];
/// The mods there are button images for, by the bit osu! gives each, in the
/// order of `Menu::mods`.
pub const MOD_IMAGES: [(u32, &str); 14] = [
    (2, "easy"),
    (1, "nofail"),
    (256, "halftime"),
    (16, "hardrock"),
    (32, "suddendeath"),
    (16384, "perfect"),
    (64, "doubletime"),
    (512, "nightcore"),
    (8, "hidden"),
    (1024, "flashlight"),
    (128, "relax"),
    (8192, "relax2"),
    (4096, "spunout"),
    (2048, "autoplay"),
];

/// The images and colours of the menus. All optional; the menus draw
/// plain shapes for what is missing.
#[derive(Clone, Debug, PartialEq)]
pub struct Menu {
    /// `Version` of skin.ini; decides a few layout details.
    pub version: f32,
    pub background: Option<Sprite>,
    pub active_text: [u8; 3],
    pub inactive_text: [u8; 3],
    /// The body of a song button.
    pub button: Option<Sprite>,
    pub top: Option<Sprite>,
    pub bottom: Option<Sprite>,
    pub back: Option<Sprite>,
    /// The buttons of the bottom bar: mode, mods, random, options; then
    /// the same four while the pointer is over them.
    pub selection: [Option<Sprite>; 4],
    pub selection_over: [Option<Sprite>; 4],
    pub star: Option<Sprite>,
    pub ranking_panel: Option<Sprite>,
    pub ranking_title: Option<Sprite>,
    pub ranking_maxcombo: Option<Sprite>,
    pub ranking_accuracy: Option<Sprite>,
    pub ranking_replay: Option<Sprite>,
    /// Large and small grade letters, in the order of `GRADES`.
    pub grades: [Option<Sprite>; 8],
    pub grades_small: [Option<Sprite>; 8],
    /// Continue, retry, back.
    pub pause: [Option<Sprite>; 3],
    pub pause_overlay: Option<Sprite>,
    /// In the order of `MOD_IMAGES`.
    pub mods: [Option<Sprite>; 14],
}

impl Default for Menu {
    fn default() -> Menu {
        Menu {
            version: 1.0,
            background: None,
            active_text: [0, 0, 0],
            inactive_text: [255, 255, 255],
            button: None,
            top: None,
            bottom: None,
            back: None,
            selection: [None; 4],
            selection_over: [None; 4],
            star: None,
            ranking_panel: None,
            ranking_title: None,
            ranking_maxcombo: None,
            ranking_accuracy: None,
            ranking_replay: None,
            grades: [None; 8],
            grades_small: [None; 8],
            pause: [None; 3],
            pause_overlay: None,
            mods: [None; 14],
        }
    }
}

impl Default for Skin {
    /// No skin: rsu's own shapes and colours.
    fn default() -> Skin {
        Skin {
            name: String::new(),
            combo: vec![[255, 150, 60], [90, 200, 110], [70, 160, 255], [235, 80, 120]],
            slider_border: [220, 220, 230],
            slider_track: None,
            overlay_above_number: true,
            cursor_centred: true,
            hitcircle: None,
            hitcircle_overlay: None,
            approach: None,
            numbers: None,
            slider_ball: None,
            follow_circle: None,
            reverse_arrow: None,
            score_point: None,
            follow_point: Vec::new(),
            hit: [None; 4],
            spinner: Vec::new(),
            spinner_approach: None,
            cursor: None,
            cursor_middle: None,
            cursor_smoke: None,
            play_skip: None,
            circular_metre: None,
            scorebar: Scorebar::default(),
            cursor_trail: None,
            score: None,
            menu: Menu::default(),
            images: 0,
        }
    }
}

/// What `skin.ini` says, as far as rsu uses it.
#[derive(Debug, PartialEq)]
struct Ini {
    combo: Vec<[u8; 3]>,
    slider_border: Option<[u8; 3]>,
    slider_track: Option<[u8; 3]>,
    overlay_above_number: bool,
    cursor_centred: bool,
    number_prefix: String,
    number_overlap: f32,
    score_prefix: String,
    score_overlap: f32,
    version: f32,
    active_text: Option<[u8; 3]>,
    inactive_text: Option<[u8; 3]>,
}

impl Ini {
    fn parse(text: &str) -> Ini {
        let mut ini = Ini {
            combo: Vec::new(),
            slider_border: None,
            slider_track: None,
            overlay_above_number: true,
            cursor_centred: true,
            number_prefix: "default".into(),
            number_overlap: -2.0,
            score_prefix: "score".into(),
            score_overlap: 0.0,
            version: 1.0,
            active_text: None,
            inactive_text: None,
        };
        let mut combo = [None; 8];
        for line in text.lines() {
            let line = line.split("//").next().unwrap_or("");
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
            let colour = || {
                let mut parts = value.split(',').map(|p| p.trim().parse::<u8>().ok());
                Some([parts.next()??, parts.next()??, parts.next()??])
            };
            let flag = value.starts_with('1');
            match key.as_str() {
                "sliderborder" => ini.slider_border = colour().or(ini.slider_border),
                "slidertrackoverride" => ini.slider_track = colour().or(ini.slider_track),
                "hitcircleoverlayabovenumber" | "hitcircleoverlayabovenumer" => ini.overlay_above_number = flag,
                "cursorcentre" => ini.cursor_centred = flag,
                // "latest" and anything else unreadable counts as current.
                "version" => ini.version = value.parse().unwrap_or(2.5),
                "songselectactivetext" => ini.active_text = colour().or(ini.active_text),
                "songselectinactivetext" => ini.inactive_text = colour().or(ini.inactive_text),
                "hitcircleprefix" if !value.is_empty() => ini.number_prefix = value.into(),
                "hitcircleoverlap" => ini.number_overlap = value.parse().unwrap_or(ini.number_overlap),
                "scoreprefix" if !value.is_empty() => ini.score_prefix = value.into(),
                "scoreoverlap" => ini.score_overlap = value.parse().unwrap_or(ini.score_overlap),
                _ => {
                    let n = key.strip_prefix("combo").and_then(|n| n.parse::<usize>().ok());
                    if let Some(n @ 1..=8) = n {
                        combo[n - 1] = colour();
                    }
                }
            }
        }
        // osu! starts a map with Combo2 and comes to Combo1 last.
        ini.combo = combo.iter().skip(1).chain(combo.first()).flatten().copied().collect();
        ini
    }
}

/// The files of a skin by lower-cased path relative to the skin folder:
/// skins are written for a file system that ignores case.
struct Files {
    root: PathBuf,
    dirs: HashMap<String, HashMap<String, PathBuf>>,
}

impl Files {
    fn find(&mut self, name: &str) -> Option<PathBuf> {
        let name = name.replace('\\', "/").to_lowercase();
        let (dir, file) = name.rsplit_once('/').unwrap_or(("", &name));
        if !self.dirs.contains_key(dir) {
            // Walk down to the folder one case-insensitive step at a time.
            let mut path = Some(self.root.clone());
            for part in dir.split('/').filter(|p| !p.is_empty() && *p != "." && *p != "..") {
                path = path.and_then(|p| {
                    let found = std::fs::read_dir(p)
                        .ok()?
                        .flatten()
                        .find(|e| e.file_name().to_string_lossy().to_lowercase() == part);
                    Some(found?.path())
                });
            }
            let list = path.and_then(|p| std::fs::read_dir(p).ok()).into_iter().flatten().flatten();
            let files = list.map(|e| (e.file_name().to_string_lossy().to_lowercase(), e.path())).collect();
            self.dirs.insert(dir.to_string(), files);
        }
        self.dirs[dir].get(file).filter(|p| p.is_file()).cloned()
    }
}

/// A decoded image waiting for its place in an atlas.
struct Image {
    w: usize,
    h: usize,
    rgba: Vec<u8>,
    /// File pixels per osu! pixel.
    density: f32,
    page: u32,
}

/// Images wider or taller than this are halved until they are not: the
/// atlas is finite and a spinner does not need 2000 pixels. The images of
/// the menus are fewer and are seen larger.
const MAX_SIDE: usize = 1024;
const MAX_SIDE_UI: usize = 1400;

fn decode(path: &Path, density: f32, page: u32) -> Result<Image, String> {
    let limit = if page == PAGE_UI { MAX_SIDE_UI } else { MAX_SIDE };
    let image = image::load(path, limit, limit)?;
    let density = density / (1u32 << image.halved) as f32;
    Ok(Image { w: image.w, h: image.h, rgba: image.rgba, density, page })
}

/// Collects a skin's images so that they can be packed tallest first.
struct Loader {
    files: Files,
    images: Vec<Image>,
}

impl Loader {
    /// The image called `name`, in its `@2x` version if there is one. The
    /// result is an index into `images`.
    fn image(&mut self, name: &str) -> Option<usize> {
        self.image_on(PAGE_ATLAS, name)
    }

    /// An image of the menus: these go into the second atlas, and a few of
    /// them come as JPEG.
    fn ui(&mut self, name: &str) -> Option<usize> {
        self.image_on(PAGE_UI, name)
    }

    fn image_on(&mut self, page: u32, name: &str) -> Option<usize> {
        let kinds: &[&str] = if page == PAGE_UI { &["png", "jpg"] } else { &["png"] };
        let files = [("@2x", 2.0), ("", 1.0)].into_iter().flat_map(|s| kinds.iter().map(move |k| (s.0, s.1, *k)));
        let found = files.into_iter().find_map(|(suffix, density, kind)| {
            let path = self.files.find(&format!("{name}{suffix}.{kind}"))?;
            match decode(&path, density, page) {
                Ok(image) => Some(image),
                Err(e) => {
                    eprintln!("rsu: skin image {} not used: {e}", path.display());
                    None
                }
            }
        })?;
        self.images.push(found);
        Some(self.images.len() - 1)
    }

    /// The first of `names` the skin has.
    fn first(&mut self, names: &[&str]) -> Option<usize> {
        names.iter().find_map(|n| self.image(n))
    }

    fn digits(&mut self, prefix: &str) -> Option<([usize; 10], [Option<usize>; 4])> {
        let mut digits = [0; 10];
        for (n, d) in digits.iter_mut().enumerate() {
            *d = self.image(&format!("{prefix}-{n}"))?;
        }
        let signs = ["dot", "comma", "percent", "x"].map(|s| self.image(&format!("{prefix}-{s}")));
        Some((digits, signs))
    }
}

impl Skin {
    /// Read the skin in `dir` and pack its images into `atlas`.
    pub fn load(dir: &Path, atlas: &mut Atlas, ui_atlas: &mut Atlas) -> Skin {
        let mut l = Loader { files: Files { root: dir.to_path_buf(), dirs: HashMap::new() }, images: Vec::new() };
        let ini = l.files.find("skin.ini").and_then(|p| std::fs::read(p).ok());
        let ini = Ini::parse(&String::from_utf8_lossy(&ini.unwrap_or_default()));

        // First everything is decoded, then packed, then turned into sprites.
        let hitcircle = l.image("hitcircle");
        let hitcircle_overlay = l.first(&["hitcircleoverlay-0", "hitcircleoverlay"]);
        let approach = l.image("approachcircle");
        let numbers = l.digits(&ini.number_prefix);
        let slider_ball = l.first(&["sliderb0", "sliderb"]);
        let follow_circle = l.first(&["sliderfollowcircle-0", "sliderfollowcircle"]);
        let reverse_arrow = l.image("reversearrow");
        let score_point = l.image("sliderscorepoint");
        // `followpoint-0`, `-1`, ... play as an animation; a single
        // `followpoint` is one frame.
        let mut follow_point: Vec<usize> = Vec::new();
        while let Some(i) = l.image(&format!("followpoint-{}", follow_point.len())) {
            follow_point.push(i);
        }
        if follow_point.is_empty() {
            follow_point.extend(l.image("followpoint"));
        }
        let hit = ["hit0", "hit50", "hit100", "hit300"].map(|n| l.first(&[&format!("{n}-0"), n]));
        // The newer kind of spinner is made of several discs; the older one
        // of a single image that turns.
        let new_style = ["spinner-bottom", "spinner-top", "spinner-middle2", "spinner-middle"];
        let mut spinner: Vec<(usize, f32)> =
            new_style.iter().zip([0.33, 0.5, 1.0, 0.0]).filter_map(|(n, turn)| Some((l.image(n)?, turn))).collect();
        if spinner.is_empty() {
            spinner.extend(l.image("spinner-circle").map(|i| (i, 1.0)));
        }
        let spinner_approach = l.image("spinner-approachcircle");
        let cursor = l.image("cursor");
        let cursor_middle = l.image("cursormiddle");
        let cursor_smoke = l.image("cursor-smoke");
        let play_skip = l.image("play-skip");
        let circular_metre = l.image("circularmetre");
        let scorebar = [
            l.image("scorebar-bg"),
            l.first(&["scorebar-colour-0", "scorebar-colour"]),
            l.first(&["scorebar-marker"]),
            l.image("scorebar-ki"),
            l.image("scorebar-kidanger"),
            l.image("scorebar-kidanger2"),
        ];
        let cursor_trail = l.image("cursortrail");
        let score = l.digits(&ini.score_prefix);

        // The menus. An animated element is shown by its first frame.
        let m_background = l.ui("menu-background");
        let m_button = l.ui("menu-button-background");
        let m_top = l.ui("songselect-top");
        let m_bottom = l.ui("songselect-bottom");
        let m_back = l.ui("menu-back-0").or_else(|| l.ui("menu-back"));

        let m_star = l.ui("star");
        let m_panel = l.ui("ranking-panel");
        let m_title = l.ui("ranking-title");
        let m_maxcombo = l.ui("ranking-maxcombo");
        let m_accuracy = l.ui("ranking-accuracy");
        let replay_names =
            if ini.version <= 1.0 { ["ranking-replay", "pause-replay"] } else { ["pause-replay", "ranking-replay"] };
        let m_replay = replay_names.into_iter().find_map(|name| l.ui(name));
        let letters = ["x", "s", "a", "b", "c", "d", "xh", "sh"];
        let m_grades = letters.map(|n| l.ui(&format!("ranking-{n}")));
        let m_grades_small = letters.map(|n| l.ui(&format!("ranking-{n}-small")));
        let m_pause = ["continue", "retry", "back"].map(|n| l.ui(&format!("pause-{n}")));
        let m_overlay = l.ui("pause-overlay");
        let m_mods = MOD_IMAGES.map(|(_, n)| l.ui(&format!("selection-mod-{n}")));

        let mut order: Vec<usize> = (0..l.images.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(l.images[i].h));
        let mut sprites = vec![None; l.images.len()];
        let mut dropped = 0;
        for i in order {
            let image = &l.images[i];
            let (w, h) = (image.w as f32 / image.density, image.h as f32 / image.density);
            sprites[i] = if image.rgba.chunks_exact(4).all(|p| p[3] == 0) {
                Some(Sprite::default())
            } else {
                let into = if image.page == PAGE_UI { &mut *ui_atlas } else { &mut *atlas };
                let uv = into.add_rgba(image.w, image.h, &image.rgba);
                dropped += uv.is_none() as u32;
                uv.map(|uv| Sprite { uv, w, h, page: image.page })
            };
        }
        if dropped > 0 {
            eprintln!("rsu: {dropped} skin images did not fit into the texture and are drawn as plain shapes");
        }
        let get = |i: Option<usize>| i.and_then(|i| sprites[i]);
        let digits = |d: Option<([usize; 10], [Option<usize>; 4])>, overlap: f32| {
            let (digits, signs) = d?;
            let mut out = Digits { overlap, ..Digits::default() };
            for (o, i) in out.digits.iter_mut().zip(digits) {
                *o = sprites[i]?;
            }
            [out.dot, out.comma, out.percent, out.x] = signs.map(get);
            Some(out)
        };
        Skin {
            name: dir.file_name().map(|n| n.to_string_lossy().trim().to_string()).unwrap_or_default(),
            // A skin that names no colours gets osu!'s own.
            combo: if ini.combo.is_empty() {
                vec![[0, 202, 0], [18, 124, 255], [242, 24, 57], [255, 192, 0]]
            } else {
                ini.combo
            },
            slider_border: ini.slider_border.unwrap_or([255, 255, 255]),
            slider_track: ini.slider_track,
            overlay_above_number: ini.overlay_above_number,
            cursor_centred: ini.cursor_centred,
            hitcircle: get(hitcircle),
            hitcircle_overlay: get(hitcircle_overlay),
            approach: get(approach),
            numbers: digits(numbers, ini.number_overlap),
            slider_ball: get(slider_ball),
            follow_circle: get(follow_circle),
            reverse_arrow: get(reverse_arrow),
            score_point: get(score_point),
            follow_point: follow_point.into_iter().filter_map(|i| sprites[i]).collect(),
            hit: hit.map(get),
            spinner: spinner.into_iter().filter_map(|(i, turn)| Some((sprites[i]?, turn))).collect(),
            spinner_approach: get(spinner_approach),
            cursor: get(cursor),
            cursor_middle: get(cursor_middle),
            cursor_smoke: get(cursor_smoke),
            play_skip: get(play_skip),
            circular_metre: get(circular_metre),
            scorebar: Scorebar {
                bg: get(scorebar[0]),
                colour: get(scorebar[1]),
                marker: get(scorebar[2]),
                ki: [get(scorebar[3]), get(scorebar[4]), get(scorebar[5])],
            },
            cursor_trail: get(cursor_trail),
            score: digits(score, ini.score_overlap),
            menu: Menu {
                version: ini.version,
                background: get(m_background),
                active_text: ini.active_text.unwrap_or([0, 0, 0]),
                inactive_text: ini.inactive_text.unwrap_or([255, 255, 255]),
                button: get(m_button),
                top: get(m_top),
                bottom: get(m_bottom),
                back: get(m_back),
                // Bottom navigation always uses rsu's own buttons.
                selection: [None; 4],
                selection_over: [None; 4],
                star: get(m_star),
                ranking_panel: get(m_panel),
                ranking_title: get(m_title),
                ranking_maxcombo: get(m_maxcombo),
                ranking_accuracy: get(m_accuracy),
                ranking_replay: get(m_replay),
                grades: silver_or_gold(m_grades.map(get)),
                grades_small: silver_or_gold(m_grades_small.map(get)),
                pause: m_pause.map(get),
                pause_overlay: get(m_overlay),
                mods: m_mods.map(get),
            },
            images: sprites.iter().flatten().count() as u32,
        }
    }

    pub fn combo_colour(&self, index: u16) -> [u8; 3] {
        self.combo[index as usize % self.combo.len()]
    }
}

/// The skins next to a Songs folder, by name.
pub fn installed(songs: Option<&Path>) -> Vec<PathBuf> {
    let dir = songs.and_then(Path::parent).map(|p| p.join("Skins"));
    let list = dir.and_then(|d| std::fs::read_dir(d).ok()).into_iter().flatten().flatten();
    let mut skins: Vec<PathBuf> = list.map(|e| e.path()).filter(|p| p.is_dir()).collect();
    skins
        .sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().trim_start_matches([' ', '-', '_']).to_lowercase()));
    skins
}

/// A skin without silver grade images shows the gold ones for them.
fn silver_or_gold(mut g: [Option<Sprite>; 8]) -> [Option<Sprite>; 8] {
    g[6] = g[6].or(g[0]);
    g[7] = g[7].or(g[1]);
    g
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skin_ini_is_read_loosely() {
        let ini = Ini::parse(
            "[General]\nName: x // a comment\nCursorCentre: 0\nHitCircleOverlayAboveNumer: 0\n\
             [Colours]\ncombo1 : 1,2,3\nCombo2: 4, 5, 6\nCombo4: 7,8,9\nCombo5: 300,0,0\n\
             SliderBorder: 10,20,30\nSliderTrackOverride: nonsense\n\
             [Fonts]\nHitCirclePrefix: Numbers\\default\nHitCircleOverlap: 12\nScorePrefix:\nScoreOverlap: x\n\
             Version: latest\nSongSelectActiveText: 1,1,1\n",
        );
        assert_eq!((ini.version, ini.active_text, ini.inactive_text), (2.5, Some([1, 1, 1]), None));
        assert_eq!(ini.combo, [[4, 5, 6], [7, 8, 9], [1, 2, 3]], "Combo2 first, Combo1 last, gaps and junk skipped");
        assert_eq!(ini.slider_border, Some([10, 20, 30]));
        assert_eq!(ini.slider_track, None);
        assert!(!ini.cursor_centred && !ini.overlay_above_number);
        assert_eq!((ini.number_prefix.as_str(), ini.number_overlap), ("Numbers\\default", 12.0));
        assert_eq!((ini.score_prefix.as_str(), ini.score_overlap), ("score", 0.0));
    }

    fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) {
        let mut encoder = png::Encoder::new(std::fs::File::create(path).unwrap(), w, h);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header().unwrap().write_image_data(rgba).unwrap();
    }

    #[test]
    fn replay_button_skin_prefers_the_asset_for_its_version() {
        let dir = std::env::temp_dir().join(format!("rsu-replay-button-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_png(&dir.join("Ranking-Replay@2x.png"), 8, 4, &[255; 128]);
        write_png(&dir.join("ranking-replay.png"), 3, 2, &[255; 24]);
        write_png(&dir.join("pause-replay.png"), 7, 2, &[255; 56]);
        let load = || Skin::load(&dir, &mut Atlas::new(), &mut Atlas::new());
        let standard = load();
        std::fs::write(dir.join("skin.ini"), "[General]\nVersion: 2.0\n").unwrap();
        let modern = load();
        std::fs::remove_file(dir.join("skin.ini")).unwrap();
        std::fs::write(dir.join("Ranking-Replay@2x.png"), b"broken").unwrap();
        let standard_sd = load();
        std::fs::remove_file(dir.join("ranking-replay.png")).unwrap();
        let alternative = load();
        std::fs::remove_dir_all(&dir).unwrap();
        for (skin, w) in [(standard, 4.0), (modern, 7.0), (standard_sd, 3.0), (alternative, 7.0)] {
            assert_eq!(skin.images, 1);
            let sprite = skin.menu.ranking_replay.unwrap();
            assert_eq!((sprite.w, sprite.h, sprite.page), (w, 2.0, PAGE_UI));
        }
    }

    #[test]
    fn menu_background_is_loaded_from_skin() {
        let dir = std::env::temp_dir().join(format!("rsu-menu-background-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        write_png(&dir.join("Menu-Background.jpg"), 4, 2, &[255; 32]);
        let skin = Skin::load(&dir, &mut Atlas::new(), &mut Atlas::new());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(skin.images, 1, "the menu background is loaded as a skin image");
        let background = skin.menu.background.unwrap();
        assert_eq!((background.w, background.h, background.page), (4.0, 2.0, PAGE_UI));
    }

    #[test]
    fn songselect_navigation_ignores_skin_images_but_keeps_bottom_panel() {
        let dir = std::env::temp_dir().join(format!("rsu-skin-nav-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for name in ["mode", "mods", "random", "options"] {
            for suffix in ["", "-over"] {
                write_png(&dir.join(format!("selection-{name}{suffix}.png")), 2, 2, &[255; 16]);
            }
        }
        write_png(&dir.join("songselect-bottom.png"), 2, 2, &[255; 16]);
        let skin = Skin::load(&dir, &mut Atlas::new(), &mut Atlas::new());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(skin.menu.selection, [None; 4]);
        assert_eq!(skin.menu.selection_over, [None; 4]);
        assert!(skin.menu.bottom.is_some());
    }

    #[test]
    fn a_skin_folder_becomes_sprites() {
        let dir = std::env::temp_dir().join(format!("rsu-skin-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Numbers")).unwrap();
        std::fs::write(dir.join("Skin.INI"), "[Fonts]\nHitCirclePrefix: numbers\\N\n[Colours]\nCombo1: 9,9,9\n")
            .unwrap();
        write_png(&dir.join("HitCircle.png"), 4, 4, &[255; 64]);
        write_png(&dir.join("hitcircle@2x.png"), 8, 6, &[200; 8 * 6 * 4]);
        write_png(&dir.join("hit300.png"), 2, 2, &[0; 16]);
        std::fs::write(dir.join("cursor.png"), b"not a png").unwrap();
        for n in 0..10 {
            write_png(&dir.join(format!("Numbers/n-{n}.png")), 2, 3, &[255; 24]);
        }
        write_png(&dir.join("Numbers/n-x.png"), 1, 3, &[255; 12]);
        // Larger than the limit: stored at half size, drawn at full size.
        write_png(&dir.join("spinner-circle.png"), 1200, 2, &vec![255; 1200 * 2 * 4]);

        write_png(&dir.join("menu-back.png"), 3, 2, &[255; 24]);
        let (mut atlas, mut ui_atlas) = (Atlas::new(), Atlas::new());
        let skin = Skin::load(&dir, &mut atlas, &mut ui_atlas);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(skin.combo, [[9, 9, 9]]);
        let circle = skin.hitcircle.unwrap();
        assert_eq!((circle.w, circle.h), (4.0, 3.0), "the @2x file, at half its pixel size");
        assert_eq!(skin.hit[3], Some(Sprite::default()), "an empty image is something that draws nothing");
        assert_eq!(skin.hit[0], None);
        assert_eq!(skin.cursor, None, "a broken file counts as missing");
        let numbers = skin.numbers.unwrap();
        assert_eq!(numbers.width("12x", 1.0), 2.0 + 2.0 + 1.0 + 2.0 * 2.0, "overlap of -2 spreads them out");
        assert_eq!(numbers.width("1.", 1.0), 2.0, "a sign the skin lacks takes no room");
        let (spinner, turn) = skin.spinner[0];
        assert_eq!((spinner.w, spinner.h, turn), (1200.0, 2.0, 1.0));
        assert!(skin.score.is_none() && skin.approach.is_none());
        let back = skin.menu.back.unwrap();
        assert_eq!((back.w, back.h, back.page), (3.0, 2.0, PAGE_UI), "menu images live in the second atlas");
        assert_eq!(ui_atlas.rows(), 4);
        assert_eq!(skin.images, 15);
    }
}
