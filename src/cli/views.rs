//! Looking at machines: graph charts stored samples, ps lists the busiest
//! processes, top attaches an interactive top, and watch is a live
//! full-screen board that keeps one sample per machine per minute.

use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::machines::{open_store, require, targets};
use super::options::{bounded, duration, string};
use super::readings::{human_bytes, round};
use super::{Context, Done, NAME};
use crate::charts::{braille, bucketize, sparkline, summarize};
use crate::errors::{AppError, exit};
use crate::output::{iso_ms, now_ms, num, opt_num};
use crate::policy::human_duration;
use crate::reading::{Sample, rate};
use crate::{alerts, transport};

/// A charted metric: its key, label, unit, fixed top, and how its points
/// are read from samples. A reading a machine didn't report adds no point.
struct Metric {
    key: &'static str,
    label: &'static str,
    unit: &'static str,
    top: Option<f64>,
    choice: &'static str,
    series: fn(&[Sample]) -> Vec<(i64, f64)>,
    format: fn(f64) -> String,
}

fn readings(samples: &[Sample], read: impl Fn(&Sample) -> Option<f64>) -> Vec<(i64, f64)> {
    samples
        .iter()
        .filter_map(|sample| read(sample).map(|value| (sample.taken_at, value)))
        .collect()
}

/// Rates between consecutive samples, at the newer one's time.
fn rates(samples: &[Sample], counter: fn(&Sample) -> f64) -> Vec<(i64, f64)> {
    samples
        .windows(2)
        .filter_map(|pair| {
            let seconds = (pair[1].taken_at - pair[0].taken_at) as f64 / 1000.0;
            let delta = counter(&pair[1]) - counter(&pair[0]);
            (delta >= 0.0 && seconds > 0.0).then(|| (pair[1].taken_at, delta / seconds))
        })
        .collect()
}

fn pct(value: f64) -> String {
    format!("{value:.1}%")
}

fn temp(value: f64) -> String {
    format!("{value:.1}°C")
}

fn per_second(value: f64) -> String {
    format!("{}/s", human_bytes(value))
}

const METRICS: [Metric; 11] = [
    Metric {
        key: "cpu_pct",
        label: "CPU",
        unit: "%",
        top: Some(100.0),
        choice: "cpu",
        series: |s| readings(s, |x| Some(x.cpu_pct)),
        format: pct,
    },
    Metric {
        key: "mem_used_pct",
        label: "Memory",
        unit: "%",
        top: Some(100.0),
        choice: "mem",
        series: |s| readings(s, |x| alerts::metric_value(x, "mem")),
        format: pct,
    },
    Metric {
        key: "disk_used_pct",
        label: "Disk",
        unit: "%",
        top: Some(100.0),
        choice: "disk",
        series: |s| readings(s, |x| alerts::metric_value(x, "disk")),
        format: pct,
    },
    Metric {
        key: "load1",
        label: "Load 1m",
        unit: "",
        top: None,
        choice: "load",
        series: |s| readings(s, |x| Some(x.load1)),
        format: |v| format!("{v:.2}"),
    },
    Metric {
        key: "swap_used_pct",
        label: "Swap",
        unit: "%",
        top: Some(100.0),
        choice: "swap",
        series: |s| readings(s, |x| alerts::metric_value(x, "swap")),
        format: pct,
    },
    Metric {
        key: "cpu_temp_c",
        label: "CPU temp",
        unit: "°C",
        top: None,
        choice: "temp",
        series: |s| readings(s, |x| x.cpu_temp_c),
        format: temp,
    },
    Metric {
        key: "gpu_temp_c",
        label: "GPU temp",
        unit: "°C",
        top: None,
        choice: "temp",
        series: |s| readings(s, |x| x.gpu_temp_c),
        format: temp,
    },
    Metric {
        key: "battery_pct",
        label: "Battery",
        unit: "%",
        top: Some(100.0),
        choice: "battery",
        series: |s| readings(s, |x| x.battery_pct),
        format: pct,
    },
    Metric {
        key: "agent_sessions",
        label: "Agents",
        unit: "",
        top: None,
        choice: "agents",
        series: |s| readings(s, |x| x.agent_sessions),
        format: |v| format!("{v:.0}"),
    },
    Metric {
        key: "net_rx_bps",
        label: "Net RX",
        unit: "B/s",
        top: None,
        choice: "net",
        series: |s| rates(s, |x| x.net_rx_bytes),
        format: per_second,
    },
    Metric {
        key: "net_tx_bps",
        label: "Net TX",
        unit: "B/s",
        top: None,
        choice: "net",
        series: |s| rates(s, |x| x.net_tx_bytes),
        format: per_second,
    },
];

pub fn graph(context: &Context) -> Result<Done, AppError> {
    let width = bounded(&context.options, "width", 10, 200)? as usize;
    let since_text = string(&context.options, "since").unwrap_or_else(|| "24h".into());
    let choice = string(&context.options, "metric");
    let name = context.argument(0).unwrap_or_default();
    let store = open_store()?;
    require(&store, &name)?;
    let window = duration(&since_text)?;
    let until = now_ms();
    let since = until - window;
    // Within the last hour, a streamed machine charts at full resolution.
    let mut samples = Vec::new();
    if window <= crate::store::LIVE_WINDOW_MS {
        let facts = crate::probe::ProbeFacts::default();
        samples = store
            .live_since(&name, since)?
            .iter()
            .filter_map(|(taken_at, bytes)| {
                let record = grove_probe::Record::decode(bytes)?;
                let offset = record.taken_at_ms - taken_at;
                crate::probe::sample_from(&record, &name, &facts, offset)
            })
            .collect();
    }
    if samples.is_empty() {
        samples = store.samples_since(&name, since as f64)?;
    }
    let chosen: Vec<&Metric> = METRICS
        .iter()
        .filter(|metric| {
            choice
                .as_deref()
                .is_none_or(|choice| metric.choice == choice)
        })
        .collect();
    let mut panels = Vec::new();
    let records: Vec<Value> = chosen
        .iter()
        .map(|metric| {
            let points = bucketize(&(metric.series)(&samples), since, until, width);
            let summary = summarize(&points);
            panels.push((
                metric,
                points.clone(),
                summary.current,
                summary.avg,
                summary.max,
            ));
            json!({
                "avg": opt_num(summary.avg),
                "current": opt_num(summary.current),
                "max": opt_num(summary.max),
                "metric": metric.key,
                "min": opt_num(summary.min),
                "points": points.iter().map(|point| opt_num(*point)).collect::<Vec<_>>(),
                "unit": metric.unit,
            })
        })
        .collect();
    let ui = &context.ui;
    let human = if samples.is_empty() {
        format!(
            "{}\nTake some with {} or keep {} running.",
            ui.muted("No samples in the window."),
            ui.command(&format!("{NAME} sample {name}")),
            ui.command(&format!("{NAME} watch"))
        )
    } else {
        let label_width = chosen
            .iter()
            .map(|metric| metric.label.len())
            .max()
            .unwrap_or(0);
        let mut lines = vec![
            format!(
                "{} {}",
                ui.heading(&name),
                ui.muted(&format!(
                    "{} to {} UTC, {} samples",
                    iso_ms(since).replace('T', " ")[..16].to_owned(),
                    iso_ms(until).replace('T', " ")[..16].to_owned(),
                    samples.len()
                ))
            ),
            String::new(),
        ];
        // A panel with nothing in it is a machine without that sensor;
        // asking for the metric by name still shows it.
        for (metric, points, current, avg, max) in panels {
            if choice.is_none() && current.is_none() {
                continue;
            }
            let legend = match current {
                None => ui.muted("no data"),
                Some(current) => ui.muted(&format!(
                    "cur {}  avg {}  max {}",
                    (metric.format)(current),
                    (metric.format)(avg.unwrap_or_default()),
                    (metric.format)(max.unwrap_or_default())
                )),
            };
            lines.push(format!(
                "{:label_width$}  {}  {legend}",
                metric.label,
                sparkline(&points, ui.unicode, metric.top)
            ));
        }
        lines.join("\n")
    };
    Ok(Done::new(
        json!({
            "bucket_ms": (window as f64 / width as f64).round() as i64,
            "machine": name,
            "metrics": records,
            "samples": samples.len(),
            "since": iso_ms(since),
            "until": iso_ms(until),
        }),
        human,
    ))
}

/// `ps` lines as pid, CPU, memory and the command name; only the name
/// leaves the machine, never its arguments.
fn parse_processes(stdout: &str) -> Vec<(i64, f64, f64, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let pid = words.next()?.parse().ok()?;
            let cpu: f64 = words.next()?.parse().ok()?;
            let mem: f64 = words
                .next()
                .and_then(|word| word.parse().ok())
                .unwrap_or(f64::NAN);
            Some((pid, cpu, mem, words.collect::<Vec<_>>().join(" ")))
        })
        .collect()
}

pub fn ps(context: &Context) -> Result<Done, AppError> {
    let limit = bounded(&context.options, "limit", 1, 500)? as usize;
    let timeout = bounded(&context.options, "timeout", 1, 120_000)?;
    let name = context.argument(0).unwrap_or_default();
    let store = open_store()?;
    let machine = require(&store, &name)?;
    let ran = transport::run_script(
        &machine,
        &store.home,
        "ps ax -o pid=,pcpu=,pmem=,comm=\n",
        Duration::from_millis(timeout as u64),
    );
    if !ran.ok() {
        let detail = ran.failure();
        return Err(AppError::new(
            "remote_command_failed",
            format!("Could not read processes from \"{name}\": {detail}."),
        )
        .hint(format!("Check reachability with '{NAME} status {name}'.")));
    }
    let mut processes = parse_processes(&ran.stdout);
    processes.sort_by(|a, b| b.1.total_cmp(&a.1));
    processes.truncate(limit);
    let ui = &context.ui;
    let human = if processes.is_empty() {
        ui.muted("No processes reported.")
    } else {
        let rows: Vec<Vec<String>> = processes
            .iter()
            .map(|(pid, cpu, mem, command)| {
                vec![
                    pid.to_string(),
                    format!("{}%", num(*cpu)),
                    format!("{}%", num(*mem)),
                    command.clone(),
                ]
            })
            .collect();
        ui.table(&["PID", "CPU", "Mem", "Command"], &rows)
    };
    let records: Vec<Value> = processes
        .iter()
        .map(|(pid, cpu, mem, command)| json!({ "command": command, "cpu_pct": num(*cpu), "mem_pct": num(*mem), "pid": pid }))
        .collect();
    Ok(Done::new(
        json!({ "checked_at": iso_ms(now_ms()), "machine": name, "processes": records }),
        human,
    ))
}

fn interactive_required(command: &str, hint: String) -> AppError {
    AppError::usage(
        "interactive_required",
        format!("{command} needs an interactive terminal."),
    )
    .hint(hint)
}

pub fn top(context: &Context) -> Result<Done, AppError> {
    let name = context.argument(0).unwrap_or_default();
    let store = open_store()?;
    let machine = require(&store, &name)?;
    if !context.interactive {
        return Err(interactive_required(
            "top",
            format!("Use '{NAME} ps {name}' for a headless snapshot."),
        ));
    }
    let mut command = if transport::is_local(&machine.endpoint) {
        Command::new("top")
    } else {
        let prefix = transport::ssh_prefix();
        let mut command = Command::new(&prefix[0]);
        let mut options = transport::ssh_options(&machine, &store.home);
        // A real terminal for top, not the -T scripts run with.
        options.retain(|option| option != "-T");
        command
            .args(&prefix[1..])
            .args(options)
            .arg("-t")
            .arg("--")
            .arg(machine.endpoint.trim())
            .arg("top");
        command
    };
    let status = command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|error| {
            AppError::new(
                "remote_command_failed",
                format!("Could not start top: {error}."),
            )
        })?;
    let code = status.code().unwrap_or(-1);
    let mut done = Done::new(json!({ "exit_code": code, "machine": name }), String::new());
    done.silent = true;
    if code != 0 {
        done.outcome.exit_code = exit::ERROR;
    }
    Ok(done)
}

/// The terminal in raw-ish mode for the board's lifetime: no echo, no line
/// buffering, Ctrl+C read as a key so the screen is always restored.
struct RawTerminal {
    saved: libc::termios,
}

impl RawTerminal {
    fn enter() -> Option<Self> {
        // SAFETY: tcgetattr/tcsetattr read and write the struct given, on
        // stdin, which the caller checked is a terminal.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut saved) != 0 {
                return None;
            }
            let mut raw = saved;
            raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
            raw.c_cc[libc::VMIN] = 0;
            raw.c_cc[libc::VTIME] = 0;
            libc::tcsetattr(0, libc::TCSANOW, &raw);
            Some(Self { saved })
        }
    }

    /// Waits up to `wait` for a key; true when it was q, Q or Ctrl+C.
    fn quit_within(&self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let mut poll = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one pollfd, valid for the call.
            let ready =
                unsafe { libc::poll(&mut poll, 1, left.as_millis().min(i32::MAX as u128) as i32) };
            if ready <= 0 {
                return false;
            }
            let mut byte = [0_u8; 1];
            // SAFETY: reads at most one byte into the buffer.
            let read = unsafe { libc::read(0, byte.as_mut_ptr().cast(), 1) };
            if read == 1 && matches!(byte[0], b'q' | b'Q' | 3) {
                return true;
            }
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        // SAFETY: restores the settings read on entry.
        unsafe {
            libc::tcsetattr(0, libc::TCSANOW, &self.saved);
        }
    }
}

fn columns() -> usize {
    // SAFETY: TIOCGWINSZ fills the winsize struct given.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(1, libc::TIOCGWINSZ, &mut size) } == 0;
    if ok && size.ws_col > 0 {
        usize::from(size.ws_col)
    } else {
        100
    }
}

/// What the board remembers per machine between frames.
#[derive(Default)]
struct Lanes {
    cpu: Vec<Option<f64>>,
    mem: Vec<Option<f64>>,
    rx: Vec<Option<f64>>,
    last: Option<(i64, f64, f64)>,
}

fn push(series: &mut Vec<Option<f64>>, value: Option<f64>, cap: usize) {
    series.push(value);
    let excess = series.len().saturating_sub(cap);
    series.drain(..excess);
}

pub fn watch(context: &Context) -> Result<Done, AppError> {
    let interval = bounded(&context.options, "interval", 1, 3600)?;
    let timeout = bounded(&context.options, "timeout", 1, 120_000)?;
    if !context.interactive {
        return Err(interactive_required(
            "watch",
            format!("Run '{NAME} sample --json' on a schedule for headless collection."),
        ));
    }
    let store = open_store()?;
    let machines = targets(&store, context.argument(0).as_deref())?;
    if machines.is_empty() {
        return Err(AppError::new("no_machines", "No machines are registered.")
            .hint(format!("Add one with '{NAME} add <name> <endpoint>'.")));
    }
    let Some(terminal) = RawTerminal::enter() else {
        return Err(interactive_required(
            "watch",
            format!("Run '{NAME} sample' instead."),
        ));
    };
    let ui = &context.ui;
    let mut lanes: Vec<Lanes> = machines.iter().map(|_| Lanes::default()).collect();
    let (mut frames, mut stored) = (0, 0);
    let mut out = std::io::stdout();
    let _ = write!(out, "\x1b[?1049h\x1b[?25l");
    let result = (|| -> Result<(), AppError> {
        loop {
            // One sample per machine per minute is kept; the live stream
            // stays in memory.
            let (taken_at, outcomes) = round(
                &store,
                &machines,
                Duration::from_millis(timeout as u64),
                machines.len(),
                60_000,
            )?;
            frames += 1;
            let lane_width = columns().saturating_sub(60).clamp(14, 40);
            let cap = if ui.unicode {
                lane_width * 2
            } else {
                lane_width
            };
            let lane = |points: &[Option<f64>], top: Option<f64>| {
                let drawn = if ui.unicode {
                    braille(points, top)
                } else {
                    sparkline(points, false, top)
                };
                format!("{drawn:>lane_width$}")
            };
            let up = outcomes
                .iter()
                .filter(|outcome| outcome.taken.is_ok())
                .count();
            let mut lines = vec![
                format!(
                    "{} {}  {}  {}  {}",
                    ui.brand("◆"),
                    ui.heading(NAME),
                    ui.success(&format!("{up}/{} up", machines.len())),
                    ui.muted(&iso_ms(taken_at)[11..19]),
                    ui.muted(&format!("every {interval}s · q to quit"))
                ),
                String::new(),
            ];
            for ((machine, outcome), lanes) in machines.iter().zip(&outcomes).zip(&mut lanes) {
                let reading = match &outcome.taken {
                    Ok(taken) => {
                        stored += usize::from(taken.stored);
                        &taken.record
                    }
                    Err(error) => {
                        for series in [&mut lanes.cpu, &mut lanes.mem, &mut lanes.rx] {
                            push(series, None, cap);
                        }
                        lanes.last = None;
                        lines.push(format!(
                            "{} {}  {}",
                            ui.danger(ui.symbols.error),
                            ui.heading(&machine.name),
                            ui.muted(error)
                        ));
                        lines.push(String::new());
                        continue;
                    }
                };
                let number = |key: &str| reading[key].as_f64().unwrap_or_default();
                let rx = lanes.last.and_then(|(at, rx, _)| {
                    rate(number("net_rx_bytes"), rx, (taken_at - at) as f64 / 1000.0)
                });
                let tx = lanes.last.and_then(|(at, _, tx)| {
                    rate(number("net_tx_bytes"), tx, (taken_at - at) as f64 / 1000.0)
                });
                lanes.last = Some((taken_at, number("net_rx_bytes"), number("net_tx_bytes")));
                push(&mut lanes.cpu, Some(number("cpu_pct")), cap);
                push(&mut lanes.mem, Some(number("mem_used_pct")), cap);
                push(&mut lanes.rx, rx, cap);
                let uptime = reading["uptime_s"]
                    .as_f64()
                    .map_or(String::new(), |uptime| {
                        format!(" · up {}", human_duration(uptime))
                    });
                lines.push(format!(
                    "{} {}  {}",
                    ui.success(ui.symbols.success),
                    ui.heading(&machine.name),
                    ui.muted(&format!(
                        "{} {} · {} cores{uptime} · {}",
                        reading["os"].as_str().unwrap_or_default(),
                        reading["arch"].as_str().unwrap_or_default(),
                        reading["cores"],
                        super::readings::health_cell(&reading["health"], context)
                    ))
                ));
                lines.push(format!(
                    "  CPU  {} {}  load {:.2} {:.2} {:.2}",
                    lane(&lanes.cpu, Some(100.0)),
                    ui.meter(number("cpu_pct"), 10),
                    number("load1"),
                    number("load5"),
                    number("load15")
                ));
                lines.push(format!(
                    "  Mem  {} {}  disk {}",
                    lane(&lanes.mem, Some(100.0)),
                    ui.meter(number("mem_used_pct"), 10),
                    ui.meter(number("disk_used_pct"), 10)
                ));
                let rate_text = |rate: Option<f64>| rate.map_or("-".into(), per_second);
                lines.push(format!(
                    "  Net  {} rx {}  tx {}",
                    lane(&lanes.rx, None),
                    rate_text(rx),
                    rate_text(tx)
                ));
                lines.push(String::new());
            }
            let frame = lines.join("\r\n");
            let _ = write!(out, "\x1b[H\x1b[2J{frame}");
            let _ = out.flush();
            if terminal.quit_within(Duration::from_secs(interval as u64)) {
                return Ok(());
            }
        }
    })();
    let _ = write!(out, "\x1b[?25h\x1b[?1049l");
    let _ = out.flush();
    drop(terminal);
    result?;
    let mut done = Done::new(
        json!({ "frames": frames, "machines": machines.len(), "samples_stored": stored }),
        String::new(),
    );
    done.silent = true;
    Ok(done)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ps -o pid=,pcpu=,pmem=,comm=` lines: a name
    /// with spaces stays whole, a line with no number is dropped.
    #[test]
    fn process_lines_keep_whole_names() {
        let output = "  412  12.5  1.2 /Applications/Code Helper (Renderer)\n    1   0.0  0.1 launchd\nbad line\n";
        assert_eq!(
            parse_processes(output),
            [
                (
                    412,
                    12.5,
                    1.2,
                    "/Applications/Code Helper (Renderer)".to_owned()
                ),
                (1, 0.0, 0.1, "launchd".to_owned())
            ]
        );
    }
}
