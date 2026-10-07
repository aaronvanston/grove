//! Linux readings from /proc, /sys and statvfs.

use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use grove_probe::{NONE_I16, NONE_U16, NONE_U32, NONE_U64, Record};

fn read(path: &str) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn read_path(path: &PathBuf) -> Option<String> {
    fs::read_to_string(path).ok()
}

/// A sysfs value in thousandths (millidegrees) to tenths, rounded.
fn milli_to_tenths(text: &str) -> Option<i16> {
    let value: i64 = text.trim().parse().ok()?;
    i16::try_from((value + if value >= 0 { 50 } else { -50 }) / 100).ok()
}

fn field(text: &str, key: &str) -> Option<u64> {
    text.lines()
        .find(|line| line.starts_with(key))?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()
}

/// Where each sensor lives, found once.
pub struct Platform {
    cpu_temp: Option<PathBuf>,
    gpu_temp: Option<PathBuf>,
    gpu_busy: Option<PathBuf>,
    battery: Option<PathBuf>,
    nvidia: Option<PathBuf>,
    nvidia_due: Option<Instant>,
    nvidia_last: (i16, u16, u32),
    temperatures: (u32, i16, i16),
    clock_ticks: u64,
    page_kb: u64,
    stats: std::collections::HashMap<i32, fs::File>,
    buffer: Vec<u8>,
}

/// Listing /proc walks every task in the kernel, more than the rest of a
/// reading costs, so new processes are looked for every fifth reading.
pub const LIST_EVERY: u32 = 5;

/// Temperatures are read every fifth reading.
const TEMPERATURE_EVERY: u32 = 5;

/// NVIDIA cards are read with nvidia-smi, the only way in without the
/// driver's library, and every two minutes rather than every reading.
const NVIDIA_EVERY: std::time::Duration = std::time::Duration::from_secs(120);

fn hwmon(names: &[&str]) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir("/sys/class/hwmon")
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    entries.into_iter().find_map(|dir| {
        let name = read_path(&dir.join("name"))?;
        let path = dir.join("temp1_input");
        (names.contains(&name.trim()) && path.exists()).then_some(path)
    })
}

fn thermal_zone() -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir("/sys/class/thermal")
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .collect();
    entries.sort();
    entries.into_iter().find_map(|dir| {
        let kind = read_path(&dir.join("type"))?;
        matches!(kind.trim(), "x86_pkg_temp" | "cpu-thermal" | "cpu_thermal")
            .then(|| dir.join("temp"))
    })
}

fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|dir| PathBuf::from(dir).join(program))
        .chain(["/usr/bin", "/usr/local/bin"].map(|dir| PathBuf::from(dir).join(program)))
        .find(|path| path.is_file())
}

impl Platform {
    pub fn new() -> Self {
        let nvidia = which("nvidia-smi");
        let mut gpu_temp = None;
        let mut gpu_busy = None;
        if nvidia.is_none()
            && let Ok(cards) = fs::read_dir("/sys/class/drm")
        {
            let mut cards: Vec<PathBuf> = cards.flatten().map(|entry| entry.path()).collect();
            cards.sort();
            for card in cards {
                let vendor = read_path(&card.join("device/vendor")).unwrap_or_default();
                if !matches!(vendor.trim(), "0x1002" | "0x10de") {
                    continue;
                }
                let busy = card.join("device/gpu_busy_percent");
                gpu_busy = busy.exists().then_some(busy);
                gpu_temp = fs::read_dir(card.join("device/hwmon"))
                    .ok()
                    .and_then(|dirs| {
                        dirs.flatten()
                            .map(|dir| dir.path().join("temp1_input"))
                            .find(|path| path.exists())
                    });
                break;
            }
        }
        if gpu_temp.is_none() && nvidia.is_none() {
            gpu_temp = hwmon(&["amdgpu"]);
        }
        let battery = fs::read_dir("/sys/class/power_supply")
            .ok()
            .and_then(|entries| {
                let mut entries: Vec<PathBuf> =
                    entries.flatten().map(|entry| entry.path()).collect();
                entries.sort();
                entries.into_iter().find(|path| {
                    path.file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("BAT"))
                        && path.join("capacity").exists()
                })
            });
        // SAFETY: sysconf only reads configuration.
        let (ticks, page) = unsafe {
            (
                libc::sysconf(libc::_SC_CLK_TCK),
                libc::sysconf(libc::_SC_PAGESIZE),
            )
        };
        Self {
            cpu_temp: hwmon(&[
                "coretemp",
                "k10temp",
                "zenpower",
                "cpu_thermal",
                "soc_thermal",
            ])
            .or_else(thermal_zone),
            gpu_temp,
            gpu_busy,
            battery,
            nvidia,
            nvidia_due: None,
            nvidia_last: (NONE_I16, NONE_U16, NONE_U32),
            temperatures: (0, NONE_I16, NONE_I16),
            clock_ticks: u64::try_from(ticks).unwrap_or(100).max(1),
            page_kb: u64::try_from(page).unwrap_or(4096) / 1024,
            stats: std::collections::HashMap::new(),
            buffer: vec![0; 4096],
        }
    }

    /// Total and idle (idle plus iowait) jiffies across all CPUs.
    pub fn cpu_ticks(&mut self) -> Option<(u64, u64)> {
        let stat = read("/proc/stat")?;
        let values: Vec<u64> = stat
            .lines()
            .next()?
            .split_whitespace()
            .skip(1)
            .take(8)
            .filter_map(|value| value.parse().ok())
            .collect();
        (values.len() == 8).then(|| (values.iter().sum(), values[3] + values[4]))
    }

    pub fn read(&mut self, record: &mut Record) {
        // SAFETY: sysconf only reads configuration.
        record.cores = u16::try_from(unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) })
            .unwrap_or(1)
            .max(1);
        if let Some(loads) = read("/proc/loadavg") {
            let mut loads = loads
                .split_whitespace()
                .map(|value| value.parse::<f64>().unwrap_or(0.0));
            let mut next = || {
                (loads.next().unwrap_or(0.0) * 100.0)
                    .round()
                    .min(f64::from(u16::MAX - 1)) as u16
            };
            record.load1_x100 = next();
            record.load5_x100 = next();
            record.load15_x100 = next();
        }
        if let Some(memory) = read("/proc/meminfo") {
            record.mem_total_kb = field(&memory, "MemTotal:").unwrap_or(0);
            record.mem_available_kb = field(&memory, "MemAvailable:").unwrap_or(0);
            if let (Some(total), Some(free)) =
                (field(&memory, "SwapTotal:"), field(&memory, "SwapFree:"))
            {
                record.swap_total_kb = total;
                record.swap_used_kb = total.saturating_sub(free);
            }
        }
        if let Some(net) = read("/proc/net/dev") {
            for line in net.lines().skip(2) {
                let line = line.replace(':', " ");
                let fields: Vec<&str> = line.split_whitespace().collect();
                if fields.first() == Some(&"lo") || fields.len() < 10 {
                    continue;
                }
                record.net_rx_bytes += fields[1].parse::<u64>().unwrap_or(0);
                record.net_tx_bytes += fields[9].parse::<u64>().unwrap_or(0);
            }
        }
        if let Some(uptime) = read("/proc/uptime")
            && let Some(seconds) = uptime
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<f64>().ok())
        {
            record.uptime_s = seconds as u32;
        }
        // SAFETY: statvfs fills the struct it is given.
        let mut disk: libc::statvfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::statvfs(c"/".as_ptr(), &mut disk) } == 0 {
            let block = disk.f_frsize as u64;
            let total = disk.f_blocks as u64 * block / 1024;
            let available = disk.f_bavail as u64 * block / 1024;
            record.disk_total_kb = total;
            record.disk_used_kb = total.saturating_sub(available);
        }
        // Temperatures move slowly, and a sensor read can reach the
        // device, so they are read every fifth reading and carried between.
        if self.temperatures.0.is_multiple_of(TEMPERATURE_EVERY) {
            let read = |path: &Option<PathBuf>| {
                path.as_ref()
                    .and_then(read_path)
                    .and_then(|text| milli_to_tenths(&text))
                    .unwrap_or(NONE_I16)
            };
            self.temperatures.1 = read(&self.cpu_temp);
            if self.gpu_temp.is_some() {
                self.temperatures.2 = read(&self.gpu_temp);
            }
        }
        self.temperatures.0 = self.temperatures.0.wrapping_add(1);
        record.cpu_temp_x10 = self.temperatures.1;
        if self.gpu_temp.is_some() {
            record.gpu_temp_x10 = self.temperatures.2;
        }
        if let Some(path) = &self.gpu_busy {
            record.gpu_util_x10 = read_path(path)
                .and_then(|text| text.trim().parse::<u16>().ok())
                .map_or(NONE_U16, |busy| busy.saturating_mul(10));
        }
        self.read_nvidia(record);
        if let Some(battery) = &self.battery {
            record.battery_pct = read_path(&battery.join("capacity"))
                .and_then(|text| text.trim().parse::<u8>().ok())
                .unwrap_or(u8::MAX);
            let state = read_path(&battery.join("status"))
                .unwrap_or_else(|| "unknown".into())
                .trim()
                .to_lowercase();
            record.battery_state = grove_probe::BATTERY_STATES
                .iter()
                .position(|known| *known == state)
                .unwrap_or(8) as u8;
        }
        if record.swap_total_kb == NONE_U64 {
            record.swap_used_kb = NONE_U64;
        }
    }

    fn read_nvidia(&mut self, record: &mut Record) {
        let Some(nvidia) = &self.nvidia else {
            return;
        };
        let now = Instant::now();
        if self.nvidia_due.is_none_or(|due| now >= due) {
            self.nvidia_due = Some(now + NVIDIA_EVERY);
            let output = std::process::Command::new(nvidia)
                .args([
                    "--query-gpu=temperature.gpu,utilization.gpu,memory.used",
                    "--format=csv,noheader,nounits",
                ])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output();
            if let Ok(output) = output {
                let text = String::from_utf8_lossy(&output.stdout);
                let fields: Vec<&str> = text
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .split(", ")
                    .collect();
                let number = |index: usize| {
                    fields
                        .get(index)
                        .and_then(|value| value.trim().parse::<f64>().ok())
                };
                self.nvidia_last = (
                    number(0).map_or(NONE_I16, |value| (value * 10.0).round() as i16),
                    number(1).map_or(NONE_U16, |value| (value * 10.0).round() as u16),
                    number(2).map_or(NONE_U32, |value| value as u32),
                );
            }
        }
        (
            record.gpu_temp_x10,
            record.gpu_util_x10,
            record.gpu_mem_used_mb,
        ) = self.nvidia_last;
    }

    pub fn pids(&mut self) -> Option<Vec<i32>> {
        Some(
            fs::read_dir("/proc")
                .ok()?
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse().ok())
                .collect(),
        )
    }

    /// The process's arguments joined by spaces, as `ps` prints them.
    pub fn arguments(&mut self, pid: i32) -> Option<String> {
        let bytes = fs::read(format!("/proc/{pid}/cmdline")).ok()?;
        let text = String::from_utf8_lossy(&bytes);
        let joined = text.trim_end_matches('\0').replace('\0', " ");
        (!joined.is_empty()).then_some(joined)
    }

    /// CPU time in nanoseconds and resident memory in kB. An agent's stat
    /// file stays open while it runs and is read again from the start; a
    /// read that finds nothing means the process is gone.
    pub fn usage(&mut self, pid: i32) -> Option<(u64, u64)> {
        use std::os::unix::fs::FileExt;
        let file = match self.stats.entry(pid) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(fs::File::open(format!("/proc/{pid}/stat")).ok()?)
            }
        };
        let read = file
            .read_at(&mut self.buffer, 0)
            .ok()
            .filter(|read| *read > 0);
        let Some(read) = read else {
            self.stats.remove(&pid);
            return None;
        };
        let stat = std::str::from_utf8(&self.buffer[..read]).ok()?;
        // The command name may hold spaces; the fields after it don't.
        let fields: Vec<&str> = stat[stat.rfind(')')? + 2..].split_whitespace().collect();
        let ticks: u64 =
            fields.get(11)?.parse::<u64>().ok()? + fields.get(12)?.parse::<u64>().ok()?;
        let rss_pages: u64 = fields.get(21)?.parse().ok()?;
        Some((
            ticks * 1_000_000_000 / self.clock_ticks,
            rss_pages * self.page_kb,
        ))
    }

    /// Closes the stat files of agents that are no longer known.
    pub fn forget(&mut self, keep: &dyn Fn(i32) -> bool) {
        self.stats.retain(|pid, _| keep(*pid));
    }
}
