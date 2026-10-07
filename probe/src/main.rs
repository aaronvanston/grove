//! grove-probe: a resident sampler. `run` takes a reading every two
//! seconds into a ring file holding the last hour; `follow` streams new
//! readings as they land, and `read` catches up once.
//!
//! Usage:
//!   grove-probe run [--dir DIR]
//!   grove-probe follow [--since SEQ] [--dir DIR]
//!   grove-probe read [--since SEQ | --last N] [--dir DIR]
//!   grove-probe facts [--dir DIR]
//!   grove-probe status [--dir DIR]
//!   grove-probe clock
//!   grove-probe version [--json]

mod agents;
mod facts;
#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
mod sampler;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use grove_probe::{Header, Ring, facts_frame};

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

fn fail(message: &str) -> ! {
    eprintln!("grove-probe: {message}");
    std::process::exit(1);
}

struct Args {
    command: String,
    dir: PathBuf,
    since: Option<u64>,
    last: Option<u64>,
    json: bool,
}

fn parse(argv: &[String]) -> Args {
    let mut args = Args {
        command: argv.first().cloned().unwrap_or_default(),
        dir: std::env::var_os("GROVE_PROBE_DIR").map_or_else(
            || PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".grove-probe"),
            PathBuf::from,
        ),
        since: None,
        last: None,
        json: false,
    };
    let mut rest = argv.iter().skip(1);
    while let Some(arg) = rest.next() {
        let mut value = || {
            rest.next()
                .cloned()
                .unwrap_or_else(|| fail(&format!("{arg} needs a value")))
        };
        match arg.as_str() {
            "--dir" => args.dir = PathBuf::from(value()),
            "--since" => {
                args.since = Some(
                    value()
                        .parse()
                        .unwrap_or_else(|_| fail("--since takes a number")),
                )
            }
            "--last" => {
                args.last = Some(
                    value()
                        .parse()
                        .unwrap_or_else(|_| fail("--last takes a number")),
                )
            }
            "--json" => args.json = true,
            other => fail(&format!("unknown argument {other}")),
        }
    }
    args
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse(&argv);
    match args.command.as_str() {
        "run" => run(&args.dir),
        "follow" => stream(&args, true),
        "read" => stream(&args, false),
        "facts" => print!(
            "{}",
            std::fs::read_to_string(args.dir.join("facts")).unwrap_or_default()
        ),
        "status" => status(&args.dir),
        // This machine's clock, for the collector to measure its offset.
        "clock" => println!("{}", now_ms()),
        "version" => version(args.json),
        _ => fail("usage: grove-probe run|follow|read|facts|status|version [--dir DIR]"),
    }
}

fn version(json: bool) {
    let target = format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH);
    if json {
        println!("{{\"name\":\"grove-probe\",\"target\":\"{target}\",\"version\":\"{VERSION}\"}}");
    } else {
        println!("grove-probe {VERSION} {target}");
    }
}

fn status(dir: &Path) {
    let ring =
        Ring::open(&dir.join("ring")).unwrap_or_else(|error| fail(&format!("no ring: {error}")));
    let header = ring
        .header()
        .unwrap_or_else(|error| fail(&error.to_string()));
    let pid = std::fs::read_to_string(dir.join("pid")).unwrap_or_default();
    println!(
        "last_seq={}\nring_id={}\nprobe_cpu_us={}\nprobe_rss_kb={}\npid={}",
        header.last_seq,
        header.ring_id,
        header.probe_cpu_us,
        header.probe_rss_kb,
        pid.trim()
    );
}

/// This process's CPU time (with children it waited for) in microseconds,
/// and its peak resident memory in kB.
fn own_usage() -> (u64, u64) {
    let time = |usage: &libc::rusage| {
        (usage.ru_utime.tv_sec + usage.ru_stime.tv_sec) as u64 * 1_000_000
            + (usage.ru_utime.tv_usec + usage.ru_stime.tv_usec) as u64
    };
    // SAFETY: getrusage fills the struct it is given.
    let (own, children) = unsafe {
        let mut own: libc::rusage = std::mem::zeroed();
        let mut children: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut own);
        libc::getrusage(libc::RUSAGE_CHILDREN, &mut children);
        (own, children)
    };
    let peak = own.ru_maxrss as u64;
    let peak_kb = if cfg!(target_os = "macos") {
        peak / 1024
    } else {
        peak
    };
    (time(&own) + time(&children), peak_kb)
}

fn run(dir: &Path) {
    std::fs::create_dir_all(dir)
        .unwrap_or_else(|error| fail(&format!("{}: {error}", dir.display())));
    let ring_id = (now_ms() as u64) ^ (u64::from(std::process::id()) << 32);
    let (ring, mut header) = Ring::open_writer(&dir.join("ring"), ring_id)
        .unwrap_or_else(|error| fail(&format!("ring: {error}")));
    let _ = std::fs::write(dir.join("pid"), format!("{}\n", std::process::id()));
    let mut sampler = sampler::Sampler::new();
    let mut facts = facts::Facts::new(dir);
    let interval = i64::from(header.interval_ms);
    let ring_path = dir.join("ring");
    loop {
        // A probe whose folder was removed has nothing left to do.
        if header.last_seq % 30 == 0 && !ring_path.exists() {
            std::process::exit(0);
        }
        // Readings land on the interval's boundaries, so a slow reading
        // never pushes the next one later.
        let now = now_ms();
        let next = (now / interval + 1) * interval;
        std::thread::sleep(Duration::from_millis((next - now) as u64));
        let taken_at = now_ms();
        let generation = facts.tick(taken_at);
        let mut record = sampler.sample(header.last_seq + 1, taken_at);
        record.facts_gen = generation;
        (header.probe_cpu_us, header.probe_rss_kb) = own_usage();
        if let Err(error) = ring.append(&mut header, &record) {
            fail(&format!("ring: {error}"));
        }
    }
}

/// `read` writes the header, the facts and every reading after `since`,
/// then ends; `follow` keeps writing readings as they land and the facts
/// whenever they change, until the reader goes away.
fn stream(args: &Args, follow: bool) {
    let path = args.dir.join("ring");
    let ring = Ring::open(&path)
        .unwrap_or_else(|error| fail(&format!("no ring at {}: {error}", path.display())));
    let first = ring
        .header()
        .unwrap_or_else(|error| fail(&error.to_string()));
    let mut since = match (args.since, args.last) {
        (_, Some(last)) => first.last_seq.saturating_sub(last),
        (Some(since), None) => since,
        (None, None) => first.last_seq,
    };
    let mut out = std::io::stdout().lock();
    let mut write = |bytes: &[u8]| {
        if out.write_all(bytes).is_err() {
            std::process::exit(0);
        }
    };
    write(&first.encode());
    let mut facts_gen: Option<u16> = None;
    let mut waiter = if follow {
        Some(Waiter::new(&ring, &path))
    } else {
        None
    };
    loop {
        let header: Header = ring
            .header()
            .unwrap_or_else(|error| fail(&error.to_string()));
        if header.ring_id != first.ring_id {
            fail("the ring was replaced");
        }
        let records = ring.read_since(&header, since).unwrap_or_default();
        if facts_gen.is_none() {
            let text = std::fs::read_to_string(args.dir.join("facts")).unwrap_or_default();
            write(&facts_frame(&text));
            facts_gen = Some(records.first().map_or(u16::MAX, |record| record.facts_gen));
        }
        for record in &records {
            if facts_gen != Some(record.facts_gen) {
                let text = std::fs::read_to_string(args.dir.join("facts")).unwrap_or_default();
                write(&facts_frame(&text));
                facts_gen = Some(record.facts_gen);
            }
            write(&record.encode());
            since = record.seq;
        }
        let _ = std::io::stdout().flush();
        match &mut waiter {
            // A reader that left, or a ring that was removed, ends the
            // stream.
            Some(waiter) => {
                if waiter.wait(Duration::from_secs(30)) || !path.exists() {
                    return;
                }
            }
            None => return,
        }
    }
}

/// Sleeps until the ring file is written, without polling: inotify on
/// Linux, kqueue on macOS. It also watches stdout, so a follower whose
/// reader went away (the SSH session ended) notices at once and exits
/// instead of waiting for the next reading to fail.
struct Waiter {
    fd: i32,
}

impl Waiter {
    /// True when the reader went away.
    fn wait(&mut self, limit: Duration) -> bool {
        let mut fds = [
            libc::pollfd {
                fd: self.fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: 1,
                events: 0,
                revents: 0,
            },
        ];
        // SAFETY: two pollfds, valid for the call.
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, limit.as_millis() as i32) };
        if ready > 0 && fds[1].revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            return true;
        }
        if ready > 0 && fds[0].revents & libc::POLLIN != 0 {
            self.drain();
        }
        false
    }
}

#[cfg(target_os = "linux")]
impl Waiter {
    fn new(_ring: &Ring, path: &Path) -> Self {
        let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap_or_default();
        // SAFETY: plain inotify calls on a path we own.
        let fd = unsafe {
            let fd = libc::inotify_init1(libc::IN_CLOEXEC);
            libc::inotify_add_watch(fd, name.as_ptr(), libc::IN_MODIFY);
            fd
        };
        Self { fd }
    }

    fn drain(&mut self) {
        let mut buffer = [0_u8; 4096];
        // SAFETY: reads queued events into the buffer.
        unsafe { libc::read(self.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    }
}

#[cfg(target_os = "macos")]
impl Waiter {
    fn new(ring: &Ring, _path: &Path) -> Self {
        use std::os::fd::AsRawFd;
        // SAFETY: registers one vnode filter on the ring's descriptor.
        let fd = unsafe {
            let queue = libc::kqueue();
            let change = libc::kevent {
                ident: ring.file().as_raw_fd() as usize,
                filter: libc::EVFILT_VNODE,
                flags: libc::EV_ADD | libc::EV_CLEAR,
                fflags: libc::NOTE_WRITE | libc::NOTE_EXTEND,
                data: 0,
                udata: std::ptr::null_mut(),
            };
            libc::kevent(queue, &change, 1, std::ptr::null_mut(), 0, std::ptr::null());
            queue
        };
        Self { fd }
    }

    fn drain(&mut self) {
        let none = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: collects pending events into a zeroed struct without waiting.
        unsafe {
            let mut event: libc::kevent = std::mem::zeroed();
            libc::kevent(self.fd, std::ptr::null(), 0, &mut event, 1, &none);
        }
    }
}

/// The machine's facts, as `key=value` lines.
#[cfg(target_os = "linux")]
fn platform_facts() -> String {
    let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
    let pretty = os_release
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|value| value.trim_matches('"').to_owned());
    // SAFETY: uname fills the struct.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut name) };
    let field = |bytes: &[libc::c_char]| {
        // SAFETY: uname's fields are NUL-terminated.
        unsafe { std::ffi::CStr::from_ptr(bytes.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    let model = std::fs::read("/sys/class/dmi/id/product_name")
        .or_else(|_| std::fs::read("/sys/firmware/devicetree/base/model"))
        .ok()
        .map(|bytes| {
            String::from_utf8_lossy(&bytes)
                .replace('\0', "")
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned()
        });
    let cpuinfo = std::fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
    let chip = cpuinfo.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        matches!(key.trim(), "model name" | "Model").then(|| value.trim().to_owned())
    });
    let (gpu, gpu_memory) = linux_gpu();
    facts::lines(&[
        ("hostname", Some(field(&name.nodename))),
        ("os", Some("Linux".into())),
        ("arch", Some(field(&name.machine))),
        ("os_version", pretty.or_else(|| Some(field(&name.release)))),
        ("model", model),
        ("chip", chip),
        ("gpu_name", gpu),
        ("gpu_mem_total_mb", gpu_memory),
        ("ip", default_route_address()),
    ])
}

/// The GPU's name (and memory, for NVIDIA), read at start only.
#[cfg(target_os = "linux")]
fn linux_gpu() -> (Option<String>, Option<String>) {
    if let Some(text) = facts::run_text(
        "nvidia-smi",
        &[
            "--query-gpu=name,memory.total",
            "--format=csv,noheader,nounits",
        ],
    ) {
        let mut fields = text.lines().next().unwrap_or_default().split(", ");
        return (
            fields.next().map(str::to_owned),
            fields.next().map(str::to_owned),
        );
    }
    let Ok(cards) = std::fs::read_dir("/sys/class/drm") else {
        return (None, None);
    };
    let mut cards: Vec<PathBuf> = cards.flatten().map(|entry| entry.path()).collect();
    cards.sort();
    for card in cards {
        let vendor = std::fs::read_to_string(card.join("device/vendor")).unwrap_or_default();
        let (brand, pattern) = match vendor.trim() {
            "0x1002" => ("AMD", ["AMD", "ATI"]),
            "0x10de" => ("NVIDIA", ["NVIDIA", "NVIDIA"]),
            _ => continue,
        };
        let named = facts::run_text("lspci", &["-mm"]).and_then(|text| {
            text.lines().find_map(|line| {
                let fields: Vec<&str> = line.split("\" \"").collect();
                let class = fields.first()?.to_lowercase();
                let vendor = fields.get(1)?;
                if !(class.contains("vga") || class.contains("3d") || class.contains("display"))
                    || !pattern.iter().any(|name| vendor.contains(name))
                {
                    return None;
                }
                let device = fields.get(2)?.split('"').next()?;
                let inner = device
                    .find('[')
                    .zip(device.find(']'))
                    .map(|(open, close)| &device[open + 1..close]);
                Some(format!("{brand} {}", inner.unwrap_or(device)))
            })
        });
        return (Some(named.unwrap_or_else(|| format!("{brand} GPU"))), None);
    }
    (None, None)
}

/// The IPv4 address of the interface the default route leaves through.
#[cfg(target_os = "linux")]
fn default_route_address() -> Option<String> {
    let routes = std::fs::read_to_string("/proc/net/route").ok()?;
    let interface = routes.lines().skip(1).find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        (fields.get(1) == Some(&"00000000")).then(|| fields[0].to_owned())
    })?;
    interface_address(&interface)
}

fn interface_address(interface: &str) -> Option<String> {
    let mut found = None;
    // SAFETY: walks the list getifaddrs returns and frees it.
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return None;
        }
        let mut entry = list;
        while !entry.is_null() {
            let item = &*entry;
            let name = std::ffi::CStr::from_ptr(item.ifa_name).to_string_lossy();
            if name == interface
                && !item.ifa_addr.is_null()
                && i32::from((*item.ifa_addr).sa_family) == libc::AF_INET
            {
                let address = &*item.ifa_addr.cast::<libc::sockaddr_in>();
                found = Some(
                    std::net::Ipv4Addr::from(u32::from_be(address.sin_addr.s_addr)).to_string(),
                );
                break;
            }
            entry = item.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    found
}

#[cfg(target_os = "macos")]
fn platform_facts() -> String {
    // SAFETY: uname fills the struct.
    let mut name: libc::utsname = unsafe { std::mem::zeroed() };
    unsafe { libc::uname(&mut name) };
    let field = |bytes: &[libc::c_char]| {
        // SAFETY: uname's fields are NUL-terminated.
        unsafe { std::ffi::CStr::from_ptr(bytes.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    };
    let arch = field(&name.machine);
    let chip = macos::sysctl_text(c"machdep.cpu.brand_string");
    // The interface the default route leaves through, asked once per
    // facts refresh.
    let interface = facts::run_text("route", &["-n", "get", "default"]).and_then(|text| {
        text.lines().find_map(|line| {
            line.trim()
                .strip_prefix("interface:")
                .map(|value| value.trim().to_owned())
        })
    });
    facts::lines(&[
        ("hostname", Some(field(&name.nodename))),
        ("os", Some("Darwin".into())),
        ("arch", Some(arch.clone())),
        ("os_version", macos::sysctl_text(c"kern.osproductversion")),
        ("model", macos::sysctl_text(c"hw.model")),
        (
            "product_name",
            macos::registry_text(c"IODeviceTree:/product", c"product-name"),
        ),
        ("chip", chip.clone()),
        ("gpu_name", if arch == "arm64" { chip } else { None }),
        (
            "ip",
            interface_address(interface.as_deref().unwrap_or("en0")),
        ),
    ])
}
