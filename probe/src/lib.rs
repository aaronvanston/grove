//! The probe's wire and file format: fixed-size little-endian readings,
//! and the ring file that holds the last hour of them.
//!
//! A reading is 128 bytes. Its sequence number is written at both ends, so
//! a reader that catches a slot half-written sees the two disagree and
//! stops there. A value a machine can't give is a sentinel (all ones, or
//! the minimum for signed fields), never zero.
//!
//! The ring file is a 64-byte header followed by `capacity` slots. The
//! writer fills slot `(seq - 1) % capacity` and then publishes `seq` in the
//! header, so `last_seq` never points past a complete reading. A stream
//! (`follow`) is the header followed by readings, back to back.

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
use std::path::Path;

pub const RECORD_SIZE: usize = 128;
pub const HEADER_SIZE: usize = 64;
pub const MAGIC: &[u8; 8] = b"GRVPRING";
pub const FORMAT_VERSION: u32 = 1;
/// Two seconds between readings and one hour kept.
pub const INTERVAL_MS: u32 = 2000;
pub const CAPACITY: u32 = 1800;

pub const NONE_U16: u16 = u16::MAX;
pub const NONE_U32: u32 = u32::MAX;
pub const NONE_U64: u64 = u64::MAX;
pub const NONE_I16: i16 = i16::MIN;
pub const NONE_U8: u8 = u8::MAX;

/// Battery states as pmset and Linux name them (lowercased), indexed by
/// the byte stored.
pub const BATTERY_STATES: [&str; 9] = [
    "",
    "charging",
    "discharging",
    "charged",
    "ac attached",
    "finishing charge",
    "full",
    "not charging",
    "unknown",
];

/// One reading. Percentages and temperatures are tenths, loads hundredths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub seq: u64,
    /// The machine's own clock, in milliseconds since the epoch.
    pub taken_at_ms: i64,
    pub cores: u16,
    /// Busy share since the previous reading; NONE on the first.
    pub cpu_pct_x10: u16,
    pub load1_x100: u16,
    pub load5_x100: u16,
    pub load15_x100: u16,
    /// Bumped whenever the machine's facts or config state change.
    pub facts_gen: u16,
    pub uptime_s: u32,
    pub mem_total_kb: u64,
    pub mem_available_kb: u64,
    pub swap_total_kb: u64,
    pub swap_used_kb: u64,
    pub disk_total_kb: u64,
    pub disk_used_kb: u64,
    pub net_rx_bytes: u64,
    pub net_tx_bytes: u64,
    pub cpu_temp_x10: i16,
    pub gpu_temp_x10: i16,
    pub gpu_util_x10: u16,
    pub battery_pct: u8,
    pub battery_state: u8,
    pub gpu_mem_used_mb: u32,
    pub claude: u16,
    pub codex: u16,
    /// What the agent sessions use together: CPU in tenths of a core's
    /// percent, and resident memory.
    pub agent_cpu_x10: u16,
    pub agent_rss_mb: u32,
}

impl Default for Record {
    fn default() -> Self {
        Self {
            seq: 0,
            taken_at_ms: 0,
            cores: 1,
            cpu_pct_x10: NONE_U16,
            load1_x100: 0,
            load5_x100: 0,
            load15_x100: 0,
            facts_gen: 0,
            uptime_s: NONE_U32,
            mem_total_kb: 0,
            mem_available_kb: 0,
            swap_total_kb: NONE_U64,
            swap_used_kb: NONE_U64,
            disk_total_kb: 0,
            disk_used_kb: 0,
            net_rx_bytes: 0,
            net_tx_bytes: 0,
            cpu_temp_x10: NONE_I16,
            gpu_temp_x10: NONE_I16,
            gpu_util_x10: NONE_U16,
            battery_pct: NONE_U8,
            battery_state: 0,
            gpu_mem_used_mb: NONE_U32,
            claude: NONE_U16,
            codex: NONE_U16,
            agent_cpu_x10: NONE_U16,
            agent_rss_mb: NONE_U32,
        }
    }
}

macro_rules! put {
    ($bytes:ident, $at:expr, $value:expr) => {{
        let value = $value.to_le_bytes();
        $bytes[$at..$at + value.len()].copy_from_slice(&value);
    }};
}

macro_rules! get {
    ($bytes:ident, $at:expr, $type:ty) => {{
        const SIZE: usize = std::mem::size_of::<$type>();
        let mut value = [0_u8; SIZE];
        value.copy_from_slice(&$bytes[$at..$at + SIZE]);
        <$type>::from_le_bytes(value)
    }};
}

impl Record {
    pub fn encode(&self) -> [u8; RECORD_SIZE] {
        let mut b = [0_u8; RECORD_SIZE];
        put!(b, 0, self.seq);
        put!(b, 8, self.taken_at_ms);
        put!(b, 16, self.cores);
        put!(b, 18, self.cpu_pct_x10);
        put!(b, 20, self.load1_x100);
        put!(b, 22, self.load5_x100);
        put!(b, 24, self.load15_x100);
        put!(b, 26, self.facts_gen);
        put!(b, 28, self.uptime_s);
        put!(b, 32, self.mem_total_kb);
        put!(b, 40, self.mem_available_kb);
        put!(b, 48, self.swap_total_kb);
        put!(b, 56, self.swap_used_kb);
        put!(b, 64, self.disk_total_kb);
        put!(b, 72, self.disk_used_kb);
        put!(b, 80, self.net_rx_bytes);
        put!(b, 88, self.net_tx_bytes);
        put!(b, 96, self.cpu_temp_x10);
        put!(b, 98, self.gpu_temp_x10);
        put!(b, 100, self.gpu_util_x10);
        b[102] = self.battery_pct;
        b[103] = self.battery_state;
        put!(b, 104, self.gpu_mem_used_mb);
        put!(b, 108, self.claude);
        put!(b, 110, self.codex);
        put!(b, 112, self.agent_cpu_x10);
        put!(b, 116, self.agent_rss_mb);
        put!(b, 120, self.seq);
        b
    }

    /// None when the slot is torn (its two sequence numbers disagree) or
    /// empty.
    pub fn decode(b: &[u8]) -> Option<Self> {
        if b.len() < RECORD_SIZE {
            return None;
        }
        let seq = get!(b, 0, u64);
        if seq == 0 || seq != get!(b, 120, u64) {
            return None;
        }
        Some(Self {
            seq,
            taken_at_ms: get!(b, 8, i64),
            cores: get!(b, 16, u16),
            cpu_pct_x10: get!(b, 18, u16),
            load1_x100: get!(b, 20, u16),
            load5_x100: get!(b, 22, u16),
            load15_x100: get!(b, 24, u16),
            facts_gen: get!(b, 26, u16),
            uptime_s: get!(b, 28, u32),
            mem_total_kb: get!(b, 32, u64),
            mem_available_kb: get!(b, 40, u64),
            swap_total_kb: get!(b, 48, u64),
            swap_used_kb: get!(b, 56, u64),
            disk_total_kb: get!(b, 64, u64),
            disk_used_kb: get!(b, 72, u64),
            net_rx_bytes: get!(b, 80, u64),
            net_tx_bytes: get!(b, 88, u64),
            cpu_temp_x10: get!(b, 96, i16),
            gpu_temp_x10: get!(b, 98, i16),
            gpu_util_x10: get!(b, 100, u16),
            battery_pct: b[102],
            battery_state: b[103],
            gpu_mem_used_mb: get!(b, 104, u32),
            claude: get!(b, 108, u16),
            codex: get!(b, 110, u16),
            agent_cpu_x10: get!(b, 112, u16),
            agent_rss_mb: get!(b, 116, u32),
        })
    }
}

/// The ring file's header, which also opens every stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub capacity: u32,
    pub interval_ms: u32,
    /// The newest complete reading.
    pub last_seq: u64,
    /// Chosen when the ring file is created; a new ring starts seq again,
    /// and a reader that sees another id knows its position is void.
    pub ring_id: u64,
    /// The probe's own CPU time (itself and the children it waited for),
    /// in microseconds, as of the newest reading.
    pub probe_cpu_us: u64,
    /// The probe's resident memory at the newest reading.
    pub probe_rss_kb: u64,
}

impl Header {
    pub fn encode(&self) -> [u8; HEADER_SIZE] {
        let mut b = [0_u8; HEADER_SIZE];
        b[..8].copy_from_slice(MAGIC);
        put!(b, 8, FORMAT_VERSION);
        put!(b, 12, RECORD_SIZE as u32);
        put!(b, 16, self.capacity);
        put!(b, 20, self.interval_ms);
        put!(b, 24, self.last_seq);
        put!(b, 32, self.ring_id);
        put!(b, 40, self.probe_cpu_us);
        put!(b, 48, self.probe_rss_kb);
        b
    }

    pub fn decode(b: &[u8]) -> io::Result<Self> {
        let invalid = |what: &str| io::Error::new(io::ErrorKind::InvalidData, what.to_owned());
        if b.len() < HEADER_SIZE || &b[..8] != MAGIC {
            return Err(invalid("not a grove-probe ring"));
        }
        if get!(b, 8, u32) != FORMAT_VERSION || get!(b, 12, u32) as usize != RECORD_SIZE {
            return Err(invalid("a ring in another format version"));
        }
        let capacity = get!(b, 16, u32);
        if capacity == 0 {
            return Err(invalid("a ring with no slots"));
        }
        Ok(Self {
            capacity,
            interval_ms: get!(b, 20, u32),
            last_seq: get!(b, 24, u64),
            ring_id: get!(b, 32, u64),
            probe_cpu_us: get!(b, 40, u64),
            probe_rss_kb: get!(b, 48, u64),
        })
    }
}

/// The ring file, opened for reading or for the one writer.
pub struct Ring {
    file: File,
}

fn slot_offset(capacity: u32, seq: u64) -> u64 {
    HEADER_SIZE as u64 + ((seq - 1) % u64::from(capacity)) * RECORD_SIZE as u64
}

impl Ring {
    pub fn open(path: &Path) -> io::Result<Self> {
        Ok(Self {
            file: File::open(path)?,
        })
    }

    /// Opens the ring for writing, creating it (with a fresh id) when it
    /// is missing or in another format.
    pub fn open_writer(path: &Path, ring_id: u64) -> io::Result<(Self, Header)> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let mut existing = [0_u8; HEADER_SIZE];
        if let Ok(header) = file
            .read_exact_at(&mut existing, 0)
            .and_then(|()| Header::decode(&existing))
            && header.capacity == CAPACITY
        {
            return Ok((Self { file }, header));
        }
        let header = Header {
            capacity: CAPACITY,
            interval_ms: INTERVAL_MS,
            last_seq: 0,
            ring_id,
            probe_cpu_us: 0,
            probe_rss_kb: 0,
        };
        // Truncating first zeroes every slot, so nothing from an older
        // ring can read as a reading of this one.
        file.set_len(0)?;
        file.set_len(HEADER_SIZE as u64 + u64::from(CAPACITY) * RECORD_SIZE as u64)?;
        file.write_all_at(&header.encode(), 0)?;
        Ok((Self { file }, header))
    }

    pub fn header(&self) -> io::Result<Header> {
        let mut bytes = [0_u8; HEADER_SIZE];
        self.file.read_exact_at(&mut bytes, 0)?;
        Header::decode(&bytes)
    }

    /// Writes the reading into its slot, then publishes it in the header.
    pub fn append(&self, header: &mut Header, record: &Record) -> io::Result<()> {
        self.file
            .write_all_at(&record.encode(), slot_offset(header.capacity, record.seq))?;
        header.last_seq = record.seq;
        self.file.write_all_at(&header.encode(), 0)
    }

    /// Readings after `since` up to the newest, oldest first, skipping
    /// what the ring no longer holds. Stops at a torn slot.
    pub fn read_since(&self, header: &Header, since: u64) -> io::Result<Vec<Record>> {
        let oldest = header.last_seq.saturating_sub(u64::from(header.capacity)) + 1;
        let first = (since + 1).max(oldest);
        let mut records = Vec::new();
        let mut bytes = [0_u8; RECORD_SIZE];
        for seq in first..=header.last_seq {
            self.file
                .read_exact_at(&mut bytes, slot_offset(header.capacity, seq))?;
            match Record::decode(&bytes) {
                Some(record) if record.seq == seq => records.push(record),
                _ => break,
            }
        }
        Ok(records)
    }

    pub fn file(&self) -> &File {
        &self.file
    }
}

/// What a stream carries after its header: readings, and the machine's
/// facts (as `key=value` lines) whenever they changed. A facts frame
/// starts with eight zero bytes, where a reading starts with its nonzero
/// sequence number, then a four-byte length and the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Reading(Record),
    Facts(String),
}

pub fn facts_frame(text: &str) -> Vec<u8> {
    let mut frame = vec![0_u8; 8];
    frame.extend_from_slice(&(text.len() as u32).to_le_bytes());
    frame.extend_from_slice(text.as_bytes());
    frame
}

/// Reads a stream: its header, then frames until the stream ends.
pub struct StreamReader<R: Read> {
    input: R,
}

impl<R: Read> StreamReader<R> {
    pub fn new(mut input: R) -> io::Result<(Self, Header)> {
        let mut bytes = [0_u8; HEADER_SIZE];
        input.read_exact(&mut bytes)?;
        Ok((Self { input }, Header::decode(&bytes)?))
    }

    /// The next frame; None at the end of the stream.
    pub fn next_frame(&mut self) -> io::Result<Option<Frame>> {
        let mut bytes = [0_u8; RECORD_SIZE];
        match self.input.read_exact(&mut bytes[..8]) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(error) => return Err(error),
        }
        if bytes[..8] == [0; 8] {
            let mut length = [0_u8; 4];
            self.input.read_exact(&mut length)?;
            let length = u32::from_le_bytes(length) as usize;
            if length > 64 * 1024 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "a facts frame too large",
                ));
            }
            let mut text = vec![0_u8; length];
            self.input.read_exact(&mut text)?;
            return Ok(Some(Frame::Facts(
                String::from_utf8_lossy(&text).into_owned(),
            )));
        }
        self.input.read_exact(&mut bytes[8..])?;
        Record::decode(&bytes)
            .map(|record| Some(Frame::Reading(record)))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "a torn reading in the stream")
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field survives the trip, and a slot whose two sequence
    /// numbers disagree reads as torn.
    #[test]
    fn readings_round_trip_and_torn_slots_are_refused() {
        let record = Record {
            seq: 42,
            taken_at_ms: 1_791_343_943_100,
            cores: 24,
            cpu_pct_x10: 75,
            swap_total_kb: 33_554_424,
            swap_used_kb: 8_171_072,
            cpu_temp_x10: -5,
            battery_pct: 100,
            battery_state: 3,
            claude: 2,
            codex: 1,
            ..Record::default()
        };
        let bytes = record.encode();
        assert_eq!(Record::decode(&bytes), Some(record));
        let mut torn = bytes;
        torn[120] ^= 1;
        assert_eq!(Record::decode(&torn), None);
        assert_eq!(Record::decode(&[0; RECORD_SIZE]), None, "an empty slot");
    }

    /// Past one lap the ring holds only the newest `capacity` readings, and
    /// a reader asking from before them starts at the oldest it still has.
    #[test]
    fn the_ring_keeps_the_last_lap_and_reads_from_any_position() {
        let dir = std::env::temp_dir().join(format!("grove-probe-ring-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let path = dir.join("ring");
        let (ring, mut header) = Ring::open_writer(&path, 7).expect("ring");
        for seq in 1..=u64::from(CAPACITY) + 5 {
            let record = Record {
                seq,
                taken_at_ms: seq as i64,
                ..Record::default()
            };
            ring.append(&mut header, &record).expect("append");
        }
        let reader = Ring::open(&path).expect("open");
        let header = reader.header().expect("header");
        assert_eq!(
            (header.last_seq, header.ring_id),
            (u64::from(CAPACITY) + 5, 7)
        );
        let all = reader.read_since(&header, 0).expect("read");
        assert_eq!(all.len(), CAPACITY as usize);
        assert_eq!(all.first().map(|record| record.seq), Some(6));
        let tail = reader
            .read_since(&header, u64::from(CAPACITY) + 2)
            .expect("read");
        assert_eq!(
            tail.iter().map(|record| record.seq).collect::<Vec<_>>(),
            [1803, 1804, 1805]
        );
        // Reopening keeps the ring and its position.
        let (_, reopened) = Ring::open_writer(&path, 9).expect("reopen");
        assert_eq!(
            (reopened.last_seq, reopened.ring_id),
            (u64::from(CAPACITY) + 5, 7)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stream's frames come back as they were sent.
    #[test]
    fn a_stream_carries_readings_and_facts() {
        let header = Header {
            capacity: CAPACITY,
            interval_ms: INTERVAL_MS,
            last_seq: 2,
            ring_id: 3,
            probe_cpu_us: 0,
            probe_rss_kb: 0,
        };
        let mut wire = header.encode().to_vec();
        wire.extend(facts_frame("hostname=cedar-01\n"));
        wire.extend(
            Record {
                seq: 2,
                ..Record::default()
            }
            .encode(),
        );
        let (mut reader, read_header) = StreamReader::new(wire.as_slice()).expect("header");
        assert_eq!(read_header, header);
        assert_eq!(
            reader.next_frame().unwrap(),
            Some(Frame::Facts("hostname=cedar-01\n".into()))
        );
        assert!(
            matches!(reader.next_frame().unwrap(), Some(Frame::Reading(record)) if record.seq == 2)
        );
        assert_eq!(reader.next_frame().unwrap(), None);
    }
}
