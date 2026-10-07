//! The probe commands (install, use, uninstall, status) and stream, the
//! long-running collector.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::commander::OptValue;
use super::machines::{open_store, require, targets};
use super::options::{duration, string};
use super::{Context, Done, NAME, VERSION};
use crate::errors::AppError;
use crate::output::{iso_ms, now_ms, num};
use crate::store::Machine;
use crate::{sha256, transport};

/// Where release archives come from unless --from says otherwise.
const RELEASES: &str = "https://github.com/aaronvanston/grove/releases/download";

/// The release target a machine needs, from `uname -sm`.
fn target_of(uname: &str) -> Option<&'static str> {
    let mut words = uname.split_whitespace();
    match (words.next()?, words.next()?) {
        ("Darwin", "arm64") => Some("darwin-arm64"),
        ("Darwin", "x86_64") => Some("darwin-x64"),
        ("Linux", "aarch64" | "arm64") => Some("linux-arm64"),
        ("Linux", "x86_64") => Some("linux-x64"),
        _ => None,
    }
}

fn install_failed(message: impl Into<String>) -> AppError {
    AppError::new("probe_install_failed", message)
}

/// The archive's bytes, from a folder or a release URL, checked against
/// the SHA256SUMS beside it.
fn fetch_archive(from: &str, archive: &str) -> Result<Vec<u8>, AppError> {
    let read = |name: &str| -> Result<Vec<u8>, AppError> {
        if from.starts_with("https://") || from.starts_with("http://") {
            let url = format!("{}/{name}", from.trim_end_matches('/'));
            let mut command = std::process::Command::new("curl");
            command.args(["-fsSL", &url]);
            let ran = transport::run(&mut command, "curl", None, Duration::from_secs(120));
            if ran.ok() {
                Ok(ran.stdout_bytes)
            } else {
                Err(install_failed(format!(
                    "Could not download {url}: {}.",
                    ran.failure()
                )))
            }
        } else {
            std::fs::read(Path::new(from).join(name)).map_err(|error| {
                install_failed(format!("Could not read {name} in {from}: {error}."))
            })
        }
    };
    let sums = String::from_utf8_lossy(&read("SHA256SUMS")?).into_owned();
    let expected = sums
        .lines()
        .find_map(|line| {
            let (hash, name) = line.split_once("  ")?;
            (name.trim() == archive).then(|| hash.trim().to_owned())
        })
        .ok_or_else(|| install_failed(format!("SHA256SUMS has no line for {archive}.")))?;
    let bytes = read(archive)?;
    let actual = sha256::hex(&bytes);
    if actual != expected {
        return Err(AppError::new(
            "checksum_mismatch",
            format!("{archive} doesn't match SHA256SUMS (expected {expected}, got {actual})."),
        ));
    }
    Ok(bytes)
}

/// The shell script that installs the probe from the archive on stdin
/// into "$1" (or ~/.grove-probe), and supervises it unless "$2" is "no".
const INSTALL: &str = r#"set -eu
dir=${1:-"$HOME/.grove-probe"}
mkdir -p "$dir/data"
new="$dir/.grove-probe.new"
tar -xzOf - grove-probe > "$new"
chmod 755 "$new"
"$new" version --json >/dev/null
mv -f "$new" "$dir/grove-probe"
if [ "${2:-yes}" != no ]; then
  if [ "$(uname -s)" = Darwin ]; then
    plist="$HOME/Library/LaunchAgents/dev.grove.probe.plist"
    mkdir -p "$(dirname "$plist")"
    cat > "$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>dev.grove.probe</string>
  <key>ProgramArguments</key>
  <array><string>$dir/grove-probe</string><string>run</string><string>--dir</string><string>$dir/data</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ProcessType</key><string>Background</string>
  <key>Nice</key><integer>10</integer>
</dict>
</plist>
PLIST
    uid=$(id -u)
    launchctl bootout "gui/$uid/dev.grove.probe" 2>/dev/null || true
    launchctl bootstrap "gui/$uid" "$plist"
  else
    unit="$HOME/.config/systemd/user/grove-probe.service"
    mkdir -p "$(dirname "$unit")"
    cat > "$unit" <<UNIT
[Unit]
Description=grove probe

[Service]
ExecStart=$dir/grove-probe run --dir $dir/data
Restart=always
RestartSec=5
Nice=10

[Install]
WantedBy=default.target
UNIT
    systemctl --user daemon-reload
    systemctl --user enable grove-probe.service
    systemctl --user restart grove-probe.service
    loginctl enable-linger "$(id -un)" 2>/dev/null || true
  fi
fi
cd "$dir" && printf 'dir=%s\n' "$(pwd -P)"
"#;

/// Stops supervision and removes the probe's folder, only when it holds a
/// grove-probe.
const UNINSTALL: &str = r#"set -u
dir=$1
if [ "$(uname -s)" = Darwin ]; then
  plist="$HOME/Library/LaunchAgents/dev.grove.probe.plist"
  if [ -f "$plist" ]; then
    launchctl bootout "gui/$(id -u)/dev.grove.probe" 2>/dev/null || true
    rm -f "$plist"
  fi
else
  unit="$HOME/.config/systemd/user/grove-probe.service"
  if [ -f "$unit" ]; then
    systemctl --user disable --now grove-probe.service 2>/dev/null || true
    rm -f "$unit"
    systemctl --user daemon-reload 2>/dev/null || true
  fi
fi
if [ -f "$dir/grove-probe" ]; then rm -rf "$dir"; fi
echo removed
"#;

fn run_with(
    machine: &Machine,
    home: &Path,
    script: &str,
    args: &[String],
    stdin: Option<&[u8]>,
) -> transport::Ran {
    let mut words = vec![
        "sh".to_owned(),
        "-c".to_owned(),
        script.to_owned(),
        "sh".to_owned(),
    ];
    words.extend(args.iter().cloned());
    let (mut command, program) = transport::command_on(machine, home, &words);
    transport::run(&mut command, program, stdin, Duration::from_secs(120))
}

pub fn install(context: &Context) -> Result<Done, AppError> {
    let name = context.argument(0).unwrap_or_default();
    let store = open_store()?;
    let machine = require(&store, &name)?;
    let uname = transport::run_script(
        &machine,
        &store.home,
        "uname -sm\n",
        Duration::from_secs(15),
    );
    if !uname.ok() {
        return Err(install_failed(format!(
            "Could not reach \"{name}\": {}.",
            uname.failure()
        )));
    }
    let target = target_of(&uname.stdout)
        .ok_or_else(|| install_failed(format!("No probe build for {}.", uname.stdout.trim())))?;
    let from = string(&context.options, "from").unwrap_or_else(|| format!("{RELEASES}/v{VERSION}"));
    let archive = format!("grove-probe-{VERSION}-{target}.tar.gz");
    let bytes = fetch_archive(&from, &archive)?;
    let dir = string(&context.options, "dir").unwrap_or_default();
    // Commander stores --no-service as service = false, the way --no-color is color = false.
    let service = !matches!(context.options.get("service"), Some(OptValue::Bool(false)));
    let ran = run_with(
        &machine,
        &store.home,
        INSTALL,
        &[dir, if service { "yes" } else { "no" }.to_owned()],
        Some(&bytes),
    );
    let installed = ran
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("dir="))
        .map(str::to_owned);
    let Some(installed) = installed.filter(|_| ran.ok()) else {
        return Err(install_failed(format!(
            "Installing on \"{name}\" failed: {}.",
            ran.failure()
        )));
    };
    store.set_probe(&name, Some(&installed))?;
    let ui = &context.ui;
    let human = format!(
        "{} Installed grove-probe {VERSION} ({target}) on {} in {installed}{}",
        ui.success(ui.symbols.success),
        ui.command(&name),
        if service { ", supervised" } else { "" }
    );
    Ok(Done::new(
        json!({ "dir": installed, "machine": name, "service": service, "target": target, "version": VERSION }),
        human,
    ))
}

pub fn use_dir(context: &Context) -> Result<Done, AppError> {
    let name = context.argument(0).unwrap_or_default();
    let dir = context.argument(1).unwrap_or_default();
    if !dir.starts_with('/') {
        return Err(AppError::usage(
            "invalid_probe_dir",
            format!("\"{dir}\" is not an absolute path."),
        )
        .hint("Pass the folder grove-probe was installed in, from the machine's root."));
    }
    let store = open_store()?;
    require(&store, &name)?;
    store.set_probe(&name, Some(dir.trim_end_matches('/')))?;
    let ui = &context.ui;
    let human = format!(
        "{} Reading {}'s probe in {dir}",
        ui.success(ui.symbols.success),
        ui.command(&name)
    );
    Ok(Done::new(json!({ "dir": dir, "machine": name }), human))
}

pub fn uninstall(context: &Context) -> Result<Done, AppError> {
    let name = context.argument(0).unwrap_or_default();
    let store = open_store()?;
    let machine = require(&store, &name)?;
    let Some(probe) = machine.probe.clone() else {
        return Err(
            AppError::new("probe_not_installed", format!("\"{name}\" has no probe."))
                .hint(format!("Install one with '{NAME} probe install {name}'.")),
        );
    };
    let ran = run_with(
        &machine,
        &store.home,
        UNINSTALL,
        std::slice::from_ref(&probe.dir),
        None,
    );
    if !ran.ok() {
        return Err(AppError::new(
            "probe_uninstall_failed",
            format!(
                "Removing the probe from \"{name}\" failed: {}.",
                ran.failure()
            ),
        ));
    }
    store.set_probe(&name, None)?;
    let ui = &context.ui;
    let human = format!(
        "{} Removed the probe from {}",
        ui.success(ui.symbols.success),
        ui.command(&name)
    );
    Ok(Done::new(
        json!({ "dir": probe.dir, "machine": name, "removed": true }),
        human,
    ))
}

pub fn status(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    let machines: Vec<Machine> = targets(&store, context.argument(0).as_deref())?
        .into_iter()
        .filter(|machine| machine.probe.is_some())
        .collect();
    // `version` names the release on disk; every release has it, so an
    // older probe says what it is too.
    let answers = transport::each(&machines, machines.len(), |machine| {
        let probe = machine.probe.as_ref().expect("filtered");
        let words = [
            "sh".to_owned(),
            "-c".to_owned(),
            r#""$1/grove-probe" status --dir "$1/data" && "$1/grove-probe" version"#.to_owned(),
            "sh".to_owned(),
            probe.dir.clone(),
        ];
        let (mut command, program) = transport::command_on(machine, &store.home, &words);
        transport::run(&mut command, program, None, Duration::from_secs(15))
    });
    let mut records = Vec::new();
    let mut rows = Vec::new();
    let ui = &context.ui;
    for (machine, ran) in machines.iter().zip(&answers) {
        let probe = machine.probe.as_ref().expect("filtered");
        let value = |key: &str| {
            ran.stdout
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{key}=")))
                .and_then(|value| value.trim().parse::<i64>().ok())
        };
        let running = ran.ok();
        let version = ran.stdout.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next() == Some("grove-probe"))
                .then(|| words.next().map(str::to_owned))
                .flatten()
        });
        let latest = store.latest_live(&machine.name)?.map(|(at, _)| at);
        records.push(json!({
            "clock_offset_ms": probe.clock_offset_ms,
            "cpu_s": value("probe_cpu_us").map(|us| num(us as f64 / 1e6)),
            "dir": probe.dir,
            "error": (!running).then(|| ran.failure()),
            "last_seq": value("last_seq"),
            "latest_reading_at": latest.map(iso_ms),
            "name": machine.name,
            "read_seq": probe.seq,
            "rss_kb": value("probe_rss_kb"),
            "running": running,
            "version": version,
        }));
        rows.push(vec![
            if running {
                ui.success(ui.symbols.success)
            } else {
                ui.danger(ui.symbols.error)
            },
            machine.name.clone(),
            probe.dir.clone(),
            version.clone().unwrap_or_else(|| "-".into()),
            value("last_seq").map_or("-".into(), |seq| seq.to_string()),
            probe.seq.to_string(),
        ]);
    }
    let human = if rows.is_empty() {
        format!(
            "{}\nInstall one with {}.",
            ui.muted("No probes installed."),
            ui.command(&format!("{NAME} probe install <name>"))
        )
    } else {
        ui.table(&["", "Name", "Folder", "Version", "Newest", "Read"], &rows)
    };
    Ok(Done::new(Value::Array(records), human))
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn request_stop(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

pub fn stream(context: &Context) -> Result<Done, AppError> {
    let limit = string(&context.options, "for")
        .map(|text| duration(&text))
        .transpose()?;
    let store = open_store()?;
    let machines: Vec<Machine> = targets(&store, context.argument(0).as_deref())?
        .into_iter()
        .filter(|machine| machine.probe.is_some())
        .collect();
    if machines.is_empty() {
        return Err(
            AppError::new("no_probes", "No machine to stream from has a probe.")
                .hint(format!("Install one with '{NAME} probe install <name>'.")),
        );
    }
    // SAFETY: the handlers only set an atomic flag.
    unsafe {
        libc::signal(
            libc::SIGINT,
            request_stop as *const () as libc::sighandler_t,
        );
        libc::signal(
            libc::SIGTERM,
            request_stop as *const () as libc::sighandler_t,
        );
    }
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                if STOP.load(Ordering::SeqCst) {
                    stop.store(true, Ordering::SeqCst);
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        })
    };
    let started = Instant::now();
    let until = limit.map(|ms| started + Duration::from_millis(ms as u64));
    let tallies = crate::probe::collect(&store, &machines, until, &stop, &mut |report| {
        context.event(report.kind, report.data);
    })?;
    stop.store(true, Ordering::SeqCst);
    let _ = watcher.join();
    let (cpu_ms, rss_kb) = own_usage();
    let ui = &context.ui;
    let mut rows = Vec::new();
    let records: Vec<Value> = machines
        .iter()
        .zip(&tallies)
        .map(|(machine, tally)| {
            let mut latencies = tally.latencies_ms.clone();
            latencies.sort_unstable();
            let at = |share: f64| {
                latencies
                    .get(((latencies.len() as f64 - 1.0) * share).round() as usize)
                    .copied()
            };
            rows.push(vec![
                machine.name.clone(),
                tally.readings.to_string(),
                tally.connects.to_string(),
                at(0.5).map_or("-".into(), |ms| format!("{ms}ms")),
            ]);
            json!({
                "bytes": tally.bytes,
                "connects": tally.connects,
                "last_error": tally.last_error,
                "latency_ms": { "max": latencies.last(), "median": at(0.5), "p95": at(0.95) },
                "name": machine.name,
                "readings": tally.readings,
            })
        })
        .collect();
    let elapsed = started.elapsed().as_millis() as u64;
    Ok(Done::new(
        json!({
            "collector": { "cpu_ms": cpu_ms, "max_rss_kb": rss_kb },
            "duration_ms": elapsed,
            "ended_at": iso_ms(now_ms()),
            "machines": records,
        }),
        ui.table(&["Name", "Readings", "Connects", "Latency"], &rows),
    ))
}

/// This process's CPU time in milliseconds and its peak resident memory.
fn own_usage() -> (u64, u64) {
    // SAFETY: getrusage fills the struct it is given.
    let usage = unsafe {
        let mut usage: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut usage);
        usage
    };
    let cpu_us = (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as u64 * 1_000_000
        + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as u64;
    let peak = usage.ru_maxrss as u64;
    (
        cpu_us / 1000,
        if cfg!(target_os = "macos") {
            peak / 1024
        } else {
            peak
        },
    )
}
