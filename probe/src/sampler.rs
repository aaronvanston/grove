//! One reading every two seconds, read straight from the kernel in this
//! process: no shells, no `ps`, no forks on the sampling path.

use std::time::Instant;

use grove_probe::{NONE_U16, NONE_U32, Record};

use crate::agents::{Agent, Cache, Known};
#[cfg(target_os = "linux")]
use crate::linux as platform;
#[cfg(target_os = "macos")]
use crate::macos as platform;

/// What the platform reads into a reading, and what it remembers between
/// readings (open sensor files, connections).
pub use platform::Platform;

pub struct Sampler {
    platform: Platform,
    previous_cpu: Option<(u64, u64)>,
    agents: Cache,
    previous_at: Option<Instant>,
    listings: u32,
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            platform: Platform::new(),
            previous_cpu: None,
            agents: Cache::default(),
            previous_at: None,
            listings: 0,
        }
    }

    pub fn sample(&mut self, seq: u64, taken_at_ms: i64) -> Record {
        let mut record = Record {
            seq,
            taken_at_ms,
            ..Record::default()
        };
        let now = Instant::now();
        let elapsed_ns = self
            .previous_at
            .map(|at| now.duration_since(at).as_nanos() as u64);
        self.previous_at = Some(now);
        if let Some(ticks) = self.platform.cpu_ticks() {
            if let Some((total, idle)) = self.previous_cpu
                && ticks.0 > total
                && ticks.1 >= idle
            {
                let delta = ticks.0 - total;
                let busy = delta.saturating_sub(ticks.1 - idle);
                record.cpu_pct_x10 = ((busy * 1000 + delta / 2) / delta).min(1000) as u16;
            }
            self.previous_cpu = Some(ticks);
        }
        self.platform.read(&mut record);
        self.count_agents(&mut record, elapsed_ns);
        record
    }

    /// Classifies pids not seen before and sums what the agents use. The
    /// whole process list is read every LIST_EVERY readings, where listing
    /// costs more than the rest of a reading; in between, only the agents
    /// already known are read, and one that has exited is dropped.
    fn count_agents(&mut self, record: &mut Record, elapsed_ns: Option<u64>) {
        let listing = self.listings.is_multiple_of(platform::LIST_EVERY);
        self.listings = self.listings.wrapping_add(1);
        let pids = if listing {
            let Some(pids) = self.platform.pids() else {
                return;
            };
            self.agents.retain(&pids);
            pids
        } else {
            self.agents
                .known
                .iter()
                .filter(|(_, known)| known.agent.is_some())
                .map(|(pid, _)| *pid)
                .collect()
        };
        let (mut claude, mut codex, mut cpu_ns, mut rss_kb) = (0_u16, 0_u16, 0_u64, 0_u64);
        for pid in pids {
            let known = match self.agents.known.get(&pid) {
                Some(known) => *known,
                None => {
                    let agent = self
                        .platform
                        .arguments(pid)
                        .and_then(|line| crate::agents::classify(&line));
                    Known { agent, cpu_ns: 0 }
                }
            };
            let Some(agent) = known.agent else {
                self.agents.known.insert(pid, known);
                continue;
            };
            match agent {
                Agent::Claude => claude += 1,
                Agent::Codex => codex += 1,
            }
            let Some((cpu, rss)) = self.platform.usage(pid) else {
                // Gone since the last listing.
                if !listing {
                    self.agents.known.remove(&pid);
                    match agent {
                        Agent::Claude => claude -= 1,
                        Agent::Codex => codex -= 1,
                    }
                }
                continue;
            };
            if known.cpu_ns > 0 {
                cpu_ns += cpu.saturating_sub(known.cpu_ns);
            }
            let now_ns = cpu;
            rss_kb += rss;
            self.agents.known.insert(
                pid,
                Known {
                    agent: Some(agent),
                    cpu_ns: now_ns,
                },
            );
        }
        let known = &self.agents.known;
        self.platform
            .forget(&|pid| known.get(&pid).is_some_and(|known| known.agent.is_some()));
        record.claude = claude;
        record.codex = codex;
        record.agent_rss_mb = (rss_kb / 1024).min(u64::from(NONE_U32 - 1)) as u32;
        record.agent_cpu_x10 = match elapsed_ns {
            Some(elapsed) if elapsed > 0 => {
                ((cpu_ns * 1000) / elapsed).min(u64::from(NONE_U16 - 1)) as u16
            }
            _ => NONE_U16,
        };
    }
}
