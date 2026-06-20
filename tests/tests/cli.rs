use std::{fs, process::Command};

use tempfile::tempdir;

use crate::rojo_test::{io_util::ROJO_PATH, serve_util::TestServeSession};

/// `rojo gen script` creates a file from a template, and re-running leaves an
/// existing file untouched (reported as skipped).
#[test]
fn gen_script_creates_and_skips() {
    let dir = tempdir().unwrap();
    let src = dir.path().join("src");
    let src_str = src.to_str().unwrap();

    let first = Command::new(ROJO_PATH)
        .args([
            "gen", "script", "Foo", "--kind", "module", "--path", src_str,
        ])
        .output()
        .expect("failed to run rojo gen");
    assert!(
        first.status.success(),
        "gen failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let file = src.join("Foo.luau");
    assert!(file.exists(), "expected the generated file to exist");
    assert!(fs::read_to_string(&file).unwrap().contains("Foo module"));

    // Editing then re-generating must not clobber the existing file.
    fs::write(&file, "-- custom edits").unwrap();
    let second = Command::new(ROJO_PATH)
        .args([
            "gen", "script", "Foo", "--kind", "module", "--path", src_str, "--json",
        ])
        .output()
        .expect("failed to run rojo gen");
    assert!(second.status.success());

    let stdout = String::from_utf8_lossy(&second.stdout);
    assert!(
        stdout.contains("\"created\": []"),
        "expected nothing created on re-run: {stdout}"
    );
    assert_eq!(
        fs::read_to_string(&file).unwrap(),
        "-- custom edits",
        "existing file should be left untouched",
    );
}

/// `rojo status`/`rojo stop` find a running server via the serve-state file and
/// control it.
#[test]
fn status_and_stop_via_cli() {
    let mut session = TestServeSession::new("empty");
    session.wait_to_come_online();
    let project = session.path().to_str().unwrap().to_owned();

    let status = Command::new(ROJO_PATH)
        .args(["status", &project, "--json"])
        .output()
        .expect("failed to run rojo status");
    let stdout = String::from_utf8_lossy(&status.stdout);
    assert!(
        stdout.contains("\"running\": true"),
        "expected status to report running: {stdout}"
    );

    let stop = Command::new(ROJO_PATH)
        .args(["stop", &project, "--json"])
        .output()
        .expect("failed to run rojo stop");
    assert!(
        stop.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );
    assert!(String::from_utf8_lossy(&stop.stdout).contains("\"stopped\": true"));

    session.wait_until_offline();

    let status = Command::new(ROJO_PATH)
        .args(["status", &project, "--json"])
        .output()
        .expect("failed to run rojo status");
    assert!(
        String::from_utf8_lossy(&status.stdout).contains("\"running\": false"),
        "expected status to report not running after stop",
    );
}
