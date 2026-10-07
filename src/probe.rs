//! Collecting from machines' probes: turning probe readings into samples,
//! catching up from a ring, and keeping one stream per machine open,
//! reconnecting and resuming from the last sequence number after any gap.
//! Each stream also times round trips to its machine with echoes over
//! the same connection, so no other process, and no ICMP, is needed, and
//! a jump host or proxy command in the way is measured along with it.

use std::collections::{HashMap, VecDeque};
use std::io::{Read, Write};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use grove_probe::{
    BATTERY_STATES, Frame, Header, NONE_I16, NONE_U8, NONE_U16, NONE_U32, NONE_U64, Record,
    StreamReader,
};

use crate::output::now_ms;
use crate::reading::Sample;
use crate::store::{ConfigState, Facts, Latest, Machine, Store};
use crate::transport;

/// What a machine said about itself in its latest facts frame.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProbeFacts {
    /// The running probe's release; None for one too old to say.
    pub probe_version: Option<String>,
    pub hostname: String,
    pub os: String,
    pub arch: String,
    pub facts: Facts,
    pub config: ConfigState,
}

/// Reads a facts frame's `key=value` lines. Times on the machine's clock
/// are moved onto this one's with `offset_ms`.
pub fn parse_facts(text: &str, offset_ms: i64) -> ProbeFacts {
    let mut values: HashMap<&str, &str> = HashMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            values.insert(key, value.trim());
        }
    }
    let text = |key: &str| {
        values
            .get(key)
            .filter(|value| !value.is_empty())
            .map(|value| (*value).to_owned())
    };
    ProbeFacts {
        probe_version: text("probe_version"),
        hostname: text("hostname").unwrap_or_default(),
        os: text("os").unwrap_or_default(),
        arch: text("arch").unwrap_or_default(),
        facts: Facts {
            ip: text("ip"),
            model: text("model"),
            chip: text("chip"),
            os_version: text("os_version"),
            product_name: text("product_name"),
            gpu: text("gpu_name"),
            gpu_mem_total_mb: text("gpu_mem_total_mb").and_then(|value| value.parse().ok()),
        },
        config: ConfigState {
            commit: text("config_commit"),
            verify: text("config_verify").and_then(|value| value.parse().ok()),
            checked_at: text("config_checked_at_ms")
                .and_then(|value| value.parse::<i64>().ok())
                .map(|at| at - offset_ms),
        },
    }
}

/// A probe reading as a sample. None for a reading with no CPU share yet
/// (the first after the probe starts).
pub fn sample_from(
    record: &Record,
    machine: &str,
    facts: &ProbeFacts,
    offset_ms: i64,
) -> Option<Sample> {
    if record.cpu_pct_x10 == NONE_U16 {
        return None;
    }
    let tenths = |value: u16| (value != NONE_U16).then(|| f64::from(value) / 10.0);
    let temperature = |value: i16| (value != NONE_I16).then(|| f64::from(value) / 10.0);
    let count = |value: u16| (value != NONE_U16).then(|| f64::from(value));
    let kb = |value: u64| (value != NONE_U64).then_some(value as f64);
    let agents = match (count(record.claude), count(record.codex)) {
        (Some(claude), Some(codex)) => Some(claude + codex),
        _ => None,
    };
    Some(Sample {
        machine: machine.to_owned(),
        taken_at: record.taken_at_ms - offset_ms,
        hostname: facts.hostname.clone(),
        os: facts.os.clone(),
        arch: facts.arch.clone(),
        cores: f64::from(record.cores),
        load1: f64::from(record.load1_x100) / 100.0,
        load5: f64::from(record.load5_x100) / 100.0,
        load15: f64::from(record.load15_x100) / 100.0,
        cpu_pct: f64::from(record.cpu_pct_x10) / 10.0,
        mem_total_kb: record.mem_total_kb as f64,
        mem_available_kb: record.mem_available_kb as f64,
        disk_total_kb: record.disk_total_kb as f64,
        disk_used_kb: record.disk_used_kb as f64,
        net_rx_bytes: record.net_rx_bytes as f64,
        net_tx_bytes: record.net_tx_bytes as f64,
        uptime_s: (record.uptime_s != NONE_U32).then(|| f64::from(record.uptime_s)),
        swap_total_kb: kb(record.swap_total_kb),
        swap_used_kb: kb(record.swap_used_kb),
        cpu_temp_c: temperature(record.cpu_temp_x10),
        gpu_temp_c: temperature(record.gpu_temp_x10),
        battery_pct: (record.battery_pct != NONE_U8).then(|| f64::from(record.battery_pct)),
        battery_state: BATTERY_STATES
            .get(usize::from(record.battery_state))
            .filter(|state| !state.is_empty())
            .map(|state| (*state).to_owned()),
        agent_sessions: agents,
        claude_sessions: count(record.claude),
        codex_sessions: count(record.codex),
        gpu_util_pct: tenths(record.gpu_util_x10),
        gpu_mem_used_mb: (record.gpu_mem_used_mb != NONE_U32)
            .then(|| f64::from(record.gpu_mem_used_mb)),
        latency_ms: None,
        agent_cpu_pct: tenths(record.agent_cpu_x10),
        agent_rss_mb: (record.agent_rss_mb != NONE_U32).then(|| f64::from(record.agent_rss_mb)),
        ping_target: None,
        address: None,
    })
}

fn probe_words(dir: &str, words: &[&str]) -> Vec<String> {
    let mut all = vec![format!("{dir}/grove-probe")];
    all.extend(words.iter().map(|word| (*word).to_owned()));
    all.extend(["--dir".to_owned(), format!("{dir}/data")]);
    all
}

/// The probe's newest reading and its facts, read once over a short run.
pub fn read_latest(
    machine: &Machine,
    home: &std::path::Path,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let probe = machine
        .probe
        .as_ref()
        .ok_or_else(|| "no probe".to_owned())?;
    let (mut command, program) = transport::command_on(
        machine,
        home,
        &probe_words(&probe.dir, &["read", "--last", "1"]),
    );
    let ran = transport::run(&mut command, program, None, timeout);
    if ran.ok() {
        Ok(ran.stdout_bytes)
    } else {
        Err(ran.failure())
    }
}

/// Facts and the newest reading from what `read` printed.
pub fn decode(bytes: &[u8]) -> Result<(Header, Option<String>, Option<Record>), String> {
    let (mut reader, header) = StreamReader::new(bytes).map_err(|error| error.to_string())?;
    let (mut facts, mut last) = (None, None);
    while let Some(frame) = reader.next_frame().map_err(|error| error.to_string())? {
        match frame {
            Frame::Facts(text) => facts = Some(text),
            Frame::Reading(record) => last = Some(record),
            Frame::Echo(_) => {}
        }
    }
    Ok((header, facts, last))
}

/// The machine's clock minus this one's, measured over one short run: the
/// probe's time against the midpoint of the round trip.
pub fn clock_offset(machine: &Machine, home: &std::path::Path) -> Option<i64> {
    if transport::is_local(&machine.endpoint) {
        return Some(0);
    }
    let probe = machine.probe.as_ref()?;
    let (mut command, program) = transport::command_on(
        machine,
        home,
        &[format!("{}/grove-probe", probe.dir), "clock".into()],
    );
    let before = now_ms();
    let ran = transport::run(&mut command, program, None, Duration::from_secs(10));
    let after = now_ms();
    let theirs: i64 = ran.stdout.trim().parse().ok()?;
    Some(theirs - (before + after) / 2)
}

/// An echo goes out this often once a stream is up.
const ECHO_EVERY: Duration = Duration::from_secs(10);
/// An echo not answered in this long counts as lost.
const ECHO_TIMEOUT: Duration = Duration::from_secs(5);
/// The round trip is the median of the last this many echoes.
const ECHO_KEPT: usize = 5;
/// Echoes stop after this many go unanswered in a row, so a probe that
/// never reads them can't fill the connection's buffers.
const ECHO_GIVE_UP: u32 = 3;

/// One stream's round trips: an echo every `ECHO_EVERY`, one at a time,
/// and the median of the last few answers, so one slow reply doesn't read
/// as a spike. Times are passed in, so the logic runs on any clock.
#[derive(Debug, Default)]
pub struct Echoes {
    /// The probe said which release it is, so it answers echoes.
    answers: bool,
    next_token: u64,
    pending: Option<(u64, Instant)>,
    last_sent: Option<Instant>,
    /// The latest outcomes, oldest first: a round trip, or None for lost.
    kept: VecDeque<Option<f64>>,
    unanswered: u32,
}

impl Echoes {
    fn keep(&mut self, outcome: Option<f64>) {
        if self.kept.len() == ECHO_KEPT {
            self.kept.pop_front();
        }
        self.kept.push_back(outcome);
    }

    /// The token to send now, if one is due. A pending echo past its
    /// timeout is counted lost first.
    pub fn due(&mut self, now: Instant) -> Option<u64> {
        if let Some((_, sent)) = self.pending
            && now.duration_since(sent) >= ECHO_TIMEOUT
        {
            self.pending = None;
            self.unanswered += 1;
            self.keep(None);
        }
        let waited = self
            .last_sent
            .is_none_or(|sent| now.duration_since(sent) >= ECHO_EVERY);
        if !self.answers || self.pending.is_some() || !waited || self.unanswered >= ECHO_GIVE_UP {
            return None;
        }
        self.next_token += 1;
        self.pending = Some((self.next_token, now));
        self.last_sent = Some(now);
        Some(self.next_token)
    }

    /// Takes in an answer. An answer to a lost or unknown echo is ignored.
    pub fn answered(&mut self, token: u64, now: Instant) {
        let Some((pending, sent)) = self.pending else {
            return;
        };
        if pending != token {
            return;
        }
        self.pending = None;
        self.unanswered = 0;
        let ms = now.duration_since(sent).as_secs_f64() * 1000.0;
        self.keep(Some(crate::output::round1(ms)));
    }

    /// The median of the answered echoes among the last few; None when
    /// none of them was answered.
    pub fn median(&self) -> Option<f64> {
        let mut times: Vec<f64> = self.kept.iter().flatten().copied().collect();
        times.sort_by(f64::total_cmp);
        let count = times.len();
        match count {
            0 => None,
            _ if count % 2 == 1 => Some(times[count / 2]),
            _ => Some(crate::output::round1(
                (times[count / 2 - 1] + times[count / 2]) / 2.0,
            )),
        }
    }
}

/// What a stream thread tells the collector.
enum Message {
    Connected {
        index: usize,
        header: Header,
        offset: i64,
    },
    Facts {
        index: usize,
        text: String,
    },
    /// The stream's round trip changed.
    RoundTrip {
        index: usize,
        ms: Option<f64>,
    },
    Reading {
        index: usize,
        record: Box<Record>,
        received_at: i64,
        bytes: usize,
    },
    Ended {
        index: usize,
        error: String,
    },
}

/// One machine's stream, kept open until `stop`: follow from the last
/// sequence number, and after any gap reconnect and resume there.
fn follow(
    index: usize,
    machine: Machine,
    home: std::path::PathBuf,
    since: Arc<std::sync::atomic::AtomicU64>,
    child_pid: Arc<AtomicI32>,
    stop: Arc<AtomicBool>,
    sender: mpsc::Sender<Message>,
) {
    let mut backoff = Duration::from_secs(1);
    while !stop.load(Ordering::SeqCst) {
        let Some(probe) = machine.probe.clone() else {
            return;
        };
        let from = since.load(Ordering::SeqCst).to_string();
        let words = probe_words(&probe.dir, &["follow", "--since", &from]);
        let (mut command, _) = transport::command_on(&machine, &home, &words);
        use std::os::unix::process::CommandExt;
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .process_group(0);
        let started = Instant::now();
        let error = match command.spawn() {
            Err(error) => error.to_string(),
            Ok(mut child) => {
                child_pid.store(child.id() as i32, Ordering::SeqCst);
                let stdout = child.stdout.take().expect("piped");
                let echo = child.stdin.take();
                // The clock is read once the stream is up, so the round trip
                // rides its shared connection and is short and even.
                let offset = || clock_offset(&machine, &home).unwrap_or(probe.clock_offset_ms);
                let outcome = read_stream(index, stdout, echo, offset, &since, &sender);
                // SAFETY: signals only the process group this thread started.
                unsafe { libc::kill(-(child.id() as i32), libc::SIGTERM) };
                let _ = child.wait();
                child_pid.store(0, Ordering::SeqCst);
                outcome
            }
        };
        let _ = sender.send(Message::Ended { index, error });
        if started.elapsed() > Duration::from_secs(30) {
            backoff = Duration::from_secs(1);
        }
        let until = Instant::now() + backoff;
        while Instant::now() < until && !stop.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(100));
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Reads one stream until it ends, sending echoes down `echo` (the
/// stream's stdin) as they fall due. Readings arrive every two seconds,
/// so checking after each frame keeps the cadence without a thread.
fn read_stream(
    index: usize,
    input: impl Read,
    mut echo: Option<impl Write>,
    offset: impl FnOnce() -> i64,
    since: &std::sync::atomic::AtomicU64,
    sender: &mpsc::Sender<Message>,
) -> String {
    let (mut reader, header) = match StreamReader::new(input) {
        Ok(opened) => opened,
        Err(error) => return error.to_string(),
    };
    let _ = sender.send(Message::Connected {
        index,
        header,
        offset: offset(),
    });
    let mut echoes = Echoes::default();
    let mut round_trip = None;
    loop {
        let frame = reader.next_frame();
        match &frame {
            Ok(Some(Frame::Echo(token))) => echoes.answered(*token, Instant::now()),
            // Only a probe that names its release answers echoes.
            Ok(Some(Frame::Facts(text))) => {
                echoes.answers =
                    echo.is_some() && text.lines().any(|line| line.starts_with("probe_version="));
            }
            _ => {}
        }
        if let Some(token) = echoes.due(Instant::now())
            && echo.as_mut().is_some_and(|pipe| {
                pipe.write_all(&token.to_le_bytes())
                    .and_then(|()| pipe.flush())
                    .is_err()
            })
        {
            echo = None;
        }
        if echoes.median() != round_trip {
            round_trip = echoes.median();
            let _ = sender.send(Message::RoundTrip {
                index,
                ms: round_trip,
            });
        }
        match frame {
            Ok(Some(Frame::Echo(_))) => {}
            Ok(Some(Frame::Facts(text))) => {
                let _ = sender.send(Message::Facts { index, text });
            }
            Ok(Some(Frame::Reading(record))) => {
                since.store(record.seq, Ordering::SeqCst);
                let _ = sender.send(Message::Reading {
                    index,
                    record: Box::new(record),
                    received_at: now_ms(),
                    bytes: grove_probe::RECORD_SIZE,
                });
            }
            Ok(None) => return "the stream ended".into(),
            Err(error) => return error.to_string(),
        }
    }
}

/// What the collector knows of one machine while it streams.
#[derive(Default)]
pub struct Tally {
    pub readings: u64,
    pub connects: u64,
    pub bytes: u64,
    pub latencies_ms: Vec<i64>,
    pub last_error: Option<String>,
    facts: ProbeFacts,
    round_trip_ms: Option<f64>,
    offset: i64,
    ring_id: Option<i64>,
    seq: u64,
    minute: Option<i64>,
    pending: Option<Sample>,
    cpu: (f64, u32),
    agent_cpu: (f64, u32),
    previous: Option<Sample>,
    dirty: bool,
}

/// What a write tells whoever runs the collector, once it has committed:
/// a machine connected, sent its facts, has a new latest reading, or lost
/// its stream.
pub struct Report {
    pub kind: &'static str,
    pub data: serde_json::Value,
}

/// Collects from every machine's probe until `stop` is set or `until`
/// passes: full-resolution readings into the live window, one sample a
/// minute into history (with its alerts), and the last reading for
/// `show` and `status`. Hooks run after each write, and `report` hears
/// what each write changed.
pub fn collect(
    store: &Store,
    machines: &[Machine],
    until: Option<Instant>,
    stop: &Arc<AtomicBool>,
    report: &mut dyn FnMut(Report),
) -> crate::store::Result<Vec<Tally>> {
    let (sender, receiver) = mpsc::channel();
    let mut tallies: Vec<Tally> = machines.iter().map(|_| Tally::default()).collect();
    let mut pids = Vec::new();
    let mut threads = Vec::new();
    for (index, machine) in machines.iter().enumerate() {
        let probe = machine
            .probe
            .as_ref()
            .expect("only machines with probes stream");
        tallies[index].ring_id = probe.ring_id;
        tallies[index].seq = probe.seq.max(0) as u64;
        let since = Arc::new(std::sync::atomic::AtomicU64::new(tallies[index].seq));
        let pid = Arc::new(AtomicI32::new(0));
        pids.push(pid.clone());
        let (machine, home, stop, sender) = (
            machine.clone(),
            store.home.clone(),
            stop.clone(),
            sender.clone(),
        );
        threads.push(std::thread::spawn(move || {
            follow(index, machine, home, since, pid, stop, sender)
        }));
    }
    drop(sender);
    let mut last_write = Instant::now();
    let mut last_prune = Instant::now();
    let mut batch: Vec<Message> = Vec::new();
    loop {
        let finished =
            stop.load(Ordering::SeqCst) || until.is_some_and(|until| Instant::now() >= until);
        match receiver.recv_timeout(Duration::from_millis(250)) {
            Ok(message) => batch.push(message),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if finished || last_write.elapsed() >= Duration::from_secs(1) {
            let prune = last_prune.elapsed() >= Duration::from_secs(60);
            if prune {
                last_prune = Instant::now();
            }
            let mut reports = Vec::new();
            let events = store.write(|store| {
                apply(
                    store,
                    machines,
                    &mut tallies,
                    std::mem::take(&mut batch),
                    prune,
                    &mut reports,
                )
            })?;
            reports.into_iter().for_each(&mut *report);
            let _ = crate::hooks::run_for_events(store, &events);
            last_write = Instant::now();
        }
        if finished {
            break;
        }
    }
    stop.store(true, Ordering::SeqCst);
    for pid in &pids {
        let pid = pid.load(Ordering::SeqCst);
        if pid > 0 {
            // SAFETY: signals only a process group a stream thread started.
            unsafe { libc::kill(-pid, libc::SIGTERM) };
        }
    }
    for thread in threads {
        let _ = thread.join();
    }
    Ok(tallies)
}

/// One write's worth of stream messages.
fn apply(
    store: &Store,
    machines: &[Machine],
    tallies: &mut [Tally],
    batch: Vec<Message>,
    prune: bool,
    reports: &mut Vec<Report>,
) -> crate::store::Result<Vec<crate::alerts::Event>> {
    reports.clear();
    let mut events = Vec::new();
    let now = now_ms();
    for message in batch {
        match message {
            Message::Connected {
                index,
                header,
                offset,
            } => {
                let tally = &mut tallies[index];
                tally.connects += 1;
                tally.offset = offset;
                tally.round_trip_ms = None;
                let ring_id = header.ring_id as i64;
                // A new ring numbers its readings from 1 again.
                if tally.ring_id.is_some_and(|known| known != ring_id) {
                    tally.seq = 0;
                }
                tally.ring_id = Some(ring_id);
                reports.push(Report {
                    kind: "connected",
                    data: serde_json::json!({ "machine": machines[index].name }),
                });
            }
            Message::Facts { index, text } => {
                let tally = &mut tallies[index];
                tally.facts = parse_facts(&text, tally.offset);
                let name = &machines[index].name;
                store.update_facts(name, &tally.facts.facts, now)?;
                if tally.facts.config.checked_at.is_some() {
                    store.update_config(name, &tally.facts.config)?;
                }
                let facts = &tally.facts;
                reports.push(Report {
                    kind: "facts",
                    data: serde_json::json!({
                        "arch": facts.arch,
                        "chip": facts.facts.chip,
                        "gpu": facts.facts.gpu,
                        "gpu_mem_total_mb": crate::output::opt_num(facts.facts.gpu_mem_total_mb),
                        "hostname": facts.hostname,
                        "ip": facts.facts.ip,
                        "machine": name,
                        "model": facts.facts.model,
                        "os": facts.os,
                        "os_version": facts.facts.os_version,
                        "probe_version": facts.probe_version,
                        "product_name": facts.facts.product_name,
                    }),
                });
            }
            Message::RoundTrip { index, ms } => tallies[index].round_trip_ms = ms,
            Message::Reading {
                index,
                record,
                received_at,
                bytes,
            } => {
                let name = &machines[index].name;
                let tally = &mut tallies[index];
                tally.readings += 1;
                tally.bytes += bytes as u64;
                tally.seq = record.seq;
                tally.dirty = true;
                let taken_at = record.taken_at_ms - tally.offset;
                tally.latencies_ms.push(received_at - taken_at);
                store.insert_live(
                    name,
                    record.seq as i64,
                    taken_at,
                    received_at,
                    &record.encode(),
                )?;
                let Some(mut sample) = sample_from(&record, name, &tally.facts, tally.offset)
                else {
                    continue;
                };
                sample.latency_ms = tally.round_trip_ms;
                let minute = taken_at / 60_000;
                if tally.minute.is_some_and(|current| current != minute)
                    && let Some(mut stored) = tally.pending.take()
                {
                    // The minute's sample carries its mean CPU, not one
                    // two-second glimpse of it.
                    if tally.cpu.1 > 0 {
                        stored.cpu_pct =
                            crate::output::round1(tally.cpu.0 / f64::from(tally.cpu.1));
                    }
                    if tally.agent_cpu.1 > 0 {
                        stored.agent_cpu_pct = Some(crate::output::round1(
                            tally.agent_cpu.0 / f64::from(tally.agent_cpu.1),
                        ));
                    }
                    store.insert_sample(&stored)?;
                    events.extend(store.evaluate_thresholds(&stored)?);
                    tally.cpu = (0.0, 0);
                    tally.agent_cpu = (0.0, 0);
                }
                tally.minute = Some(minute);
                tally.cpu.0 += sample.cpu_pct;
                tally.cpu.1 += 1;
                if let Some(agent) = sample.agent_cpu_pct {
                    tally.agent_cpu.0 += agent;
                    tally.agent_cpu.1 += 1;
                }
                tally.pending = Some(sample.clone());
                tally.previous = Some(sample);
                let _ = received_at;
            }
            Message::Ended { index, error } => {
                reports.push(Report {
                    kind: "disconnected",
                    data: serde_json::json!({ "error": error, "machine": machines[index].name }),
                });
                tallies[index].last_error = Some(error);
                events.extend(store.record_contact(&machines[index].name, now, false)?);
            }
        }
    }
    for (machine, tally) in machines.iter().zip(tallies.iter_mut()) {
        if !std::mem::take(&mut tally.dirty) {
            continue;
        }
        events.extend(store.record_contact(&machine.name, now, true)?);
        if let Some(sample) = &tally.previous {
            let previous = store.latest(&machine.name)?;
            let seconds = previous.as_ref().map_or(0.0, |previous| {
                (sample.taken_at - previous.taken_at) as f64 / 1000.0
            });
            let mut reading = sample.record();
            let rate = |now: f64, before: Option<f64>| {
                crate::output::opt_num(
                    before.and_then(|before| crate::reading::rate(now, before, seconds)),
                )
            };
            reading["net_rx_bps"] = rate(
                sample.net_rx_bytes,
                previous.as_ref().map(|previous| previous.net_rx_bytes),
            );
            reading["net_tx_bps"] = rate(
                sample.net_tx_bytes,
                previous.as_ref().map(|previous| previous.net_tx_bytes),
            );
            reports.push(Report {
                kind: "reading",
                data: serde_json::json!({ "machine": machine.name, "reading": reading }),
            });
            store.set_latest(
                &machine.name,
                &Latest {
                    taken_at: sample.taken_at,
                    reading: reading.clone(),
                    net_rx_bytes: sample.net_rx_bytes,
                    net_tx_bytes: sample.net_tx_bytes,
                    jiffies: None,
                },
            )?;
        }
        store.set_probe_position(
            &machine.name,
            tally.ring_id.unwrap_or(0),
            tally.seq as i64,
            tally.offset,
        )?;
    }
    if prune {
        store.prune_live(now - crate::store::LIVE_WINDOW_MS)?;
        events.extend(store.evaluate_fleet_sessions(now)?);
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Echoes go out one at a time every ten seconds once the probe says
    /// it answers them; the round trip is the median of the last five, a
    /// lost one drops out of it, and three lost in a row stop them.
    #[test]
    fn echoes_time_the_round_trip_and_give_up_on_a_silent_probe() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let mut echoes = Echoes::default();
        assert_eq!(
            echoes.due(at(0)),
            None,
            "not before the probe says it answers"
        );
        echoes.answers = true;
        let first = echoes.due(at(0)).expect("the first goes out at once");
        assert_eq!(echoes.due(at(1_000)), None, "one at a time");
        echoes.answered(first + 9, at(30));
        assert_eq!(echoes.median(), None, "an unknown token is ignored");
        echoes.answered(first, at(30));
        assert_eq!(echoes.median(), Some(30.0));
        assert_eq!(echoes.due(at(9_000)), None, "every ten seconds");
        for (sent, took) in [(10_000, 40), (20_000, 400), (30_000, 20)] {
            let token = echoes.due(at(sent)).expect("due");
            echoes.answered(token, at(sent + took));
        }
        assert_eq!(
            echoes.median(),
            Some(35.0),
            "one slow reply doesn't move it far"
        );
        // Lost: the pending one times out at the next check.
        let lost = echoes.due(at(40_000)).expect("due");
        assert_eq!(
            echoes.due(at(45_000)),
            None,
            "counted lost, not due again yet"
        );
        echoes.answered(lost, at(45_100));
        assert_eq!(
            echoes.median(),
            Some(35.0),
            "a lost one drops out, and a late answer counts for nothing"
        );
        let mut silent = Echoes {
            answers: true,
            ..Echoes::default()
        };
        for second in [0, 10, 20] {
            assert!(silent.due(at(second * 1000)).is_some(), "{second}");
        }
        assert_eq!(
            silent.due(at(30_000)),
            None,
            "three lost in a row stop them"
        );
        assert_eq!(silent.median(), None);
    }

    /// A stream answering echoes gives its readings a round trip; one
    /// from a probe that doesn't (it names no release) is never asked.
    #[test]
    fn a_stream_times_echoes_from_a_probe_that_answers() {
        let header = Header {
            capacity: grove_probe::CAPACITY,
            interval_ms: grove_probe::INTERVAL_MS,
            last_seq: 0,
            ring_id: 1,
            probe_cpu_us: 0,
            probe_rss_kb: 0,
        };
        let run = |facts: &str| {
            let mut wire = header.encode().to_vec();
            wire.extend(grove_probe::facts_frame(facts));
            wire.extend(grove_probe::echo_frame(1));
            let (sender, receiver) = mpsc::channel();
            let mut asked = Vec::new();
            let since = std::sync::atomic::AtomicU64::new(0);
            read_stream(0, wire.as_slice(), Some(&mut asked), || 0, &since, &sender);
            drop(sender);
            let trips: Vec<Option<f64>> = receiver
                .iter()
                .filter_map(|message| match message {
                    Message::RoundTrip { ms, .. } => Some(ms),
                    _ => None,
                })
                .collect();
            (asked, trips)
        };
        let (asked, trips) = run("probe_version=0.1.3\nhostname=cedar-01\n");
        assert_eq!(asked, 1_u64.to_le_bytes());
        assert_eq!(trips.len(), 1, "{trips:?}");
        assert!(trips[0].is_some_and(|ms| ms < 1000.0), "{trips:?}");
        let (asked, trips) = run("hostname=cedar-01\n");
        assert!(asked.is_empty() && trips.is_empty());
    }

    /// A probe reading lands in the same sample a script reading of the
    /// same machine gives: tenths and hundredths back to their units,
    /// sentinels back to nothing, times onto this clock.
    #[test]
    fn a_probe_reading_becomes_a_sample() {
        let facts = parse_facts(
            "probe_version=0.1.3\nhostname=cedar-01\nos=Linux\narch=x86_64\nmodel=MS-7D25\nconfig_commit=0123456789abcdef0123456789abcdef01234567\nconfig_verify=0\nconfig_checked_at_ms=10500\n",
            500,
        );
        assert_eq!(
            (
                facts.facts.model.as_deref(),
                facts.config.checked_at,
                facts.probe_version.as_deref()
            ),
            (Some("MS-7D25"), Some(10_000), Some("0.1.3"))
        );
        let record = Record {
            seq: 9,
            taken_at_ms: 1_000_500,
            cores: 24,
            cpu_pct_x10: 7,
            load1_x100: 7,
            mem_total_kb: 65_618_936,
            mem_available_kb: 55_493_984,
            swap_total_kb: 33_554_424,
            swap_used_kb: 8_171_072,
            disk_total_kb: 982_292_956,
            disk_used_kb: 510_181_132,
            cpu_temp_x10: 320,
            claude: 1,
            codex: 0,
            ..Record::default()
        };
        let sample = sample_from(&record, "cedar-01", &facts, 500).expect("a sample");
        let reading = sample.record();
        assert_eq!(sample.taken_at, 1_000_000);
        assert_eq!(
            (
                &reading["cpu_pct"],
                &reading["load1"],
                &reading["cpu_temp_c"]
            ),
            (
                &serde_json::json!(0.7),
                &serde_json::json!(0.07),
                &serde_json::json!(32)
            )
        );
        assert_eq!(
            (&reading["mem_used_pct"], &reading["swap_used_pct"]),
            (&serde_json::json!(15.4), &serde_json::json!(24.4))
        );
        assert_eq!(
            (
                &reading["agent_sessions"],
                &reading["gpu_temp_c"],
                &reading["battery_pct"]
            ),
            (
                &serde_json::json!(1),
                &serde_json::Value::Null,
                &serde_json::Value::Null
            )
        );
        assert!(
            sample_from(
                &Record {
                    cpu_pct_x10: NONE_U16,
                    ..record
                },
                "cedar-01",
                &facts,
                0
            )
            .is_none()
        );
    }
}
