use std::path::Path;
use std::process::Command;

#[test]
fn printed_settings_are_the_settings_passed_at_launch() {
    let exe = env!("CARGO_BIN_EXE_imacs");
    for agent in ["codex", "claude"] {
        let output = Command::new(exe)
            .args(["print-hook-settings", "--agent", agent, "--pane", "fixture-pane"])
            .env("IMACS_SOCKET", "/tmp/imacs-fixture.sock")
            .output()
            .unwrap();
        assert!(output.status.success());
        let printed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(
            printed,
            imacs_core::hook::session_settings(
                Path::new(exe),
                Path::new("/tmp/imacs-fixture.sock"),
                "fixture-pane",
                agent == "codex"
            )
        );
    }
}
