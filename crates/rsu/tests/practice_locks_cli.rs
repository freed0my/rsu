use std::process::Command;

#[test]
fn practice_locks_are_documented_and_reject_non_practice_modes() {
    let rsu = env!("CARGO_BIN_EXE_rsu");
    for flag in ["--ar-lock", "--od-lock"] {
        let help = Command::new(rsu).arg("--help").output().unwrap();
        assert!(help.status.success());
        assert!(String::from_utf8_lossy(&help.stdout).contains(flag));
        for args in [vec![], vec!["--pattern"], vec!["--replay", "old.tkr", "map.osu"]] {
            let result = Command::new(rsu).arg(flag).args(args).output().unwrap();
            assert_eq!(result.status.code(), Some(2));
            assert!(String::from_utf8_lossy(&result.stderr)
                .contains(&format!("{flag} requires a beatmap and cannot be used with replay or pattern")));
        }
    }
}
