//! macOS readings from Mach host statistics, sysctl, libproc, the SMC and
//! the IORegistry.

use std::ffi::{CStr, c_char, c_void};
use std::mem::{size_of, zeroed};

use grove_probe::{NONE_I16, NONE_U16, Record};

pub type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFArrayRef = *const c_void;
type IoObject = u32;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        allocator: CFTypeRef,
        text: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringGetCString(
        text: CFStringRef,
        buffer: *mut c_char,
        size: isize,
        encoding: u32,
    ) -> bool;
    fn CFDictionaryGetValue(dictionary: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    fn CFNumberGetValue(number: CFTypeRef, kind: isize, value: *mut c_void) -> bool;
    fn CFBooleanGetValue(boolean: CFTypeRef) -> bool;
    fn CFArrayGetCount(array: CFArrayRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: isize) -> CFTypeRef;
    fn CFDataGetLength(data: CFTypeRef) -> isize;
    fn CFDataGetBytePtr(data: CFTypeRef) -> *const u8;
    fn CFRelease(object: CFTypeRef);
}

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOServiceMatching(name: *const c_char) -> CFDictionaryRef;
    fn IOServiceGetMatchingServices(
        port: u32,
        matching: CFDictionaryRef,
        iterator: *mut IoObject,
    ) -> i32;
    fn IOIteratorNext(iterator: IoObject) -> IoObject;
    fn IORegistryEntryGetName(entry: IoObject, name: *mut c_char) -> i32;
    fn IORegistryEntryCreateCFProperty(
        entry: IoObject,
        key: CFStringRef,
        allocator: CFTypeRef,
        options: u32,
    ) -> CFTypeRef;
    fn IORegistryEntryFromPath(port: u32, path: *const c_char) -> IoObject;
    fn IOObjectRelease(object: IoObject) -> i32;
    fn IOServiceOpen(service: IoObject, task: u32, kind: u32, connection: *mut u32) -> i32;
    fn IOConnectCallStructMethod(
        connection: u32,
        selector: u32,
        input: *const c_void,
        input_size: usize,
        output: *mut c_void,
        output_size: *mut usize,
    ) -> i32;
    fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
    fn IOPSCopyPowerSourcesList(info: CFTypeRef) -> CFArrayRef;
    fn IOPSGetPowerSourceDescription(info: CFTypeRef, source: CFTypeRef) -> CFDictionaryRef;
    static mach_task_self_: u32;
}

#[repr(C)]
struct Timebase {
    numer: u32,
    denom: u32,
}

unsafe extern "C" {
    fn mach_host_self() -> u32;
    fn mach_timebase_info(info: *mut Timebase) -> i32;
}

const UTF8: u32 = 0x0800_0100;
const NUMBER_SINT64: isize = 4;

/// A CFString that lives as long as the probe.
fn cfstr(text: &CStr) -> CFStringRef {
    // SAFETY: creates a new string from a NUL-terminated buffer.
    unsafe { CFStringCreateWithCString(std::ptr::null(), text.as_ptr(), UTF8) }
}

fn cf_i64(number: CFTypeRef) -> Option<i64> {
    if number.is_null() {
        return None;
    }
    let mut value = 0_i64;
    // SAFETY: reads a CFNumber into an i64.
    unsafe { CFNumberGetValue(number, NUMBER_SINT64, (&mut value as *mut i64).cast()) }
        .then_some(value)
}

fn sysctl_raw<T>(name: &CStr, value: &mut T) -> bool {
    let mut size = size_of::<T>();
    // SAFETY: the buffer is a T of `size` bytes.
    unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            (value as *mut T).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) == 0
    }
}

pub fn sysctl_text(name: &CStr) -> Option<String> {
    let mut size = 0_usize;
    // SAFETY: the first call sizes the buffer, the second fills it.
    unsafe {
        if libc::sysctlbyname(
            name.as_ptr(),
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
        let mut buffer = vec![0_u8; size];
        if libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        ) != 0
        {
            return None;
        }
        let text = CStr::from_bytes_until_nul(&buffer)
            .ok()?
            .to_string_lossy()
            .trim()
            .to_owned();
        (!text.is_empty()).then_some(text)
    }
}

/// The SMC request and reply, as the AppleSMC user client lays it out.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct KeyInfo {
    size: u32,
    kind: u32,
    attributes: u8,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Version {
    major: u8,
    minor: u8,
    build: u8,
    reserved: u8,
    release: u16,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Limits {
    version: u16,
    length: u16,
    cpu: u32,
    gpu: u32,
    memory: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KeyData {
    key: u32,
    version: Version,
    limits: Limits,
    info: KeyInfo,
    result: u8,
    status: u8,
    command: u8,
    index: u32,
    bytes: [u8; 32],
}

impl Default for KeyData {
    fn default() -> Self {
        // SAFETY: all-zero is a valid request.
        unsafe { zeroed() }
    }
}

const FLOAT: u32 = u32::from_be_bytes(*b"flt ");

/// Temperatures from the SMC, the way macmon reads them: the average of
/// the float sensors whose keys start Tp, Te or Ts (CPU) and Tg (GPU).
struct Smc {
    connection: u32,
    cpu: Vec<(u32, KeyInfo)>,
    gpu: Vec<(u32, KeyInfo)>,
}

impl Smc {
    fn call(&self, request: &KeyData) -> Option<KeyData> {
        let mut reply = KeyData::default();
        let mut size = size_of::<KeyData>();
        // SAFETY: both buffers are KeyData-sized, as the selector expects.
        let status = unsafe {
            IOConnectCallStructMethod(
                self.connection,
                2,
                (request as *const KeyData).cast(),
                size_of::<KeyData>(),
                (&mut reply as *mut KeyData).cast(),
                &mut size,
            )
        };
        (status == 0 && reply.result == 0).then_some(reply)
    }

    fn info(&self, key: u32) -> Option<KeyInfo> {
        Some(
            self.call(&KeyData {
                key,
                command: 9,
                ..KeyData::default()
            })?
            .info,
        )
    }

    fn float(&self, key: u32, info: KeyInfo) -> Option<f32> {
        let reply = self.call(&KeyData {
            key,
            info,
            command: 5,
            ..KeyData::default()
        })?;
        let mut bytes = [0_u8; 4];
        bytes.copy_from_slice(&reply.bytes[..4]);
        Some(f32::from_le_bytes(bytes))
    }

    fn open() -> Option<Self> {
        let mut connection = 0;
        for (service, name) in services(c"AppleSMC") {
            if name == "AppleSMCKeysEndpoint" && connection == 0 {
                // SAFETY: opens a user client on the service just found.
                unsafe { IOServiceOpen(service, mach_task_self_, 0, &mut connection) };
            }
            // SAFETY: each service from the iterator is released once.
            unsafe { IOObjectRelease(service) };
        }
        if connection == 0 {
            return None;
        }
        let mut smc = Self {
            connection,
            cpu: Vec::new(),
            gpu: Vec::new(),
        };
        let count_key = u32::from_be_bytes(*b"#KEY");
        let count_info = smc.info(count_key)?;
        let count = smc.call(&KeyData {
            key: count_key,
            info: count_info,
            command: 5,
            ..KeyData::default()
        })?;
        let count = u32::from_be_bytes([
            count.bytes[0],
            count.bytes[1],
            count.bytes[2],
            count.bytes[3],
        ]);
        for index in 0..count {
            let Some(reply) = smc.call(&KeyData {
                index,
                command: 8,
                ..KeyData::default()
            }) else {
                continue;
            };
            let name = reply.key.to_be_bytes();
            let cpu = name.starts_with(b"Tp") || name.starts_with(b"Te") || name.starts_with(b"Ts");
            let gpu = name.starts_with(b"Tg");
            if !cpu && !gpu {
                continue;
            }
            let Some(info) = smc
                .info(reply.key)
                .filter(|info| info.size == 4 && info.kind == FLOAT)
            else {
                continue;
            };
            if smc.float(reply.key, info).is_none() {
                continue;
            }
            if cpu {
                smc.cpu.push((reply.key, info));
            } else {
                smc.gpu.push((reply.key, info));
            }
        }
        Some(smc)
    }

    fn average(&self, keys: &[(u32, KeyInfo)]) -> i16 {
        let values: Vec<f32> = keys
            .iter()
            .filter_map(|(key, info)| self.float(*key, *info))
            .filter(|value| *value > 0.0 && *value <= 150.0)
            .collect();
        if values.is_empty() {
            return NONE_I16;
        }
        let average = values.iter().sum::<f32>() / values.len() as f32;
        (average * 10.0).round() as i16
    }
}

/// Every IORegistry service matching `class`, with its name.
fn services(class: &CStr) -> Vec<(IoObject, String)> {
    let mut found = Vec::new();
    let mut iterator = 0;
    // SAFETY: matching consumes the dictionary; the iterator is released.
    unsafe {
        if IOServiceGetMatchingServices(0, IOServiceMatching(class.as_ptr()), &mut iterator) != 0 {
            return found;
        }
        loop {
            let service = IOIteratorNext(iterator);
            if service == 0 {
                break;
            }
            let mut name = [0 as c_char; 128];
            IORegistryEntryGetName(service, name.as_mut_ptr());
            found.push((
                service,
                CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned(),
            ));
        }
        IOObjectRelease(iterator);
    }
    found
}

/// A string property of an IORegistry path, such as the product name.
pub fn registry_text(path: &CStr, key: &CStr) -> Option<String> {
    // SAFETY: the entry and the property are released after reading.
    unsafe {
        let entry = IORegistryEntryFromPath(0, path.as_ptr());
        if entry == 0 {
            return None;
        }
        let name = cfstr(key);
        let value = IORegistryEntryCreateCFProperty(entry, name, std::ptr::null(), 0);
        CFRelease(name);
        IOObjectRelease(entry);
        if value.is_null() {
            return None;
        }
        let length = CFDataGetLength(value).max(0) as usize;
        let bytes = std::slice::from_raw_parts(CFDataGetBytePtr(value), length).to_vec();
        CFRelease(value);
        let text = String::from_utf8_lossy(&bytes)
            .trim_end_matches('\0')
            .trim()
            .to_owned();
        (!text.is_empty()).then_some(text)
    }
}

/// New processes are looked for every fifth reading, as on Linux: the
/// list holds a thousand or more pids to check against what is known.
pub const LIST_EVERY: u32 = 5;

/// Temperatures move slowly and each SMC key is a round trip to the
/// controller, so they are read every fifteenth reading (thirty seconds) and the
/// last values carried in between.
const TEMPERATURE_EVERY: u32 = 15;

/// A battery's charge moves slowly, so it is read every fifteenth reading.
const BATTERY_EVERY: u32 = 15;

pub struct Platform {
    /// None once a look found no internal battery; else the reading count
    /// and the last charge and state.
    battery: Option<(u32, u8, u8)>,
    smc: Option<Smc>,
    temperatures: (u32, i16, i16),
    gpu: Option<IoObject>,
    keys: Keys,
    page_size: u64,
    timebase: (u64, u64),
    disk_path: &'static CStr,
}

/// CFString keys made once.
struct Keys {
    statistics: CFStringRef,
    utilization: CFStringRef,
    capacity: CFStringRef,
    max_capacity: CFStringRef,
    charging: CFStringRef,
    charged: CFStringRef,
    finishing: CFStringRef,
    state: CFStringRef,
    kind: CFStringRef,
}

fn text_of(value: CFTypeRef) -> String {
    if value.is_null() {
        return String::new();
    }
    let mut buffer = [0 as c_char; 64];
    // SAFETY: copies at most the buffer's size.
    if unsafe { CFStringGetCString(value, buffer.as_mut_ptr(), buffer.len() as isize, UTF8) } {
        // SAFETY: CFStringGetCString NUL-terminates on success.
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    } else {
        String::new()
    }
}

fn truth(value: CFTypeRef) -> bool {
    // SAFETY: reads a CFBoolean that the dictionary owns.
    !value.is_null() && unsafe { CFBooleanGetValue(value) }
}

impl Platform {
    pub fn new() -> Self {
        let mut timebase = Timebase { numer: 1, denom: 1 };
        // SAFETY: fills the struct.
        unsafe { mach_timebase_info(&mut timebase) };
        let mut page_size = 0_u64;
        sysctl_raw(c"hw.pagesize", &mut page_size);
        let gpu = services(c"IOAccelerator")
            .into_iter()
            .map(|(service, _)| service)
            .next();
        Self {
            battery: Some((0, grove_probe::NONE_U8, 0)),
            smc: Smc::open(),
            temperatures: (0, NONE_I16, NONE_I16),
            gpu,
            keys: Keys {
                statistics: cfstr(c"PerformanceStatistics"),
                utilization: cfstr(c"Device Utilization %"),
                capacity: cfstr(c"Current Capacity"),
                max_capacity: cfstr(c"Max Capacity"),
                charging: cfstr(c"Is Charging"),
                charged: cfstr(c"Is Charged"),
                finishing: cfstr(c"Is Finishing Charge"),
                state: cfstr(c"Power Source State"),
                kind: cfstr(c"Type"),
            },
            page_size: page_size.max(4096),
            timebase: (u64::from(timebase.numer), u64::from(timebase.denom.max(1))),
            disk_path: if std::path::Path::new("/System/Volumes/Data").is_dir() {
                c"/System/Volumes/Data"
            } else {
                c"/"
            },
        }
    }

    /// Total and idle ticks across all CPUs.
    pub fn cpu_ticks(&mut self) -> Option<(u64, u64)> {
        // SAFETY: host_statistics fills a host_cpu_load_info of `count` ints.
        unsafe {
            let mut load: libc::host_cpu_load_info = zeroed();
            let mut count = (size_of::<libc::host_cpu_load_info>() / size_of::<i32>()) as u32;
            if libc::host_statistics(
                mach_host_self(),
                libc::HOST_CPU_LOAD_INFO,
                (&mut load as *mut libc::host_cpu_load_info).cast(),
                &mut count,
            ) != 0
            {
                return None;
            }
            let ticks = load.cpu_ticks.map(u64::from);
            Some((ticks.iter().sum(), ticks[libc::CPU_STATE_IDLE as usize]))
        }
    }

    pub fn read(&mut self, record: &mut Record) {
        let mut cores = 0_i32;
        sysctl_raw(c"hw.logicalcpu", &mut cores);
        record.cores = u16::try_from(cores).unwrap_or(1).max(1);
        let mut loads = [0.0_f64; 3];
        // SAFETY: fills three doubles.
        if unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) } == 3 {
            let scale = |load: f64| (load * 100.0).round().min(f64::from(u16::MAX - 1)) as u16;
            (record.load1_x100, record.load5_x100, record.load15_x100) =
                (scale(loads[0]), scale(loads[1]), scale(loads[2]));
        }
        let mut memory = 0_u64;
        sysctl_raw(c"hw.memsize", &mut memory);
        record.mem_total_kb = memory / 1024;
        // SAFETY: host_statistics64 fills a vm_statistics64 of `count` ints.
        unsafe {
            let mut vm: libc::vm_statistics64 = zeroed();
            let mut count = libc::HOST_VM_INFO64_COUNT;
            if libc::host_statistics64(
                mach_host_self(),
                libc::HOST_VM_INFO64,
                (&mut vm as *mut libc::vm_statistics64).cast(),
                &mut count,
            ) == 0
            {
                let pages = u64::from(vm.free_count)
                    + u64::from(vm.inactive_count)
                    + u64::from(vm.speculative_count);
                record.mem_available_kb = pages * self.page_size / 1024;
            }
        }
        // SAFETY: an all-zero xsw_usage is valid.
        let mut swap: libc::xsw_usage = unsafe { zeroed() };
        if sysctl_raw(c"vm.swapusage", &mut swap) {
            record.swap_total_kb = swap.xsu_total / 1024;
            record.swap_used_kb = swap.xsu_used / 1024;
        }
        // SAFETY: statfs fills the struct.
        let mut disk: libc::statfs = unsafe { zeroed() };
        if unsafe { libc::statfs(self.disk_path.as_ptr(), &mut disk) } == 0 {
            let block = u64::from(disk.f_bsize);
            let total = disk.f_blocks * block / 1024;
            record.disk_total_kb = total;
            record.disk_used_kb = total.saturating_sub(disk.f_bavail * block / 1024);
        }
        (record.net_rx_bytes, record.net_tx_bytes) = network();
        let mut boot: libc::timeval = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        if sysctl_raw(c"kern.boottime", &mut boot) {
            record.uptime_s = (record.taken_at_ms / 1000 - boot.tv_sec).max(0) as u32;
        }
        if let Some(smc) = &self.smc {
            let (count, cpu, gpu) = &mut self.temperatures;
            if count.is_multiple_of(TEMPERATURE_EVERY) {
                (*cpu, *gpu) = (smc.average(&smc.cpu), smc.average(&smc.gpu));
            }
            *count = count.wrapping_add(1);
            (record.cpu_temp_x10, record.gpu_temp_x10) = (*cpu, *gpu);
        }
        record.gpu_util_x10 = self.gpu_utilization();
        if let Some((count, pct, state)) = self.battery {
            if count % BATTERY_EVERY == 0 {
                let mut fresh = Record::default();
                if self.read_battery(&mut fresh) {
                    self.battery = Some((
                        count.wrapping_add(1),
                        fresh.battery_pct,
                        fresh.battery_state,
                    ));
                } else {
                    self.battery = None;
                }
            } else {
                self.battery = Some((count.wrapping_add(1), pct, state));
            }
            if let Some((_, pct, state)) = self.battery {
                (record.battery_pct, record.battery_state) = (pct, state);
            }
        }
    }

    fn gpu_utilization(&self) -> u16 {
        let Some(gpu) = self.gpu else {
            return NONE_U16;
        };
        // SAFETY: the property is released after reading.
        unsafe {
            let statistics =
                IORegistryEntryCreateCFProperty(gpu, self.keys.statistics, std::ptr::null(), 0);
            if statistics.is_null() {
                return NONE_U16;
            }
            let percent = cf_i64(CFDictionaryGetValue(statistics, self.keys.utilization));
            CFRelease(statistics);
            percent.map_or(NONE_U16, |percent| (percent.clamp(0, 100) * 10) as u16)
        }
    }

    /// The internal battery, named as pmset names its state. False when
    /// the machine has none.
    fn read_battery(&self, record: &mut Record) -> bool {
        let mut found = false;
        // SAFETY: the info and list are released; descriptions are owned
        // by the info.
        unsafe {
            let info = IOPSCopyPowerSourcesInfo();
            if info.is_null() {
                return false;
            }
            let list = IOPSCopyPowerSourcesList(info);
            if !list.is_null() {
                for index in 0..CFArrayGetCount(list) {
                    let source =
                        IOPSGetPowerSourceDescription(info, CFArrayGetValueAtIndex(list, index));
                    if source.is_null()
                        || text_of(CFDictionaryGetValue(source, self.keys.kind))
                            != "InternalBattery"
                    {
                        continue;
                    }
                    let current = cf_i64(CFDictionaryGetValue(source, self.keys.capacity));
                    let max = cf_i64(CFDictionaryGetValue(source, self.keys.max_capacity))
                        .filter(|max| *max > 0);
                    if let (Some(current), Some(max)) = (current, max) {
                        record.battery_pct = ((current * 100 + max / 2) / max).clamp(0, 100) as u8;
                    }
                    let on_battery =
                        text_of(CFDictionaryGetValue(source, self.keys.state)) == "Battery Power";
                    let state = if on_battery {
                        "discharging"
                    } else if truth(CFDictionaryGetValue(source, self.keys.finishing)) {
                        "finishing charge"
                    } else if truth(CFDictionaryGetValue(source, self.keys.charging)) {
                        "charging"
                    } else if truth(CFDictionaryGetValue(source, self.keys.charged)) {
                        "charged"
                    } else {
                        "ac attached"
                    };
                    record.battery_state = grove_probe::BATTERY_STATES
                        .iter()
                        .position(|known| *known == state)
                        .unwrap_or(0) as u8;
                    found = true;
                    break;
                }
                CFRelease(list);
            }
            CFRelease(info);
        }
        found
    }

    pub fn pids(&mut self) -> Option<Vec<i32>> {
        // SAFETY: the first call counts, the second fills a buffer with room
        // to spare.
        unsafe {
            let count = libc::proc_listallpids(std::ptr::null_mut(), 0);
            if count <= 0 {
                return None;
            }
            let mut pids = vec![0_i32; count as usize + 64];
            let filled = libc::proc_listallpids(
                pids.as_mut_ptr().cast(),
                (pids.len() * size_of::<i32>()) as i32,
            );
            if filled <= 0 {
                return None;
            }
            pids.truncate(filled as usize);
            pids.retain(|pid| *pid > 0);
            Some(pids)
        }
    }

    /// The process's arguments joined by spaces, as `ps` prints them, from
    /// KERN_PROCARGS2: argc, the executable path, padding, then argv.
    pub fn arguments(&mut self, pid: i32) -> Option<String> {
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        let mut size = 0_usize;
        // SAFETY: the first call sizes the buffer, the second fills it.
        unsafe {
            if libc::sysctl(
                mib.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            ) != 0
            {
                return None;
            }
            let mut buffer = vec![0_u8; size];
            if libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buffer.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            ) != 0
            {
                return None;
            }
            buffer.truncate(size);
            let argc = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?).max(0) as usize;
            let mut rest = &buffer[4..];
            let path_end = rest.iter().position(|byte| *byte == 0)?;
            rest = &rest[path_end..];
            let start = rest.iter().position(|byte| *byte != 0)?;
            let words: Vec<String> = rest[start..]
                .split(|byte| *byte == 0)
                .take(argc)
                .map(|word| String::from_utf8_lossy(word).into_owned())
                .collect();
            let joined = words.join(" ");
            (!joined.is_empty()).then_some(joined)
        }
    }

    /// Nothing is held open per process on macOS.
    pub fn forget(&mut self, _keep: &dyn Fn(i32) -> bool) {}

    /// CPU time in nanoseconds and resident memory in kB.
    pub fn usage(&mut self, pid: i32) -> Option<(u64, u64)> {
        // SAFETY: proc_pidinfo fills a proc_taskinfo.
        let info = unsafe {
            let mut info: libc::proc_taskinfo = zeroed();
            let size = size_of::<libc::proc_taskinfo>() as i32;
            if libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTASKINFO,
                0,
                (&mut info as *mut libc::proc_taskinfo).cast(),
                size,
            ) != size
            {
                return None;
            }
            info
        };
        let ticks = info.pti_total_user + info.pti_total_system;
        Some((
            ticks * self.timebase.0 / self.timebase.1,
            info.pti_resident_size / 1024,
        ))
    }
}

/// `struct ifmibdata` from <net/if_mib.h>: an interface's name, flags and
/// 64-bit counters.
#[repr(C)]
struct InterfaceData {
    name: [u8; 16],
    pcount: u32,
    flags: u32,
    send_length: u32,
    send_max: u32,
    send_drops: u32,
    filler: [u32; 4],
    data: libc::if_data64,
}

/// Bytes in and out since boot over every interface but loopback, read
/// one interface at a time through the interface MIB, which carries the
/// 64-bit counters netstat reports.
fn network() -> (u64, u64) {
    let mut count = 0_i32;
    if !sysctl_raw(c"net.link.generic.system.ifcount", &mut count) {
        return (0, 0);
    }
    let (mut rx, mut tx) = (0, 0);
    for index in 1..=count {
        // CTL_NET, PF_LINK, NETLINK_GENERIC, IFMIB_IFDATA, index, IFDATA_GENERAL
        let mut mib = [libc::CTL_NET, libc::PF_LINK, 0, 2, index, 1];
        // SAFETY: an all-zero ifmibdata is valid, and sysctl fills at most
        // its size.
        let mut data: InterfaceData = unsafe { zeroed() };
        let mut size = size_of::<InterfaceData>();
        let found = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                6,
                (&mut data as *mut InterfaceData).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        } == 0;
        if found && data.flags & libc::IFF_LOOPBACK as u32 == 0 {
            rx += data.data.ifi_ibytes;
            tx += data.data.ifi_obytes;
        }
    }
    (rx, tx)
}
