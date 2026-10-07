//! The probe's command line, as the collector and the installer call it.
//! (Having this suite also makes `cargo test` build the probe binary that
//! grove's own command-line tests stream from.)

use std::process::Command;

fn probe(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_grove-probe"))
        .args(args)
        .output()
        .expect("grove-probe runs")
}

#[test]
fn version_names_the_build_the_installer_checks() {
    let output = probe(&["version", "--json"]);
    assert!(output.status.success());
    let text = String::from_utf8(output.stdout).expect("utf-8");
    assert!(text.contains("\"name\":\"grove-probe\""), "{text}");
    assert!(
        text.contains(&format!("\"version\":\"{}\"", env!("CARGO_PKG_VERSION"))),
        "{text}"
    );
}

#[test]
fn status_fails_where_there_is_no_ring() {
    let dir = std::env::temp_dir().join(format!("grove-probe-test-{}", std::process::id()));
    let output = probe(&["status", "--dir", dir.to_str().expect("utf-8 path")]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no ring"));
}
