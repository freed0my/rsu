use std::process::Command;

#[test]
fn ar_requires_a_beatmap() {
    let output = Command::new(env!("CARGO_BIN_EXE_rsu"))
        .args(["--ar", "5"])
        .output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--ar requires a beatmap"), "{stderr}");
}
