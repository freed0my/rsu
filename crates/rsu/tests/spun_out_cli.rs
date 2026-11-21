use std::process::Command;

#[test]
fn spun_out_cannot_be_combined_with_autoplay() {
    let output = Command::new(env!("CARGO_BIN_EXE_rsu"))
        .args(["missing.osu", "--mods", "SO", "--auto"])
        .output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("--auto cannot be combined with SO"), "{stderr}");
}

#[test]
fn spun_out_is_parsed_before_replay_validation() {
    let output = Command::new(env!("CARGO_BIN_EXE_rsu"))
        .args(["map.osu", "--mods", "HDSO", "--replay", "old.tkr"])
        .output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("a replay already carries the mods"), "{stderr}");
}
