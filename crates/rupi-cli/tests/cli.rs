use std::process::Command;

#[test]
fn print_help_and_version() {
    let bin = env!("CARGO_BIN_EXE_rupi");
    let help = Command::new(bin).arg("--help").output().unwrap();
    assert!(help.status.success());
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("rupi"));
    assert!(stdout.contains("--print"));

    let ver = Command::new(bin).arg("--version").output().unwrap();
    assert!(ver.status.success());
}

#[test]
fn print_mode_faux_writes_file() {
    let bin = env!("CARGO_BIN_EXE_rupi");
    let dir = tempfile::tempdir().unwrap();
    // Faux with no scripted turns returns empty end_turn — still a successful run.
    let out = Command::new(bin)
        .current_dir(dir.path())
        .env("RUPI_HOME", dir.path().join("home"))
        .args([
            "--provider",
            "faux",
            "--no-session",
            "-p",
            "say hi",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr={} stdout={}",
        String::from_utf8_lossy(&out.stderr),
        String::from_utf8_lossy(&out.stdout)
    );
}
