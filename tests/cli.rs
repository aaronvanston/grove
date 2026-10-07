//! The installed command line: the built binary run in a throwaway HOME and
//! GROVE_HOME. Expected envelopes, codes and messages come from what was
//! printed for the same commands.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};

/// Stands in for ssh: `-G` finds no ping target, cedar-01 answers with a
/// Linux reading captured from a sample script (names replaced), and any
/// other host refuses the connection.
const SSH_STUB: &str = r#"#!/bin/sh
case " $* " in *" -G "*) exit 255 ;; esac
while [ "$1" != "--" ]; do shift; done
cat >/dev/null
if [ "$2" != cedar-01 ]; then
  echo "ssh: connect to host $2 port 22: Connection refused" >&2
  exit 255
fi
cat <<'OUT'
hostname=cedar-01
os=Linux
arch=x86_64
cores=24
load1=0.07
load5=0.16
load15=0.21
cpu_pct=0.7
mem_total_kb=65618936
mem_available_kb=55493984
disk_total_kb=982292956
disk_used_kb=510181132
net_rx_bytes=20474573431
net_tx_bytes=22516176468
ip=192.0.2.26
model=MS-7D25
os_version=Ubuntu 26.04 LTS
swap_total_kb=33554424
swap_used_kb=8171072
cpu_temp_c=32.0
gpu_temp_c=40
agent_sessions=1
claude_sessions=1
codex_sessions=0
OUT
"#;

struct Sandbox {
    root: PathBuf,
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Out {
    fn data(&self) -> Value {
        let envelope: Value = serde_json::from_str(&self.stdout).expect("a JSON envelope");
        assert_eq!(envelope["ok"], true, "{}", self.stdout);
        assert_eq!(envelope["schemaVersion"], 1);
        envelope["data"].clone()
    }
}

impl Sandbox {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let base = std::env::temp_dir().canonicalize().expect("temp dir");
        let root = base.join(format!(
            "grove-cli-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        assert!(root.starts_with(&base));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).expect("home");
        let stub = root.join("ssh");
        std::fs::write(&stub, SSH_STUB).expect("stub");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("mode");
        // Service managers are stubs that only note the call, so nothing
        // here registers with the real launchd or systemd.
        std::fs::create_dir_all(root.join("guard")).expect("guard");
        for tool in ["launchctl", "systemctl", "loginctl"] {
            let path = root.join("guard").join(tool);
            let log = root.join("guard.log");
            std::fs::write(
                &path,
                format!("#!/bin/sh\necho \"{tool} $*\" >>'{}'\n", log.display()),
            )
            .expect("stub");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("mode");
        }
        Self { root }
    }

    fn run(&self, args: &[&str]) -> Out {
        let output = Command::new(env!("CARGO_BIN_EXE_grove"))
            .args(args)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("GROVE_HOME", self.root.join("state"))
            .env(
                "PATH",
                format!(
                    "{}:/usr/bin:/bin:/usr/sbin:/sbin",
                    self.root.join("guard").display()
                ),
            )
            .env("GROVE_SSH_COMMAND", self.root.join("ssh"))
            .stdin(Stdio::null())
            .output()
            .expect("runs");
        Out {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A machine record with its timestamps checked and blanked.
fn without_times(mut record: Value) -> Value {
    let added = record["added_at"].as_str().expect("added_at").to_owned();
    assert!(added.ends_with('Z') && added.len() == 24, "{added}");
    record["added_at"] = json!("-");
    record
}

#[test]
fn a_machine_is_added_labeled_listed_and_removed() {
    let sandbox = Sandbox::new();
    let added = sandbox.run(&[
        "add",
        "cam-mbp",
        "cam-mbp.local",
        "--trust",
        " personal ",
        "--json",
    ]);
    assert_eq!(added.code, 0, "{}", added.stderr);
    assert_eq!(
        without_times(added.data()),
        json!({
            "added_at": "-",
            "config": { "checked_at": null, "commit": null, "verify": null },
            "endpoint": "cam-mbp.local",
            "labels": { "locality": null, "power": null, "privacy": null, "trust": "personal" },
            "name": "cam-mbp",
            "port": 22,
        })
    );
    // Only the labels passed change, and an empty value clears one.
    let labeled = sandbox.run(&[
        "label", "cam-mbp", "--power", "battery", "--trust", "", "--json",
    ]);
    assert_eq!(labeled.code, 0, "{}", labeled.stderr);
    assert_eq!(
        labeled.data()["labels"],
        json!({ "locality": null, "power": "battery", "privacy": null, "trust": null })
    );
    let listed = sandbox.run(&["ls", "--json"]);
    assert_eq!(listed.data().as_array().map(Vec::len), Some(1));
    let removed = sandbox.run(&["rm", "cam-mbp", "--yes", "--json"]);
    assert_eq!(
        removed.data(),
        json!({ "name": "cam-mbp", "removed": true })
    );
    assert_eq!(sandbox.run(&["list", "--json"]).data(), json!([]));
}

#[test]
fn failures_carry_their_codes_and_exit_codes() {
    let sandbox = Sandbox::new();
    assert_eq!(sandbox.run(&["add", "cam-mbp", "cam-mbp.local"]).code, 0);
    let cases: [(&[&str], i32, &str, &str); 8] = [
        (
            &["add", "cam-mbp", "x"],
            1,
            "machine_exists",
            "A machine named \"cam-mbp\" is already registered.",
        ),
        (
            &["add", "bad name", "x"],
            2,
            "invalid_machine_name",
            "\"bad name\" is not a valid machine name.",
        ),
        (
            &["add", "ok", "a b"],
            2,
            "invalid_endpoint",
            "\"a b\" is not a valid endpoint.",
        ),
        (
            &["label", "cam-mbp"],
            2,
            "no_labels_given",
            "Labeling a machine needs a label to set.",
        ),
        (
            &["label", "nobody", "--trust", "x"],
            1,
            "machine_not_found",
            "No machine named \"nobody\" is registered.",
        ),
        (
            &["rm", "cam-mbp"],
            2,
            "action_required",
            "Removing a machine needs explicit confirmation.",
        ),
        (
            &["bogus"],
            2,
            "invalid_usage",
            "error: unknown command 'bogus'",
        ),
        (
            &["list", "--jsonl"],
            2,
            "conflicting_output_modes",
            "Use either --json or --jsonl, not both.",
        ),
    ];
    for (args, code, error_code, message) in cases {
        let mut argv = args.to_vec();
        argv.push("--json");
        let out = sandbox.run(&argv);
        // Commander's own line comes first for parse errors; the envelope
        // is the last JSON document on stderr.
        let stderr = out
            .stderr
            .find("{\n")
            .map_or("", |start| &out.stderr[start..]);
        let error: Value =
            serde_json::from_str(stderr).unwrap_or_else(|_| panic!("{args:?}: {}", out.stderr));
        assert_eq!(
            (
                out.code,
                error["error"]["code"].as_str(),
                error["error"]["message"].as_str()
            ),
            (code, Some(error_code), Some(message)),
            "{args:?}"
        );
    }
    // Human mode says the same thing in a line, and nothing on stdout.
    let human = sandbox.run(&["rm", "nobody", "--yes"]);
    assert_eq!(human.code, 1);
    assert!(human.stdout.is_empty());
    assert!(
        human
            .stderr
            .contains("No machine named \"nobody\" is registered."),
        "{}",
        human.stderr
    );
}

/// Sampling is best-effort: the run exits 0 with the failure in its
/// result, while status, the gate, exits 1. A reading carries the split
/// agent count and health, and --min-store-interval keeps the history sparse.
#[test]
fn sampling_reports_each_machine_and_status_gates_on_reachability() {
    let sandbox = Sandbox::new();
    for (name, endpoint) in [
        ("cedar-01", "cedar-01"),
        ("down-01", "down-01"),
        ("here", "localhost"),
    ] {
        assert_eq!(sandbox.run(&["add", name, endpoint]).code, 0);
    }
    let sampled = sandbox.run(&["sample", "--json"]);
    assert_eq!(sampled.code, 0, "{}", sampled.stderr);
    let data = sampled.data();
    assert_eq!((&data["failed"], &data["stored"]), (&json!(1), &json!(2)));
    let [cedar, down, here] = [0, 1, 2].map(|index| data["machines"][index].clone());
    assert_eq!(
        down["error"],
        "ssh: connect to host down-01 port 22: Connection refused"
    );
    assert_eq!(
        (&down["ok"], &down["sample"]),
        (&json!(false), &Value::Null)
    );
    let reading = &cedar["sample"];
    for (key, value) in [
        ("agent_sessions", json!(1)),
        (
            "agents",
            json!({ "claude": 1, "codex": 0, "cpu_pct": null, "rss_mb": null }),
        ),
        (
            "health",
            json!({ "score": 100, "status": "healthy", "reason": null }),
        ),
        ("net_rx_bps", Value::Null),
    ] {
        assert_eq!(reading[key], value, "{key}");
    }
    // This machine is read with sh, not over SSH, and isn't pinged.
    assert_eq!(here["ok"], true, "{}", here["error"]);
    assert_eq!(here["sample"]["ping_target"], Value::Null);

    let status = sandbox.run(&["status", "--json"]);
    assert_eq!(status.code, 1);
    let data = status.data();
    assert_eq!((&data["up"], &data["down"]), (&json!(2), &json!(1)));
    assert_eq!(data["machines"][0]["health"]["status"], "healthy");
    assert_eq!(data["machines"][1]["health"]["status"], "unreachable");

    let again = sandbox.run(&["sample", "cedar-01", "--min-store-interval", "1h", "--json"]);
    let data = again.data();
    assert_eq!(
        (&data["stored"], &data["machines"][0]["stored"]),
        (&json!(0), &json!(false))
    );
    assert!(data["machines"][0]["sample"]["net_rx_bps"].is_number());
    // show has the last reading, stored or not; history only what was stored.
    let shown = sandbox.run(&["show", "cedar-01", "--json"]).data();
    assert_eq!(shown["history"]["count"], 1);
    assert!(shown["latest"]["net_rx_bps"].is_number());
    assert_eq!(
        (&shown["machine"]["model"], &shown["machine"]["ip"]),
        (&json!("MS-7D25"), &json!("192.0.2.26"))
    );
    let history = sandbox.run(&["history", "cedar-01", "--json"]).data();
    assert_eq!(history["samples"].as_array().map(Vec::len), Some(1));
    let refused = sandbox.run(&["sample", "--min-store-interval", "soon", "--json"]);
    assert_eq!(refused.code, 2);
    assert!(
        refused.stderr.contains("invalid_duration"),
        "{}",
        refused.stderr
    );
}

#[test]
fn samples_are_kept_for_the_retention_and_pruned_on_demand() {
    let sandbox = Sandbox::new();
    let retention = |args: &[&str]| {
        sandbox
            .run(&[&["retention"], args, &["--json"]].concat())
            .data()
    };
    assert_eq!(
        retention(&[]),
        json!({ "retention": "90d", "retention_ms": 7_776_000_000_i64 })
    );
    assert_eq!(retention(&["30d"])["retention"], "30d");
    assert_eq!(
        retention(&["off"]),
        json!({ "retention": null, "retention_ms": null })
    );
    assert_eq!(sandbox.run(&["retention", "0s", "--json"]).code, 2);

    assert_eq!(sandbox.run(&["add", "cedar-01", "cedar-01"]).code, 0);
    assert_eq!(sandbox.run(&["sample", "--json"]).code, 0);
    let off = sandbox.run(&["prune", "--json"]).data();
    assert_eq!(off, json!({ "before": null, "deleted": 0 }));
    let pruned = sandbox
        .run(&["prune", "--older-than", "0s", "--json"])
        .data();
    assert_eq!(pruned["deleted"], 1);
    assert_eq!(
        sandbox.run(&["show", "cedar-01", "--json"]).data()["history"]["count"],
        0
    );
}

/// A down rule fires through sampling and runs its hook after the run;
/// `alerts state` and `policy explain` then gate on what was recorded.
/// The hook only appends to a file in the sandbox.
#[test]
fn alerts_fire_hooks_run_and_policy_explains_the_verdict() {
    let sandbox = Sandbox::new();
    assert_eq!(sandbox.run(&["add", "cedar-01", "cedar-01"]).code, 0);
    assert_eq!(sandbox.run(&["add", "down-01", "down-01"]).code, 0);
    assert_eq!(
        sandbox
            .run(&["alerts", "add", "down", "--for", "1s", "--json"])
            .code,
        0
    );
    let log = sandbox.root.join("hook.log");
    let command = format!(
        "printf '%s %s %s\\n' \"$GROVE_EVENT\" \"$GROVE_MACHINE\" \"$GROVE_METRIC\" >> '{}'; cat >/dev/null",
        log.display()
    );
    assert_eq!(
        sandbox
            .run(&["hooks", "add", "log", &command, "--json"])
            .code,
        0
    );
    assert_eq!(sandbox.run(&["sample", "--json"]).code, 0);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    assert_eq!(
        sandbox.run(&["sample", "--json"]).code,
        0,
        "hooks never change the exit code"
    );
    assert_eq!(
        std::fs::read_to_string(&log).unwrap_or_default(),
        "fire down-01 down\n"
    );
    let state = sandbox.run(&["alerts", "state", "--json"]);
    assert_eq!(state.code, 1);
    assert_eq!(state.data()["firing"], 1);
    let runs = sandbox.run(&["hooks", "runs", "--json"]).data();
    assert_eq!(
        (&runs[0]["exit_code"], &runs[0]["metric"]),
        (&json!(0), &json!("down"))
    );

    assert_eq!(
        sandbox
            .run(&["policy", "set", "--max-sessions", "1", "--json"])
            .code,
        0
    );
    let explained = sandbox.run(&["policy", "explain", "cedar-01", "--json"]);
    assert_eq!(explained.code, 1);
    let data = explained.data();
    assert_eq!(data["reasons"][0], "at_capacity", "{data}");
    assert_eq!(
        (&data["sessions"]["used"], &data["sessions"]["cap_scope"]),
        (&json!(1), &json!("fleet"))
    );
    let down = sandbox
        .run(&["policy", "explain", "down-01", "--json"])
        .data();
    assert_eq!(
        down["reasons"],
        json!([
            "unreachable",
            "no_capacity",
            "unknown_sessions",
            "alerts_firing"
        ])
    );
}

/// The probe binary the workspace built beside grove.
fn probe_binary() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_BIN_EXE_grove")).with_file_name("grove-probe");
    assert!(
        path.is_file(),
        "build the workspace first: {}",
        path.display()
    );
    path
}

/// A release folder holding one probe archive for this machine and its
/// SHA256SUMS line.
fn release(sandbox: &Sandbox) -> PathBuf {
    let dist = sandbox.root.join("dist");
    let stage = sandbox.root.join("stage");
    std::fs::create_dir_all(&dist).expect("dist");
    std::fs::create_dir_all(&stage).expect("stage");
    std::fs::copy(probe_binary(), stage.join("grove-probe")).expect("copy");
    let target = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", _) => "darwin-x64",
        (_, "aarch64") => "linux-arm64",
        _ => "linux-x64",
    };
    let archive = format!("grove-probe-{}-{target}.tar.gz", env!("CARGO_PKG_VERSION"));
    let packed = Command::new("tar")
        .arg("-czf")
        .arg(dist.join(&archive))
        .arg("-C")
        .arg(&stage)
        .arg("grove-probe")
        .status()
        .expect("tar");
    assert!(packed.success());
    let sum = Command::new("sh")
        .arg("-c")
        .arg(format!("cd '{}' && (sha256sum '{archive}' 2>/dev/null || shasum -a 256 '{archive}') > SHA256SUMS", dist.display()))
        .status()
        .expect("sum");
    assert!(sum.success());
    dist
}

/// --no-service installs the probe without touching launchd or systemd.
#[test]
fn the_probe_installs_unsupervised_with_no_service() {
    let sandbox = Sandbox::new();
    assert_eq!(sandbox.run(&["add", "here", "localhost"]).code, 0);
    let dist = release(&sandbox);
    let place = sandbox.root.join("remote");
    let installed = sandbox.run(&[
        "probe",
        "install",
        "here",
        "--from",
        &dist.display().to_string(),
        "--dir",
        &place.display().to_string(),
        "--no-service",
        "--json",
    ]);
    assert_eq!(installed.code, 0, "{}", installed.stderr);
    assert_eq!(installed.data()["service"], false);
    assert!(place.join("grove-probe").is_file());
    let log = std::fs::read_to_string(sandbox.root.join("guard.log")).unwrap_or_default();
    assert!(
        !log.contains("launchctl") && !log.contains("systemctl") && !log.contains("loginctl"),
        "{log}"
    );
    assert!(
        !sandbox
            .root
            .join("home/Library/LaunchAgents/dev.grove.probe.plist")
            .exists()
    );
    assert!(
        !sandbox
            .root
            .join("home/.config/systemd/user/grove-probe.service")
            .exists()
    );
}

/// Install checks the archive, unpacks the probe where asked and hands it
/// to the service manager; uninstall takes all of it away again. A
/// tampered archive never reaches the machine.
#[test]
fn the_probe_installs_supervised_and_uninstalls() {
    let sandbox = Sandbox::new();
    assert_eq!(sandbox.run(&["add", "here", "localhost"]).code, 0);
    let dist = release(&sandbox);
    let place = sandbox.root.join("remote");
    let place_text = place.display().to_string();
    let installed = sandbox.run(&[
        "probe",
        "install",
        "here",
        "--from",
        &dist.display().to_string(),
        "--dir",
        &place_text,
        "--json",
    ]);
    assert_eq!(installed.code, 0, "{}", installed.stderr);
    assert_eq!(installed.data()["service"], true);
    assert!(place.join("grove-probe").is_file() && place.join("data").is_dir());
    let log = std::fs::read_to_string(sandbox.root.join("guard.log")).unwrap_or_default();
    if cfg!(target_os = "macos") {
        assert!(
            sandbox
                .root
                .join("home/Library/LaunchAgents/dev.grove.probe.plist")
                .is_file()
        );
        assert!(log.contains("launchctl bootstrap gui/"), "{log}");
    } else {
        assert!(
            sandbox
                .root
                .join("home/.config/systemd/user/grove-probe.service")
                .is_file()
        );
        assert!(
            log.contains("systemctl --user restart grove-probe.service"),
            "{log}"
        );
    }
    let removed = sandbox.run(&["probe", "uninstall", "here", "--json"]);
    assert_eq!(removed.code, 0, "{}", removed.stderr);
    assert!(!place.exists());
    assert!(
        !sandbox
            .root
            .join("home/Library/LaunchAgents/dev.grove.probe.plist")
            .exists()
    );

    let archive = std::fs::read_dir(&dist)
        .unwrap()
        .flatten()
        .find(|entry| entry.file_name().to_string_lossy().ends_with(".tar.gz"))
        .unwrap()
        .path();
    std::fs::write(&archive, b"tampered").unwrap();
    let refused = sandbox.run(&[
        "probe",
        "install",
        "here",
        "--from",
        &dist.display().to_string(),
        "--json",
    ]);
    assert_eq!(refused.code, 1);
    assert!(
        refused.stderr.contains("checksum_mismatch"),
        "{}",
        refused.stderr
    );
}

/// A running probe streams into the store: readings arrive within the
/// interval with their latency, sample answers from them, and graph
/// draws the hour at full resolution.
#[test]
fn a_stream_collects_from_a_running_probe() {
    let sandbox = Sandbox::new();
    assert_eq!(sandbox.run(&["add", "here", "localhost"]).code, 0);
    let place = sandbox.root.join("probe");
    std::fs::create_dir_all(&place).expect("place");
    std::fs::copy(probe_binary(), place.join("grove-probe")).expect("copy");
    let mut probe = Command::new(place.join("grove-probe"))
        .args(["run", "--dir"])
        .arg(place.join("data"))
        .env("HOME", sandbox.root.join("home"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("probe starts");
    let used = sandbox.run(&["probe", "use", "here", &place.display().to_string()]);
    assert_eq!(used.code, 0, "{}", used.stderr);
    let streamed = sandbox.run(&["stream", "--for", "7s", "--json"]);
    let _ = probe.kill();
    let _ = probe.wait();
    assert_eq!(streamed.code, 0, "{}", streamed.stderr);
    let machine = &streamed.data()["machines"][0];
    assert!(machine["readings"].as_u64().unwrap_or(0) >= 2, "{machine}");
    assert_eq!(machine["connects"], 1);
    assert!(
        machine["latency_ms"]["max"].as_i64().unwrap_or(i64::MAX) < 2000,
        "{machine}"
    );
    let sampled = sandbox.run(&["sample", "here", "--json"]).data();
    assert_eq!(sampled["machines"][0]["ok"], true);
    assert!(sampled["machines"][0]["sample"]["mem_used_pct"].is_number());
    let graph = sandbox
        .run(&["graph", "here", "--since", "10m", "--json"])
        .data();
    assert!(graph["samples"].as_u64().unwrap_or(0) >= 2);
}
