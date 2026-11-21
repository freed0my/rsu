//! rsu-sim: the game core without a window. Replays are verified here,
//! the parser is run over whole song folders, and the core is timed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use rsu_core::auto::autoplay;
use rsu_core::game::{Event, Game, HitResult, Summary};
use rsu_core::{from_fx, Chart, NestedKind, ObjectKind, RULESET_VERSION};
use rsu_formats::md5::hex;
use rsu_formats::osu::{self, ParseError};
use rsu_formats::osr::{self as stable, Osr};
use rsu_formats::replay::Replay;

const HELP: &str = "\
rsu-sim: headless tools for rsu

  rsu-sim info  MAP.osu                 what the game derives from a beatmap
  rsu-sim auto  MAP.osu OUT.tkr [HZ] [MODS]
                                         write a perfect-play replay (pointer at HZ, default 133;
                                         mods: dt, nc or ht)
  rsu-sim run   MAP.osu REPLAY.tkr      re-simulate a replay and check it against its recorded result
  rsu-sim bench MAP.osu [HZ]            time the core on this map (pointer at HZ, default 1000)
  rsu-sim scan  SONGS_DIR               parse every .osu below a folder and report
  rsu-sim why   MAP.osu [HZ]            show where a perfect play does not get a 300, and why
  rsu-sim osr   SONGS_DIR OSR...        re-simulate osu! stable replays (files or folders of .osr)
                                         and compare with the result stable recorded
  rsu-sim scores-db OSU_DIR             list stable's local scores and whether their replays exist
  rsu-sim stream-check AUDIO...         check that playing a track from the middle (the exact way
                                         plays jump) gives the samples of decoding it whole";

type Res<T> = Result<T, Box<dyn std::error::Error>>;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = |i: usize| args.get(i).map(String::as_str);
    let result = match (arg(0), arg(1), arg(2)) {
        (Some("info"), Some(map), _) => info(Path::new(map)),
        (Some("auto"), Some(map), Some(out)) => match rsu_core::mods::parse(arg(4).unwrap_or("")) {
            Ok(mods) => auto(Path::new(map), Path::new(out), rate(arg(3), 133), mods),
            Err(e) => Err(e.into()),
        },
        (Some("run"), Some(map), Some(replay)) => run(Path::new(map), Path::new(replay)),
        (Some("bench"), Some(map), hz) => bench(Path::new(map), rate(hz, 1000)),
        (Some("scan"), Some(dir), _) => scan(Path::new(dir)),
        (Some("why"), Some(map), hz) => why(Path::new(map), rate(hz, 0)),
        (Some("stream-check"), Some(_), _) => stream_check(&args[1..]),
        (Some("osr"), Some(songs), Some(_)) => osr(Path::new(songs), &args[2..]),
        (Some("scores-db"), Some(dir), _) => scores_db(Path::new(dir)),
        _ => {
            eprintln!("{HELP}");
            std::process::exit(2);
        }
    };
    match result {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("rsu-sim: {e}");
            std::process::exit(1);
        }
    }
}

fn rate(arg: Option<&str>, default: u32) -> u32 {
    arg.and_then(|v| v.parse().ok()).unwrap_or(default)
}

fn load(path: &Path) -> Res<(osu::Beatmap, [u8; 16], Arc<Chart>)> {
    let (map, md5) = osu::load(path)?;
    let chart = Arc::new(map.to_chart());
    Ok((map, md5, chart))
}

/// Time to advance to after the last input so that every object is judged.
fn end_time(chart: &Chart) -> i64 {
    chart.objects.last().map_or(0, |o| o.end.max(o.t) + 2_000_000)
}

fn print_summary(s: &Summary) {
    println!(
        "  score {}  accuracy {:.2}%  max combo {}  300/100/50/miss {}/{}/{}/{}  state {:016x}",
        s.score,
        s.accuracy() * 100.0,
        s.max_combo,
        s.n300,
        s.n100,
        s.n50,
        s.miss,
        s.state_hash
    );
}

fn info(path: &Path) -> Res<bool> {
    let (map, md5, chart) = load(path)?;
    let count = |k| chart.objects.iter().filter(|o| o.kind == k).count();
    println!("{} - {} [{}] by {}", map.artist, map.title, map.version, map.creator);
    println!("  format v{}  md5 {}  audio '{}'", map.format_version, hex(&md5), map.audio_filename);
    let d = map.difficulty;
    println!("  CS {} AR {} OD {} HP {}", d.cs, d.ar, d.od, d.hp);
    println!(
        "  objects: {} circles, {} sliders, {} spinners; {} breaks; {} timing points",
        count(ObjectKind::Circle),
        count(ObjectKind::Slider),
        count(ObjectKind::Spinner),
        map.breaks.len(),
        map.timing_points.len()
    );
    let nested = |k| chart.nested.iter().filter(|n| n.kind == k).count();
    println!(
        "  slider parts: {} ticks, {} repeats; {} path vertices",
        nested(NestedKind::Tick),
        nested(NestedKind::Repeat),
        chart.path.len()
    );
    println!(
        "  radius {:.2} px  approach {} ms  hit windows +-{:.1} / {:.1} / {:.1} ms  score multiplier {}",
        from_fx(chart.radius),
        chart.preempt / 1000,
        chart.window_300 as f64 / 1e3,
        chart.window_100 as f64 / 1e3,
        chart.window_50 as f64 / 1e3,
        chart.score_multiplier
    );
    if map.skipped_lines > 0 {
        println!("  {} unparseable lines were skipped", map.skipped_lines);
    }
    Ok(true)
}

fn auto(map_path: &Path, out: &Path, hz: u32, mods: u32) -> Res<bool> {
    let (_, md5, chart) = load(map_path)?;
    let inputs = autoplay(&chart, hz);
    let end = end_time(&chart);
    let summary = Game::simulate(chart, mods, &inputs, end);
    let replay = Replay {
        ruleset: RULESET_VERSION,
        mods,
        rate_pct: 0,
        map_md5: md5,
        unix_time: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64),
        player: "rsu autoplay".into(),
        summary,
        end_time: end,
        inputs,
    };
    let bytes = replay.encode();
    std::fs::write(out, &bytes)?;
    println!("wrote {} ({} events, {} bytes)", out.display(), replay.inputs.len(), bytes.len());
    print_summary(&summary);
    Ok(true)
}

/// How the clicks of a replay sat against the beat: the numbers behind
/// "the sound feels late".
fn print_timing(chart: &Arc<Chart>, replay: &Replay) {
    let mut errors: Vec<f64> = Vec::new();
    let mut game = Game::with_mods(chart.clone(), replay.mods);
    let mut sink = |e| match e {
        Event::Judged { object, result, error, .. }
            if result != HitResult::Miss && chart.objects[object as usize].kind == ObjectKind::Circle =>
        {
            errors.push(error as f64 / 1000.0)
        }
        Event::SliderHead { hit: true, error, .. } => errors.push(error as f64 / 1000.0),
        _ => {}
    };
    for &input in &replay.inputs {
        game.push(input, &mut sink);
    }
    game.advance_to(replay.end_time, &mut sink);
    if errors.is_empty() {
        return;
    }
    errors.sort_by(f64::total_cmp);
    let n = errors.len() as f64;
    let mean = errors.iter().sum::<f64>() / n;
    let sd = (errors.iter().map(|e| (e - mean).powi(2)).sum::<f64>() / n).sqrt();
    let early = errors.iter().filter(|e| **e < 0.0).count() as f64 / n * 100.0;
    // Speed mods stretch map time; what the player felt is real time.
    let speed = rsu_core::mods::speed(replay.mods);
    println!(
        "hit timing over {} clicks (negative is early): mean {:+.1} ms, median {:+.1} ms, spread {:.1} ms, {:.0}% early",
        errors.len(),
        mean / speed,
        errors[errors.len() / 2] / speed,
        sd / speed,
        early
    );
}

fn run(map_path: &Path, replay_path: &Path) -> Res<bool> {
    let (_, md5, chart) = load(map_path)?;
    let replay = Replay::read(replay_path)?;
    println!(
        "replay by '{}', {} events, rules v{}, map {}{}",
        replay.player,
        replay.inputs.len(),
        replay.ruleset,
        hex(&replay.map_md5),
        match replay.mods {
            0 => String::new(),
            m => format!(", mods {}", rsu_core::mods::name(m)),
        }
    );
    if replay.map_md5 != md5 {
        println!("MISMATCH: the replay was recorded on a different beatmap (this one is {})", hex(&md5));
        return Ok(false);
    }
    if replay.ruleset != RULESET_VERSION {
        println!("note: recorded under rules v{}, this build has v{RULESET_VERSION}", replay.ruleset);
    }
    let started = Instant::now();
    let summary = Game::simulate(chart.clone(), replay.mods, &replay.inputs, replay.end_time);
    let took = started.elapsed();
    println!("recorded:");
    print_summary(&replay.summary);
    println!("re-simulated in {:.3} ms:", took.as_secs_f64() * 1e3);
    print_summary(&summary);
    let same = summary == replay.summary;
    println!(
        "{}",
        if same { "VERIFIED: identical result" } else { "MISMATCH: the simulation diverged from the recording" }
    );
    print_timing(&chart, &replay);
    Ok(same)
}

fn bench(map_path: &Path, hz: u32) -> Res<bool> {
    let (map, _, chart) = load(map_path)?;
    let inputs = autoplay(&chart, hz);
    let end = end_time(&chart);
    let length_s = chart.objects.last().map_or(0.0, |o| o.t as f64 / 1e6);
    println!(
        "{} [{}]: {} objects over {:.0} s, {} input events (pointer at {hz} Hz)",
        map.title,
        map.version,
        chart.objects.len(),
        length_s,
        inputs.len()
    );
    let mut best = f64::MAX;
    let mut summary = Summary::default();
    for _ in 0..20 {
        let started = Instant::now();
        summary = Game::simulate(chart.clone(), 0, &inputs, end);
        best = best.min(started.elapsed().as_secs_f64());
    }
    println!(
        "  full simulation: {:.3} ms (best of 20), {:.1} ns per event, {:.0}x real time",
        best * 1e3,
        best * 1e9 / inputs.len().max(1) as f64,
        length_s / best
    );
    print_summary(&summary);

    let replay = Replay {
        ruleset: RULESET_VERSION,
        mods: 0,
        rate_pct: 0,
        map_md5: [0; 16],
        unix_time: 0,
        player: String::new(),
        summary,
        end_time: end,
        inputs,
    };
    let started = Instant::now();
    let bytes = replay.encode();
    let enc = started.elapsed().as_secs_f64();
    let started = Instant::now();
    let back = Replay::decode(&bytes)?;
    let dec = started.elapsed().as_secs_f64();
    println!(
        "  replay: {} bytes ({:.1} bytes per event), encode {:.3} ms, decode {:.3} ms, round trip {}",
        bytes.len(),
        bytes.len() as f64 / replay.inputs.len().max(1) as f64,
        enc * 1e3,
        dec * 1e3,
        if back == replay { "exact" } else { "DIFFERS" }
    );
    Ok(back == replay)
}

/// Run autoplay and print everything that went wrong, with context.
fn why(map_path: &Path, hz: u32) -> Res<bool> {
    use rsu_core::game::{Event, HitResult};
    let (_, _, chart) = load(map_path)?;
    let inputs = autoplay(&chart, hz);
    let mut game = Game::new(chart.clone());
    let mut events = Vec::new();
    for &input in &inputs {
        game.push(input, &mut |e| events.push(e));
    }
    game.advance_to(end_time(&chart), &mut |e| events.push(e));
    let describe = |i: u32| {
        let o = &chart.objects[i as usize];
        let prev = (i > 0).then(|| chart.objects[i as usize - 1]);
        let mut text = format!(
            "#{i} {:?} t={} ms end={} ms at ({:.1},{:.1})",
            o.kind,
            o.t / 1000,
            o.end / 1000,
            from_fx(o.x),
            from_fx(o.y)
        );
        if o.kind == ObjectKind::Slider {
            let s = chart.slider(o);
            text += &format!(
                " spans={} span={} us length={:.1}px parts={}",
                s.spans,
                s.span_duration,
                from_fx(s.length as i32),
                s.nested_len
            );
        }
        if o.kind == ObjectKind::Spinner {
            text += &format!(" needs {} half turns", o.extra);
        }
        if let Some(p) = prev {
            text += &format!("; previous {:?} ended {} ms before", p.kind, (o.t - p.end) / 1000);
        }
        text
    };
    let mut bad = 0;
    for e in &events {
        match *e {
            Event::Judged { object, result, t, .. } if result != HitResult::Hit300 => {
                bad += 1;
                println!("{result:?} at {} ms: {}", t / 1000, describe(object));
                for e in &events {
                    match *e {
                        Event::SliderHead { object: o, hit, t, error } if o == object => {
                            println!("    head hit={hit} at {} us (error {error} us)", t)
                        }
                        Event::SliderPoint { object: o, kind, hit, t } if o == object && !hit => {
                            println!("    {kind:?} missed at {t} us")
                        }
                        Event::Blocked { object: o, t } if o == object => println!("    click blocked at {t} us"),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    println!("{bad} objects without a 300");
    Ok(bad == 0)
}

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let path = e.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|x| x.eq_ignore_ascii_case("osu")) {
            out.push(path);
        }
    }
}

/// Whether any object starts before the previous one has ended. One cursor
/// cannot be in two places, so such maps are not perfectly playable.
fn overlaps(chart: &Chart) -> bool {
    chart.objects.windows(2).any(|w| w[1].t < w[0].end)
}

fn scan(dir: &Path) -> Res<bool> {
    let mut files = Vec::new();
    collect(dir, &mut files);
    files.sort();
    let started = Instant::now();
    let (mut ok, mut other_mode, mut not_map, mut unreadable) = (0u32, 0u32, 0u32, 0u32);
    let (mut objects, mut with_skips, mut skipped_lines, mut empty) = (0u64, 0u32, 0u64, 0u32);
    let mut autoplay_imperfect = Vec::new();
    let mut examples = Vec::new();
    for path in &files {
        let Ok(bytes) = std::fs::read(path) else {
            unreadable += 1;
            continue;
        };
        match osu::parse(&bytes) {
            Ok(map) => {
                ok += 1;
                let chart = Arc::new(map.to_chart());
                objects += chart.objects.len() as u64;
                if chart.objects.is_empty() {
                    empty += 1;
                }
                if map.skipped_lines > 0 {
                    with_skips += 1;
                    skipped_lines += map.skipped_lines as u64;
                    if examples.len() < 5 {
                        examples.push(format!("{} ({} lines)", path.display(), map.skipped_lines));
                    }
                }
                // Every map must be fully hittable by a perfect player; if
                // not, either the rules or the autoplay have a hole.
                let end = end_time(&chart);
                let s = Game::simulate(chart.clone(), 0, &autoplay(&chart, 0), end);
                if s.n300 as usize != chart.objects.len() {
                    autoplay_imperfect.push(format!(
                        "{}: {}/{} (100/50/miss {}/{}/{}){}",
                        path.display(),
                        s.n300,
                        chart.objects.len(),
                        s.n100,
                        s.n50,
                        s.miss,
                        if overlaps(&chart) { "  [objects overlap in time]" } else { "" }
                    ));
                }
            }
            Err(ParseError::UnsupportedMode(_)) => other_mode += 1,
            Err(ParseError::NotABeatmap) => {
                not_map += 1;
                if examples.len() < 5 {
                    examples.push(format!("{} (not a beatmap)", path.display()));
                }
            }
        }
    }
    println!("{} .osu files in {:.2} s", files.len(), started.elapsed().as_secs_f64());
    println!("  osu! standard, parsed:   {ok} ({objects} objects, {empty} without objects)");
    println!("  other game modes:        {other_mode}");
    println!("  not beatmaps:            {not_map}");
    println!("  unreadable:              {unreadable}");
    println!("  with skipped lines:      {with_skips} ({skipped_lines} lines)");
    let explained = autoplay_imperfect.iter().filter(|e| e.ends_with("in time]")).count();
    println!(
        "  autoplay not all 300s:   {} ({} of them have objects overlapping in time)",
        autoplay_imperfect.len(),
        explained
    );
    for e in examples.iter() {
        println!("    {e}");
    }
    for e in autoplay_imperfect.iter().filter(|e| !e.ends_with("in time]")).take(12) {
        println!("    {e}");
    }
    Ok(not_map == 0 && unreadable == 0)
}

/// Decode each file whole, then from several frames in the middle the way
/// a stream does after a jump, and compare the samples.
fn stream_check(files: &[String]) -> Res<bool> {
    use rsu_audio::stream::Decoder;
    let mut ok = true;
    for file in files {
        let path = Path::new(file);
        let started = Instant::now();
        let whole = match rsu_audio::decode::load(path) {
            Ok(w) => w,
            Err(e) => {
                println!("SKIP  {file}: {e}");
                continue;
            }
        };
        let whole_ms = started.elapsed().as_secs_f64() * 1e3;
        let frames = whole.len() as u64 / 2;
        let mut worst = 0.0f32;
        let mut first_bad: Option<(u64, u64)> = None;
        let mut open_ms = 0.0f64;
        let mut length_ok = true;
        let starts = [0, 1, 4_000, frames / 7, frames / 3, frames / 2 + 12_345, frames.saturating_sub(30_000)];
        for (n, &pos) in starts.iter().enumerate() {
            let t = Instant::now();
            let mut d = Decoder::open(path, pos, true)?;
            let mut got: Vec<f32> = Vec::new();
            // A second's worth after the start, or all of it for the first.
            let want = if n == 0 { usize::MAX } else { 48_000 * 2 };
            let mut first_chunk = true;
            while got.len() < want {
                match d.next(8192)? {
                    Some(chunk) => got.extend_from_slice(chunk),
                    None => break,
                }
                if first_chunk {
                    open_ms = open_ms.max(t.elapsed().as_secs_f64() * 1e3);
                    first_chunk = false;
                }
            }
            if n == 0 && got.len() != whole.len() {
                length_ok = false;
            }
            let expect = &whole[(pos as usize * 2).min(whole.len())..];
            let len = got.len().min(expect.len()).min(want);
            if got.len().min(want) != expect.len().min(want) {
                length_ok = false;
            }
            for i in 0..len {
                let d = (got[i] - expect[i]).abs();
                if d > 0.0 && first_bad.is_none() {
                    first_bad = Some((pos, i as u64 / 2));
                }
                worst = worst.max(d);
            }
        }
        let verdict = if worst == 0.0 && length_ok { "SAME" } else { "DIFF" };
        ok &= verdict == "SAME";
        let name = path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy()).unwrap_or_default();
        println!(
            "{verdict}  worst {worst:.2e}{}{}  whole {whole_ms:.0} ms, first chunk after a jump <= {open_ms:.1} ms  {name}/{}",
            first_bad.map(|(p, i)| format!(" (from {p}, frame +{i})")).unwrap_or_default(),
            if length_ok { "" } else { "  LENGTH DIFFERS" },
            path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
        );
    }
    Ok(ok)
}

/// Every beatmap below `songs` by its MD5.
fn index(songs: &Path) -> std::collections::HashMap<[u8; 16], PathBuf> {
    let mut files = Vec::new();
    collect(songs, &mut files);
    files
        .into_iter()
        .filter_map(|p| std::fs::read(&p).ok().map(|b| (rsu_formats::md5::md5(&b), p)))
        .collect()
}

fn osr(songs: &Path, args: &[String]) -> Res<bool> {
    let mut files = Vec::new();
    for a in args {
        let p = PathBuf::from(a);
        if p.is_dir() {
            for e in std::fs::read_dir(&p)?.flatten() {
                if e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("osr")) {
                    files.push(e.path());
                }
            }
        } else {
            files.push(p);
        }
    }
    files.sort();
    let maps = index(songs);
    let (mut same, mut differ, mut skipped) = (0u32, 0u32, 0u32);
    for path in &files {
        let name = path.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
        let osr = match Osr::read(path) {
            Ok(o) => o,
            Err(e) => {
                println!("{name}: {e}");
                skipped += 1;
                continue;
            }
        };
        let h = &osr.header;
        let why_not = if h.mode != 0 {
            Some("not osu!standard".to_string())
        } else if stable::foreign_mods(h.mods) != 0 {
            Some(format!("mods {} not in rsu", stable::mod_names(stable::foreign_mods(h.mods))))
        } else if !maps.contains_key(&h.map_md5) {
            Some("beatmap not in Songs".to_string())
        } else {
            None
        };
        if let Some(why) = why_not {
            println!("{name}: skipped, {why}");
            skipped += 1;
            continue;
        }
        let (map, _) = osu::load(&maps[&h.map_md5])?;
        let mut replay = osr.to_replay();
        let chart = Arc::new(map.to_chart_with_mods(replay.mods));
        if std::env::var_os("TAKT_OSR_FLIP").is_some() && rsu_core::mods::flips(replay.mods) {
            for i in &mut replay.inputs {
                if let rsu_core::InputKind::Move { y, .. } = &mut i.kind {
                    *y = 384 * rsu_core::FX_ONE - *y;
                }
            }
        }
        let r = &replay.summary;
        // A failed or abandoned play judged fewer objects than the map has:
        // stop where its frames stop instead of missing the rest.
        let judged = (r.n300 + r.n100 + r.n50 + r.miss) as usize;
        let end = if judged < chart.objects.len() { replay.end_time } else { replay.end_time.max(end_time(&chart)) };
        let s = Game::simulate(chart.clone(), replay.mods, &replay.inputs, end);
        let counts = |s: &Summary| (s.n300, s.n100, s.n50, s.miss, s.max_combo);
        let ok = counts(&s) == counts(r);
        if ok {
            same += 1;
        } else {
            differ += 1;
        }
        println!(
            "{name}: {} {}  stable {}/{}/{}/{} x{}  rsu {}/{}/{}/{} x{}",
            if ok { "SAME  " } else { "DIFFER" },
            stable::mod_names(h.mods),
            r.n300,
            r.n100,
            r.n50,
            r.miss,
            r.max_combo,
            s.n300,
            s.n100,
            s.n50,
            s.miss,
            s.max_combo
        );
    }
    println!("{} replays: {same} same, {differ} differ, {skipped} skipped", files.len());
    Ok(differ == 0)
}

fn scores_db(dir: &Path) -> Res<bool> {
    let data = std::fs::read(dir.join("scores.db"))?;
    let scores = stable::scores_db(&data).ok_or("scores.db is not in a layout this reader knows")?;
    let mut modes = [0u32; 4];
    let (mut with_replay, mut foreign) = (0u32, 0u32);
    for s in &scores {
        modes[s.header.mode.min(3) as usize] += 1;
        if dir.join("Data/r").join(s.header.replay_file_name()).is_file() {
            with_replay += 1;
        }
        if stable::foreign_mods(s.header.mods) != 0 {
            foreign += 1;
        }
    }
    println!(
        "{} scores (standard {}, taiko {}, catch {}, mania {}); {} have their replay in Data/r; {} use mods rsu lacks",
        scores.len(),
        modes[0],
        modes[1],
        modes[2],
        modes[3],
        with_replay,
        foreign
    );
    for s in scores.iter().rev().take(5) {
        let h = &s.header;
        println!(
            "  {} {} {} {}/{}/{}/{} x{} {}",
            h.unix_time(),
            h.player,
            h.score,
            h.n300,
            h.n100,
            h.n50,
            h.miss,
            h.max_combo,
            stable::mod_names(h.mods)
        );
    }
    Ok(true)
}
