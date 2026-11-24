//! Integration tests verifying interactive commands reject non-TTY pipes with
//! exit code 2 (`Exit::Usage`) without issuing network requests.

use std::process::{Command, Stdio};

#[test]
fn interactive_commands_reject_non_tty_with_usage_exit() {
    let bin = env!("CARGO_BIN_EXE_flyco");
    for args in [
        &[][..],
        &["claude"][..],
        &["codex"][..],
        &["devin"][..],
        &["antigravity"][..],
        &["resume", "--last"][..],
    ] {
        let output = Command::new(bin)
            .args(args)
            .env("FLYCO_TOKEN", "fk_test_non_tty_token")
            .env("FLYCO_API_URL", "http://127.0.0.1:9")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .expect("spawn flyco binary");
        assert_eq!(
            output.status.code(),
            Some(2),
            "args={args:?} stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("flyco run"),
            "args={args:?} expected non-interactive hint, got: {stderr}"
        );
    }
}
