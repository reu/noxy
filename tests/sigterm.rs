//! Verifies the `noxy` binary shuts down cleanly on SIGTERM (the signal
//! Kubernetes / systemd / `docker stop` send), not just Ctrl-C/SIGINT.

#![cfg(unix)]

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

#[tokio::test]
async fn sigterm_triggers_clean_shutdown() {
    // Only runs when the CLI binary is built (e.g. `--features cli`).
    let Some(bin) = option_env!("CARGO_BIN_EXE_noxy") else {
        eprintln!("noxy binary not built; skipping SIGTERM test");
        return;
    };

    // Start a reverse proxy on an ephemeral port; the upstream never needs to
    // exist, we only need the process to be up and listening.
    let mut child = Command::new(bin)
        .args(["--upstream", "http://127.0.0.1:9", "--port", "0"])
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn noxy binary");

    // Give it time to install the signal handler and bind.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "noxy should still be running before SIGTERM"
    );

    let pid = child.id().expect("child pid");
    let sent = std::process::Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("run kill");
    assert!(sent.success(), "failed to send SIGTERM");

    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .expect("noxy should exit promptly after SIGTERM")
        .expect("wait for noxy");

    assert!(
        status.success(),
        "SIGTERM should cause a clean (exit 0) shutdown, got {status:?}"
    );
}
