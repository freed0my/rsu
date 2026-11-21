#[test]
fn flip_left_right_is_a_recognized_cli_option() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rsu"))
        .args(["--flip-left-right", "--help"])
        .output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("unknown option '--flip-left-right'"), "{stderr}");
    assert!(output.status.success(), "{stderr}");
}

#[test]
fn flip_up_down_is_a_recognized_cli_option() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_rsu"))
        .args(["--flip-up-down", "--help"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("unknown option '--flip-up-down'"), "{stderr}");
    assert!(output.status.success(), "{stderr}");
}
