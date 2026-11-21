use std::process::Command;

fn rejects(args: &[&str], message: &str) {
    let output = Command::new(env!("CARGO_BIN_EXE_rsu")).args(args).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(message), "{stderr}");
}

#[test]
fn cs_and_hp_require_a_beatmap_and_disallow_replay() {
    for flag in ["--cs", "--hp"] {
        let message = format!("{flag} requires a beatmap");
        rejects(&[flag, "0"], &message);
        rejects(&[flag, "5", "--pattern"], &message);
        rejects(&[flag, "5", "test.osu", "--replay", "test.tkr"], &message);
    }
}

#[test]
fn cs_and_hp_reject_out_of_range_and_nonfinite_values() {
    for flag in ["--cs", "--hp"] {
        for value in ["-0.1", "13", "NaN", "inf"] {
            rejects(&[flag, value, "test.osu"], &format!("{flag} must be between"));
        }
    }
}
