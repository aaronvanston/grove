//! Network latency to a remote machine: three ICMP pings to the host SSH
//! resolves its endpoint to (read from `ssh -G`, so aliases, `user@`
//! endpoints and HostName overrides reach the same machine the samples
//! do), and the median of the replies. Skipped when a jump host or proxy
//! command sits in between, where a direct ping would measure another path.

use std::net::IpAddr;
use std::process::Command;
use std::time::Duration;

use crate::output::round1;
use crate::store::Machine;
use crate::transport::{run, ssh_prefix};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ping {
    pub target: Option<String>,
    pub address: Option<String>,
    pub latency_ms: Option<f64>,
}

/// Resolves the machine's ping target and pings it.
pub fn probe(machine: &Machine) -> Ping {
    let prefix = ssh_prefix();
    let mut resolve = Command::new(&prefix[0]);
    resolve.args(&prefix[1..]).args([
        "-G",
        "-p",
        &machine.port.to_string(),
        "--",
        machine.endpoint.trim(),
    ]);
    let config = run(&mut resolve, "ssh", None, Duration::from_secs(5));
    let Some(target) = config
        .ok()
        .then(|| target_from_ssh_config(&config.stdout))
        .flatten()
    else {
        return Ping::default();
    };
    let mut ping = Command::new(if cfg!(target_os = "macos") {
        "/sbin/ping"
    } else {
        "ping"
    });
    // Three pings 0.2 s apart, given up on after 3 s.
    ping.args(["-c", "3", "-i", "0.2"])
        .args(if cfg!(target_os = "macos") {
            ["-t", "3"]
        } else {
            ["-w", "3"]
        })
        .arg(&target)
        .env("LC_ALL", "C");
    let output = run(&mut ping, "ping", None, Duration::from_secs(4));
    let (address, latency_ms) = parse_ping(&target, &output.stdout);
    Ping {
        target: Some(target),
        address: address.map(|address| address.to_string()),
        latency_ms: latency_ms.map(round1),
    }
}

/// The resolved host name from `ssh -G` output, unless a proxy is in the way.
fn target_from_ssh_config(config: &str) -> Option<String> {
    let mut hostname = None;
    for line in config.lines() {
        let Some((key, value)) = line.trim().split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key {
            "hostname" => hostname = Some(value),
            "proxyjump" | "proxycommand" if value != "none" => return None,
            _ => {}
        }
    }
    hostname
        .filter(|name| !name.is_empty() && !name.starts_with('-'))
        .map(str::to_owned)
}

/// The address ping resolved the target to and the median round trip, so
/// one slow reply doesn't read as a spike. No replies leave it unknown.
fn parse_ping(target: &str, output: &str) -> (Option<IpAddr>, Option<f64>) {
    let address = target.parse::<IpAddr>().ok().or_else(|| {
        let header = output.lines().find(|line| line.starts_with("PING "))?;
        let end = header.find(')')?;
        let start = header[..end].rfind('(')? + 1;
        header[start..end].parse().ok()
    });
    let mut times: Vec<f64> = output
        .lines()
        .filter_map(|line| {
            let at = line.find("time=")? + "time=".len();
            let digits: String = line[at..]
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            digits.parse().ok()
        })
        .collect();
    times.sort_by(f64::total_cmp);
    let count = times.len();
    let median = match count {
        0 => None,
        _ if count % 2 == 1 => Some(times[count / 2]),
        _ => Some((times[count / 2 - 1] + times[count / 2]) / 2.0),
    };
    (address, median)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_output_gives_the_address_and_the_median_round_trip() {
        let mac = "PING cedar-01.tailc0ffee.ts.net (100.64.0.21): 56 data bytes\n\
            64 bytes from 100.64.0.21: icmp_seq=0 ttl=64 time=5.733 ms\n\
            64 bytes from 100.64.0.21: icmp_seq=1 ttl=64 time=123.634 ms\n\
            64 bytes from 100.64.0.21: icmp_seq=2 ttl=64 time=6.1 ms\n";
        assert_eq!(
            parse_ping("cedar-01.tailc0ffee.ts.net", mac),
            (Some(IpAddr::from([100, 64, 0, 21])), Some(6.1))
        );
        let linux = "PING cedar-02(fd7a:115c:a1e0::a17 (fd7a:115c:a1e0::a17)) 56 data bytes\n\
            64 bytes from fd7a:115c:a1e0::a17: icmp_seq=1 ttl=64 time=6.00 ms\n\
            64 bytes from fd7a:115c:a1e0::a17: icmp_seq=3 ttl=64 time=7.00 ms\n";
        assert_eq!(
            parse_ping("cedar-02", linux),
            (Some("fd7a:115c:a1e0::a17".parse().unwrap()), Some(6.5))
        );
        let silent = "PING cedar-01 (100.64.0.21): 56 data bytes\nRequest timeout for icmp_seq 0\n";
        assert_eq!(
            parse_ping("cedar-01", silent),
            (Some(IpAddr::from([100, 64, 0, 21])), None)
        );
        assert_eq!(parse_ping("nowhere", ""), (None, None));
    }

    #[test]
    fn ssh_config_names_the_target_unless_a_proxy_sits_in_between() {
        let config =
            "host cedar-01\nhostname cedar-01.tailc0ffee.ts.net\nport 22\nproxycommand none\n";
        assert_eq!(
            target_from_ssh_config(config).as_deref(),
            Some("cedar-01.tailc0ffee.ts.net")
        );
        let jumped = "hostname cedar-01\nproxyjump bastion\n";
        assert_eq!(target_from_ssh_config(jumped), None);
    }
}
