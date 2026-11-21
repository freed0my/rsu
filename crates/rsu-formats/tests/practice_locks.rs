use std::sync::Arc;

use rsu_core::{mods, Button, Game, Input, InputKind};
use rsu_formats::osu::{parse, PracticeDifficulty};

#[test]
fn practice_locks_scale_only_the_selected_timings_and_use_mod_fallback() {
    let map = parse(
        b"osu file format v14\n[Difficulty]\nApproachRate:5\nOverallDifficulty:5\n[HitObjects]\n100,100,2000,1,0\n",
    )
    .unwrap();
    for (game_mods, pct) in [
        (mods::HARD_ROCK | mods::DOUBLE_TIME, 0),
        (mods::EASY | mods::HALF_TIME, 0),
        (mods::DOUBLE_TIME, 5),
        (mods::HALF_TIME, 250),
        (0, 100),
        (0, 137),
    ] {
        let rate = mods::speed_with(game_mods, pct);
        for (ar_lock, od_lock) in [(true, false), (false, true), (true, true)] {
            for (ar, od) in [(None, None), (Some(0.0), Some(10.0))] {
                let practice = PracticeDifficulty { ar, od, ar_lock, od_lock, ..Default::default() };
                let baseline =
                    map.to_chart_with_practice(game_mods, PracticeDifficulty { ar, od, ..Default::default() });
                let locked = map.to_chart_with_practice_at_rate(game_mods, practice, rate);
                let scaled = |us: i64, lock| if lock { (us as f64 * rate).round() as i64 } else { us };
                assert_eq!(locked.preempt, scaled(baseline.preempt, ar_lock));
                assert_eq!(locked.fade_in, scaled(baseline.fade_in, ar_lock));
                assert_eq!(locked.window_300, scaled(baseline.window_300, od_lock));
                assert_eq!(locked.window_100, scaled(baseline.window_100, od_lock));
                assert_eq!(locked.window_50, scaled(baseline.window_50, od_lock));
                assert_eq!(locked.score_multiplier, baseline.score_multiplier);
                assert_eq!(locked.radius, baseline.radius);
                assert_eq!(locked.objects[0].t, baseline.objects[0].t);
            }
        }
    }
}

#[test]
fn practice_ar_lock_uses_compensated_preempt_for_stacking() {
    let map = parse(b"osu file format v14\n[General]\nStackLeniency:0.7\n[Difficulty]\nApproachRate:10\n[HitObjects]\n100,100,2000,1,0\n100,100,2400,1,0\n").unwrap();
    let unlocked = map.to_chart_with_mods(mods::DOUBLE_TIME);
    let locked =
        map.to_chart_with_practice(mods::DOUBLE_TIME, PracticeDifficulty { ar_lock: true, ..Default::default() });
    assert_eq!(unlocked.objects[0].x, unlocked.objects[1].x);
    assert_ne!(locked.objects[0].x, locked.objects[1].x);
}

#[test]
fn practice_od_lock_preserves_actual_judgements_at_wall_clock_offsets() {
    let map =
        parse(b"osu file format v14\n[Difficulty]\nOverallDifficulty:0\n[HitObjects]\n100,100,2000,1,0\n").unwrap();
    let run = |rate: f64, od_lock, wall_offset| {
        let chart =
            Arc::new(map.to_chart_with_practice_at_rate(0, PracticeDifficulty { od_lock, ..Default::default() }, rate));
        let o = chart.objects[0];
        let offset = (wall_offset as f64 * rate).round() as i64;
        let input = [
            Input { t: 0, kind: InputKind::Move { x: o.x, y: o.y } },
            Input { t: o.t + offset, kind: InputKind::Press(Button::K1) },
        ];
        let s = Game::simulate(chart, 0, &input, o.t + 600_000);
        (s.n300, s.n100, s.n50, s.miss)
    };
    for rate in [0.05, 0.75, 1.0, 1.37, 1.5, 2.5] {
        for (offset, expected) in [
            (-110_000, (0, 1, 0, 0)),
            (60_000, (1, 0, 0, 0)),
            (110_000, (0, 1, 0, 0)),
            (180_000, (0, 0, 1, 0)),
            (210_000, (0, 0, 0, 1)),
        ] {
            assert_eq!(run(rate, true, offset), expected, "rate={rate}, offset={offset}");
        }
    }
    assert_eq!(run(2.5, false, 60_000), (0, 0, 1, 0));
}
