//! The reading commands: sample takes readings, and the records and
//! renderings they share.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::machines::{open_store, targets};
use super::options::{bounded, duration, string};
use super::{Context, Done, NAME};
use crate::alerts::Event;
use crate::errors::AppError;
use crate::output::{iso_ms, now_ms, num};
use crate::policy::human_duration;
use crate::reading::{STALE_AFTER_MS, Sample, cpu_between, parse, rate};
use crate::store::{Latest, Machine, Store};
use crate::{ping, script, transport};

const KIB: f64 = 1024.0;

/// "1.5GiB": binary units, one decimal past bytes.
pub fn human_bytes(bytes: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes;
    let mut unit = 0;
    while value >= KIB && unit < units.len() - 1 {
        value /= KIB;
        unit += 1;
    }
    if unit == 0 {
        format!("{}B", value.round())
    } else {
        format!("{value:.1}{}", units[unit])
    }
}

/// What one machine's run brought back.
struct Collected {
    source: Source,
    ping: ping::Ping,
    elapsed: Duration,
}

/// Where a machine's reading came from.
enum Source {
    /// The sample script, where no probe is installed (or it didn't answer).
    Script(transport::Ran),
    /// What the machine's probe printed for `read --last 1`.
    Probe(Vec<u8>),
    /// A stream already holds a reading under two intervals old.
    Live(Value),
}

/// How fresh a streamed reading must be for `sample` to answer with it.
pub const LIVE_FRESH_MS: i64 = 2 * grove_probe::INTERVAL_MS as i64 + 1000;

/// Reads every machine: the latest streamed reading when one is fresh,
/// else its probe's newest reading, else the sample script; remote
/// machines are pinged at the same time.
fn collect(
    machines: &[Machine],
    store: &Store,
    timeout: Duration,
    concurrency: usize,
) -> Result<Vec<Collected>, AppError> {
    let home = store.home.clone();
    let now = now_ms();
    let mut live = Vec::new();
    for machine in machines {
        let fresh = match &machine.probe {
            Some(_) => store
                .latest(&machine.name)?
                .filter(|latest| now - latest.taken_at <= LIVE_FRESH_MS)
                .map(|latest| latest.reading),
            None => None,
        };
        live.push(fresh);
    }
    let jobs: Vec<(&Machine, Option<Value>)> = machines.iter().zip(live).collect();
    Ok(transport::each(&jobs, concurrency, |(machine, live)| {
        let started = Instant::now();
        if let Some(reading) = live {
            return Collected {
                source: Source::Live(reading.clone()),
                ping: ping::Ping::default(),
                elapsed: started.elapsed(),
            };
        }
        std::thread::scope(|scope| {
            let pinging = (!transport::is_local(&machine.endpoint))
                .then(|| scope.spawn(|| ping::probe(machine)));
            let source = match crate::probe::read_latest(machine, &home, timeout) {
                Ok(bytes) if machine.probe.is_some() => Source::Probe(bytes),
                _ => Source::Script(transport::run_script(
                    machine,
                    &home,
                    script::SAMPLE_SCRIPT,
                    timeout,
                )),
            };
            let ping = pinging
                .and_then(|pinging| pinging.join().ok())
                .unwrap_or_default();
            Collected {
                source,
                ping,
                elapsed: started.elapsed(),
            }
        })
    }))
}

/// A probe's `read` output as a parsed reading.
fn parse_probe(bytes: &[u8], machine: &Machine) -> Result<crate::reading::Parsed, String> {
    let (_, facts, record) = crate::probe::decode(bytes)?;
    let offset = machine
        .probe
        .as_ref()
        .map_or(0, |probe| probe.clock_offset_ms);
    let facts = crate::probe::parse_facts(&facts.unwrap_or_default(), offset);
    let record = record.ok_or("The probe has no reading yet.")?;
    let sample = crate::probe::sample_from(&record, &machine.name, &facts, offset)
        .ok_or("The probe has no reading yet.")?;
    Ok(crate::reading::Parsed {
        sample,
        facts: facts.facts,
        config: facts.config,
        jiffies: None,
    })
}

/// A reading's record, and whether it was stored.
pub struct Taken {
    pub record: Value,
    pub stored: bool,
}

/// Records one machine's run: contact, facts, config, the last reading,
/// the sample itself when it is due, and the alert transitions they
/// cause, all in one write. Transitions are pushed onto `events`.
fn record_run(
    store: &Store,
    machine: &Machine,
    collected: &Collected,
    taken_at: i64,
    min_store_ms: i64,
    retention: Option<i64>,
    events: &mut Vec<Event>,
) -> Result<Result<Taken, String>, AppError> {
    let parsed = match &collected.source {
        // The stream stores and alerts on its own readings.
        Source::Live(reading) => {
            return Ok(Ok(Taken {
                record: reading.clone(),
                stored: false,
            }));
        }
        Source::Probe(bytes) => parse_probe(bytes, machine),
        Source::Script(ran) if ran.ok() => parse(&ran.stdout, &machine.name, taken_at)
            .map_err(|key| format!("The machine returned an unreadable sample ({key}).")),
        Source::Script(ran) => Err(ran.failure()),
    };
    store.write(|store| {
        events.extend(store.record_contact(&machine.name, taken_at, parsed.is_ok())?);
        let parsed = match parsed {
            Ok(parsed) => parsed,
            Err(error) => return Ok(Err(error)),
        };
        let previous = store.latest(&machine.name)?;
        let mut sample: Sample = parsed.sample;
        // Two /proc/stat readings close together give the real busy share
        // between them; otherwise the process-table estimate stands.
        let fresh = previous
            .as_ref()
            .filter(|previous| taken_at - previous.taken_at <= STALE_AFTER_MS);
        if let Some(cpu) = fresh
            .and_then(|previous| previous.jiffies)
            .zip(parsed.jiffies)
            .and_then(|(older, newer)| cpu_between(older, newer))
        {
            sample.cpu_pct = cpu;
        }
        sample.latency_ms = collected.ping.latency_ms;
        sample.ping_target.clone_from(&collected.ping.target);
        sample.address.clone_from(&collected.ping.address);
        let stored = min_store_ms == 0
            || store
                .last_sample_at(&machine.name)?
                .is_none_or(|last| taken_at - last >= min_store_ms);
        if stored {
            store.insert_sample(&sample)?;
            // Alerts read stored samples only, so only a stored reading
            // can move them.
            events.extend(store.evaluate_thresholds(&sample)?);
            if let Some(retention) = retention {
                store.prune(Some(&machine.name), taken_at - retention)?;
            }
        }
        let seconds = previous.as_ref().map_or(0.0, |previous| {
            (taken_at - previous.taken_at) as f64 / 1000.0
        });
        let mut record = sample.record();
        record["net_rx_bps"] = crate::output::opt_num(
            previous
                .as_ref()
                .and_then(|previous| rate(sample.net_rx_bytes, previous.net_rx_bytes, seconds)),
        );
        record["net_tx_bps"] = crate::output::opt_num(
            previous
                .as_ref()
                .and_then(|previous| rate(sample.net_tx_bytes, previous.net_tx_bytes, seconds)),
        );
        store.set_latest(
            &machine.name,
            &Latest {
                taken_at,
                reading: record.clone(),
                net_rx_bytes: sample.net_rx_bytes,
                net_tx_bytes: sample.net_tx_bytes,
                jiffies: parsed.jiffies,
            },
        )?;
        store.update_facts(&machine.name, &parsed.facts, taken_at)?;
        store.update_config(&machine.name, &parsed.config)?;
        Ok(Ok(Taken { record, stored }))
    })
}

/// One machine's part of a round.
pub struct Outcome {
    pub elapsed: Duration,
    pub taken: Result<Taken, String>,
}

/// One sampling round: every machine read at once, each recorded in its
/// own write, the fleet warning judged once all have landed, and hooks
/// run last, where they can't change the outcome.
pub fn round(
    store: &Store,
    machines: &[Machine],
    timeout: Duration,
    concurrency: usize,
    min_store_ms: i64,
) -> Result<(i64, Vec<Outcome>), AppError> {
    let taken_at = now_ms();
    let collected = collect(machines, store, timeout, concurrency)?;
    let retention = store.retention()?;
    let mut events = Vec::new();
    let mut outcomes = Vec::new();
    for (machine, collected) in machines.iter().zip(&collected) {
        let taken = record_run(
            store,
            machine,
            collected,
            taken_at,
            min_store_ms,
            retention,
            &mut events,
        )?;
        outcomes.push(Outcome {
            elapsed: collected.elapsed,
            taken,
        });
    }
    events.extend(store.write(|store| store.evaluate_fleet_sessions(taken_at))?);
    let _ = crate::hooks::run_for_events(store, &events);
    Ok((taken_at, outcomes))
}

pub fn sample(context: &Context) -> Result<Done, AppError> {
    let timeout = bounded(&context.options, "timeout", 1, 120_000)?;
    let concurrency = bounded(&context.options, "concurrency", 1, 64)?;
    let min_store_ms = match string(&context.options, "minStoreInterval") {
        Some(text) => duration(&text)?,
        None => 0,
    };
    let store = open_store()?;
    let machines = targets(&store, context.argument(0).as_deref())?;
    let (taken_at, outcomes) = round(
        &store,
        &machines,
        Duration::from_millis(timeout as u64),
        concurrency as usize,
        min_store_ms,
    )?;
    let mut entries = Vec::new();
    let mut rows = Vec::new();
    let (mut failed, mut stored) = (0, 0);
    let ui = &context.ui;
    for (machine, outcome) in machines.iter().zip(outcomes) {
        let duration_ms = num(outcome.elapsed.as_millis() as f64);
        match outcome.taken {
            Ok(taken) => {
                stored += usize::from(taken.stored);
                let record = &taken.record;
                let pct = |key: &str| record[key].as_f64().unwrap_or_default();
                rows.push(vec![
                    ui.success(ui.symbols.success),
                    machine.name.clone(),
                    ui.meter(pct("cpu_pct"), 10),
                    format!("{:.2}", pct("load1")),
                    ui.meter(pct("mem_used_pct"), 10),
                    ui.meter(pct("disk_used_pct"), 10),
                    format!(
                        "{}/{}",
                        human_bytes(pct("net_rx_bytes")),
                        human_bytes(pct("net_tx_bytes"))
                    ),
                    health_cell(&record["health"], context),
                ]);
                entries.push(json!({
                    "duration_ms": duration_ms,
                    "error": null,
                    "name": machine.name,
                    "ok": true,
                    "sample": taken.record,
                    "stored": taken.stored,
                }));
            }
            Err(error) => {
                failed += 1;
                rows.push(vec![
                    ui.danger(ui.symbols.error),
                    machine.name.clone(),
                    ui.muted(&error),
                ]);
                entries.push(json!({
                    "duration_ms": duration_ms,
                    "error": error,
                    "name": machine.name,
                    "ok": false,
                    "sample": null,
                    "stored": false,
                }));
            }
        }
    }
    let data = json!({
        "checked_at": iso_ms(taken_at),
        "failed": failed,
        "machines": entries,
        "stored": stored,
    });
    let human = if machines.is_empty() {
        format!(
            "{}\nAdd one with {}.",
            ui.muted("No machines to sample."),
            ui.command(&format!("{NAME} add <name> <endpoint>"))
        )
    } else {
        ui.table(
            &[
                "",
                "Name",
                "CPU",
                "Load",
                "Mem",
                "Disk",
                "RX/TX total",
                "Health",
            ],
            &rows,
        )
    };
    Ok(Done::new(data, human))
}

/// "79 degraded by disk": the score, its band, and why.
pub fn health_cell(health: &Value, context: &Context) -> String {
    let ui = &context.ui;
    let status = health["status"].as_str().unwrap_or("pending");
    let painted = match status {
        "healthy" => ui.success(status),
        "degraded" => ui.warning(status),
        "pending" => ui.muted(status),
        _ => ui.danger(status),
    };
    let score = health["score"]
        .as_u64()
        .map_or(String::new(), |score| format!("{score} "));
    match health["reason"]["metric"].as_str() {
        Some(metric) => format!("{score}{painted} ({metric})"),
        None => format!("{score}{painted}"),
    }
}

/// The health `status` reports: the last reading's when it is fresh,
/// unreachable when the machine didn't answer, else pending.
pub fn current_health(
    store: &Store,
    machine: &Machine,
    reachable: bool,
    now: i64,
) -> Result<Value, AppError> {
    if !reachable {
        return Ok(json!({ "score": null, "status": "unreachable", "reason": null }));
    }
    let latest = store.latest(&machine.name)?;
    Ok(match latest {
        Some(latest) if now - latest.taken_at <= STALE_AFTER_MS => latest.reading["health"].clone(),
        _ => json!({ "score": null, "status": "pending", "reason": null }),
    })
}

/// The round trip a live stream measured, from its latest reading while
/// that is fresh: timed over the stream's own connection, so it holds for
/// machines behind a jump host too.
pub fn streamed_round_trip(
    store: &Store,
    machine: &Machine,
    now: i64,
) -> Result<Option<f64>, AppError> {
    if machine.probe.is_none() {
        return Ok(None);
    }
    Ok(store
        .latest(&machine.name)?
        .filter(|latest| now - latest.taken_at <= LIVE_FRESH_MS)
        .and_then(|latest| latest.reading["latency_ms"].as_f64()))
}

/// CPU and GPU temperatures share one cell.
fn temperature(record: &Value) -> Option<String> {
    let parts: Vec<String> = [("cpu", "cpu_temp_c"), ("gpu", "gpu_temp_c")]
        .into_iter()
        .filter(|(_, key)| !record[key].is_null())
        .map(|(label, key)| format!("{label} {}°C", record[key]))
        .collect();
    (!parts.is_empty()).then(|| parts.join("  "))
}

fn show_machine(machine: &Machine) -> Value {
    let facts = &machine.facts;
    json!({
        "added_at": iso_ms(machine.added_at),
        "chip": facts.chip,
        "config": super::machines::config_record(machine),
        "endpoint": machine.endpoint,
        "gpu": facts.gpu,
        "gpu_mem_total_mb": crate::output::opt_num(facts.gpu_mem_total_mb),
        "ip": facts.ip,
        "labels": super::machines::labels_record(&machine.labels),
        "model": facts.model,
        "name": machine.name,
        "os_version": facts.os_version,
        "port": machine.port,
        "probe": super::machines::probe_record(machine),
        "product_name": facts.product_name,
    })
}

pub fn show(context: &Context) -> Result<Done, AppError> {
    let name = context.argument(0).unwrap_or_default();
    let store = open_store()?;
    let machine = super::machines::require(&store, &name)?;
    let summary = store.sample_summary(&name)?;
    // The last reading, stored or not; a machine whose readings were
    // only ever stored has just those until the next one is taken.
    let latest = match store.latest(&name)? {
        Some(latest) => latest.reading,
        None => store
            .list_samples(&name, 1)?
            .first()
            .map_or(Value::Null, Sample::record),
    };
    let data = json!({
        "history": {
            "count": summary.count,
            "first_at": crate::output::opt_iso(summary.first_at),
            "last_at": crate::output::opt_iso(summary.last_at),
        },
        "latest": latest,
        "machine": show_machine(&machine),
    });
    let human = render_show(&data, &machine, context);
    Ok(Done::new(data, human))
}

fn render_show(data: &Value, machine: &Machine, context: &Context) -> String {
    let ui = &context.ui;
    let mut lines = vec![format!(
        "{} {}",
        ui.heading(&machine.name),
        ui.muted(&format!("{}:{}", machine.endpoint, machine.port))
    )];
    let sample = &data["latest"];
    if sample.is_null() {
        lines.push(String::new());
        lines.push(ui.muted("No samples recorded."));
        lines.push(format!(
            "Take one with {}.",
            ui.command(&format!("{NAME} sample {}", machine.name))
        ));
        return lines.join("\n");
    }
    let number = |key: &str| sample[key].as_f64().unwrap_or_default();
    let facts = &machine.facts;
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row = |field: &str, value: String| rows.push(vec![field.to_owned(), value]);
    let os_version = facts
        .os_version
        .as_deref()
        .map_or(String::new(), |version| format!(" {version}"));
    row(
        "Host",
        format!(
            "{} ({}{os_version} {})",
            sample["hostname"].as_str().unwrap_or_default(),
            sample["os"].as_str().unwrap_or_default(),
            sample["arch"].as_str().unwrap_or_default()
        ),
    );
    let hardware: Vec<&str> = [&facts.product_name, &facts.model, &facts.chip]
        .into_iter()
        .filter_map(|fact| fact.as_deref())
        .collect();
    if !hardware.is_empty() {
        row("Hardware", hardware.join(" · "));
    }
    if let Some(gpu) = &facts.gpu {
        row("GPU", gpu.clone());
    }
    if let Some(ip) = &facts.ip {
        row("Address", ip.clone());
    }
    let labels: Vec<String> = crate::store::LABEL_KEYS
        .iter()
        .zip(&machine.labels)
        .filter_map(|(key, value)| value.as_ref().map(|value| format!("{key} {value}")))
        .collect();
    if !labels.is_empty() {
        row("Labels", labels.join(" · "));
    }
    if machine.config.commit.is_some() {
        let age = machine.config.checked_at.map_or(String::new(), |at| {
            format!(
                ", read {} ago",
                human_duration((now_ms() - at) as f64 / 1000.0)
            )
        });
        let state = if machine.config.verify == Some(0) {
            "verified"
        } else {
            "modified"
        };
        row(
            "Config",
            format!(
                "{} {state}{age}",
                super::machines::config_cell(machine, context)
            ),
        );
    }
    row("Health", health_cell(&sample["health"], context));
    row("Cores", sample["cores"].to_string());
    row(
        "CPU",
        format!(
            "{} load {:.2} {:.2} {:.2}",
            ui.meter(number("cpu_pct"), 14),
            number("load1"),
            number("load5"),
            number("load15")
        ),
    );
    row(
        "Memory",
        format!(
            "{} {} of {}",
            ui.meter(number("mem_used_pct"), 14),
            human_bytes((number("mem_total_kb") - number("mem_available_kb")) * KIB),
            human_bytes(number("mem_total_kb") * KIB)
        ),
    );
    if let Some(swap) = sample["swap_used_pct"].as_f64() {
        row(
            "Swap",
            format!(
                "{} {} of {}",
                ui.meter(swap, 14),
                human_bytes(number("swap_used_kb") * KIB),
                human_bytes(number("swap_total_kb") * KIB)
            ),
        );
    }
    row(
        "Disk",
        format!(
            "{} {} of {}",
            ui.meter(number("disk_used_pct"), 14),
            human_bytes(number("disk_used_kb") * KIB),
            human_bytes(number("disk_total_kb") * KIB)
        ),
    );
    if let Some(temperature) = temperature(sample) {
        row("Temperature", temperature);
    }
    if let Some(gpu) = sample["gpu_util_pct"].as_f64() {
        row("GPU load", ui.meter(gpu, 14));
    }
    if let Some(battery) = sample["battery_pct"].as_f64() {
        let state = sample["battery_state"]
            .as_str()
            .map_or(String::new(), |state| format!(" {state}"));
        row("Battery", format!("{}%{state}", num(battery)));
    }
    if !sample["agent_sessions"].is_null() {
        let split = match (
            sample["agents"]["claude"].as_f64(),
            sample["agents"]["codex"].as_f64(),
        ) {
            (Some(claude), Some(codex)) => format!(" (claude {claude}, codex {codex})"),
            _ => String::new(),
        };
        row(
            "Agent sessions",
            format!("{}{split}", sample["agent_sessions"]),
        );
    }
    if let Some(latency) = sample["latency_ms"].as_f64() {
        row("Latency", format!("{}ms", num(latency)));
    }
    row(
        "Network",
        format!(
            "{} in / {} out since boot",
            human_bytes(number("net_rx_bytes")),
            human_bytes(number("net_tx_bytes"))
        ),
    );
    if let Some(uptime) = sample["uptime_s"].as_f64() {
        row("Uptime", human_duration(uptime));
    }
    row(
        "Sampled",
        sample["taken_at"].as_str().unwrap_or_default().to_owned(),
    );
    let history = &data["history"];
    let count = history["count"].as_i64().unwrap_or_default();
    let since = history["first_at"]
        .as_str()
        .map_or(String::new(), |first| format!(" since {first}"));
    lines.push(String::new());
    lines.push(ui.table(&["Field", "Value"], &rows));
    lines.push(String::new());
    lines.push(ui.muted(&format!(
        "{count} sample{} recorded{since}",
        if count == 1 { "" } else { "s" }
    )));
    lines.join("\n")
}

pub fn history(context: &Context) -> Result<Done, AppError> {
    let limit = bounded(&context.options, "limit", 1, 1000)?;
    let name = context.argument(0).unwrap_or_default();
    let store = open_store()?;
    super::machines::require(&store, &name)?;
    // One extra row lets the oldest shown sample still derive a rate.
    let rows = store.list_samples(&name, limit + 1)?;
    let samples: Vec<Value> = rows
        .iter()
        .take(limit as usize)
        .enumerate()
        .map(|(index, sample)| sample.record_with_rates(rows.get(index + 1)))
        .collect();
    let human = render_history(&samples, &name, context);
    Ok(Done::new(
        json!({ "machine": name, "samples": samples }),
        human,
    ))
}

fn render_history(samples: &[Value], name: &str, context: &Context) -> String {
    let ui = &context.ui;
    if samples.is_empty() {
        return format!(
            "{}\nTake one with {}.",
            ui.muted("No samples recorded."),
            ui.command(&format!("{NAME} sample {name}"))
        );
    }
    type Cell = fn(&Value) -> String;
    let percent = |value: &Value| format!("{value}%");
    // A column for a sensor nothing here reported is noise.
    let optional: [(&str, &str, Cell); 4] = [
        ("Swap", "swap_used_pct", |sample| {
            sample["swap_used_pct"]
                .as_f64()
                .map_or("-".into(), |_| format!("{}%", sample["swap_used_pct"]))
        }),
        ("Temp", "cpu_temp_c", |sample| {
            temperature(sample).unwrap_or_else(|| "-".into())
        }),
        ("Batt", "battery_pct", |sample| {
            sample["battery_pct"]
                .as_f64()
                .map_or("-".into(), |_| format!("{}%", sample["battery_pct"]))
        }),
        ("Agents", "agent_sessions", |sample| {
            sample["agent_sessions"]
                .as_f64()
                .map_or("-".into(), |_| sample["agent_sessions"].to_string())
        }),
    ];
    let shown: Vec<&(&str, &str, Cell)> = optional
        .iter()
        .filter(|(header, key, _)| {
            samples.iter().any(|sample| {
                !sample[*key].is_null() || (*header == "Temp" && !sample["gpu_temp_c"].is_null())
            })
        })
        .collect();
    let headers: Vec<&str> = ["Taken", "CPU", "Load", "Mem", "Disk"]
        .into_iter()
        .chain(shown.iter().map(|(header, _, _)| *header))
        .chain(["RX/s", "TX/s"])
        .collect();
    let rate = |value: &Value| {
        value
            .as_f64()
            .map_or("-".into(), |bps| format!("{}/s", human_bytes(bps)))
    };
    let rows: Vec<Vec<String>> = samples
        .iter()
        .map(|sample| {
            let taken = sample["taken_at"]
                .as_str()
                .unwrap_or_default()
                .replace('T', " ");
            [
                taken.chars().take(19).collect(),
                percent(&sample["cpu_pct"]),
                format!("{:.2}", sample["load1"].as_f64().unwrap_or_default()),
                percent(&sample["mem_used_pct"]),
                percent(&sample["disk_used_pct"]),
            ]
            .into_iter()
            .chain(shown.iter().map(|(_, _, cell)| cell(sample)))
            .chain([rate(&sample["net_rx_bps"]), rate(&sample["net_tx_bps"])])
            .collect()
        })
        .collect();
    ui.table(&headers, &rows)
}
