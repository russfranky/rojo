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

/// `rojo gen script` rejects names that could escape the target directory or
/// break generated Luau, and still accepts ordinary names.
#[test]
fn gen_rejects_unsafe_names() {
    let dir = tempdir().unwrap();
    let dir_str = dir.path().to_str().unwrap();

    for bad in ["../evil", "/abs", "", "a/b", ".."] {
        let out = Command::new(ROJO_PATH)
            .args(["gen", "script", bad, "--kind", "module", "--path", dir_str])
            .output()
            .expect("failed to run rojo gen");
        assert!(
            !out.status.success(),
            "expected gen to reject unsafe name {bad:?}"
        );
    }

    let ok = Command::new(ROJO_PATH)
        .args(["gen", "script", "Ok", "--kind", "module", "--path", dir_str])
        .output()
        .expect("failed to run rojo gen");
    assert!(ok.status.success(), "expected gen to accept a normal name");
}

/// Restarting repeatedly keeps the server reachable with a stable session id and
/// an intact serve-state file (no clobber by the exiting predecessor).
#[test]
fn restart_preserves_session_and_state() {
    let mut session = TestServeSession::new("empty");
    session.wait_to_come_online();
    let project = session.path().to_str().unwrap().to_owned();
    let before = session.get_api_rojo().unwrap().session_id.to_string();

    // Stop whatever server is live for this project when the test ends, even on
    // a panic — `rojo restart` detaches the replacement from this process.
    let _cleanup = StopOnDrop(project.clone());

    for _ in 0..3 {
        let restart = Command::new(ROJO_PATH)
            .args(["restart", &project, "--json"])
            .output()
            .expect("failed to run rojo restart");
        assert!(
            restart.status.success(),
            "restart failed: {}",
            String::from_utf8_lossy(&restart.stderr)
        );

        let status = Command::new(ROJO_PATH)
            .args(["status", &project, "--json"])
            .output()
            .expect("failed to run rojo status");
        let stdout = String::from_utf8_lossy(&status.stdout);
        assert!(
            stdout.contains("\"running\": true"),
            "server not reachable after restart: {stdout}"
        );
        assert!(
            stdout.contains(&before),
            "session id changed across restart: {stdout}"
        );
    }
}

/// Stops the Rojo server for a project when dropped, for test cleanup.
struct StopOnDrop(String);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        let _ = Command::new(ROJO_PATH).args(["stop", &self.0]).output();
    }
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
