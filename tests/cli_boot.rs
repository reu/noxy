//! Regression test: the `noxy` binary must install a rustls crypto provider on
//! startup, otherwise it panics ("Could not automatically determine the
//! process-level CryptoProvider") the moment it builds any TLS config.

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;

#[tokio::test]
async fn cli_boots_without_crypto_provider_panic() {
    let Some(bin) = option_env!("CARGO_BIN_EXE_noxy") else {
        eprintln!("noxy binary not built; skipping");
        return;
    };

    // Reverse mode builds an upstream rustls ClientConfig at startup, which is
    // exactly what panics without an installed provider.
    let mut child = Command::new(bin)
        .args(["--upstream", "http://127.0.0.1:9", "--port", "0"])
        .kill_on_drop(true)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn noxy binary");

    tokio::time::sleep(Duration::from_millis(800)).await;

    // If the provider weren't installed, the process would have already panicked
    // and exited; still running means it booted cleanly.
    match child.try_wait().expect("poll child") {
        None => {
            child.start_kill().ok();
        }
        Some(status) => panic!("noxy exited on startup instead of running: {status:?}"),
    }
}
