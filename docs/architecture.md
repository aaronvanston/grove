# Architecture

Grove is two Rust binaries with no runtime dependencies: `grove`, the command line you run, and `grove-probe`, the small sampler that runs on each machine. SQLite is compiled into `grove`; the probe needs nothing but the kernel.

## Modules

```text
src/main.rs           argv -> cli::run -> exit code
src/cli/mod.rs        builds the command tree from the catalog, dispatches, and is the
                      only writer to stdout and stderr
src/cli/commander.rs  the argv parser: options anywhere before --, --opt=value, --no-*,
                      groups, Commander's error wording
src/cli/options.rs    reading parsed option values and their invalid_options errors
src/cli/machines.rs   add, list, label, rm, status
src/cli/readings.rs   sample, show, history, and the sampling round they share
src/cli/views.rs      graph, ps, top, watch
src/cli/probes.rs     probe install|use|uninstall|status, stream
src/cli/alerts.rs     alerts and hooks;  src/cli/policy.rs  policy and drift
src/cli/data.rs       prune, retention;  src/cli/system.rs  doctor, schema, describe, completion
src/catalog.rs        the command catalog (catalog.json): help, schema, describe,
                      completions and the parser tree all read it
src/transport.rs      ssh and sh, timeouts, process groups, bounded parallelism
src/script.rs         the sample script, for machines without a probe
src/probe.rs          probe readings to samples, read-once, and the stream collector
src/reading.rs        a sample, its record, and parsing the script's output
src/health.rs         the score;  src/alerts.rs  window rules;  src/hooks.rs  running hooks
src/policy.rs         eligibility checks;  src/drift.rs  config drift
src/store/            the SQLite store: machines, samples, live readings, alerts, hooks,
                      policy, ordered migrations (schema.rs)
probe/src/lib.rs      the reading format, the ring file and the stream format, shared by
                      both binaries
probe/src/main.rs     run, follow, read, facts, status, clock, version
probe/src/sampler.rs  one reading; linux.rs and macos.rs read the kernel; agents.rs
                      decides which processes are agent sessions; facts.rs the facts
                      and config state
```

## Execution lifecycle

1. Preflight scans argv for the presentation flags, so even a parse error is reported in the mode and color asked for.
2. The parser reads argv against the tree built from the catalog. Every parse error exits 2; with `--json` it also prints an `invalid_usage` envelope.
3. The command runs with the parsed arguments, options and styling, and returns a `Done` (the data, plus text for people) or an `AppError` (code, message, hint, exit code).
4. `cli::run` renders exactly one outcome: the human text, or one envelope for `--json` and `--jsonl`. Errors go to stderr. A long-running command may write `--jsonl` events before it, through `Context::event`, so a caller can follow it as it runs.

## The probe

`grove-probe run` takes a reading every two seconds, on the interval's boundaries, so a slow reading never pushes the next one later. It reads the kernel directly and starts no other program on that path:

- Linux: `/proc/stat`, `/proc/meminfo`, `/proc/loadavg`, `/proc/net/dev`, `/proc/uptime`, `statvfs`, hwmon and thermal zones, the DRM device for AMD GPUs, the power supply class, and `/proc/<pid>/{stat,cmdline}` to count agent sessions.
- macOS: Mach host statistics, `sysctl` and each interface's 64-bit counters from the interface MIB, `statfs`, libproc, the SMC for temperatures, the IORegistry for GPU load, and IOKit power sources for the battery.

CPU, load, memory, swap, disk and network are read every time. The slower-moving or costlier values are read less often and carried forward in between: temperatures every 10 seconds on Linux and every 30 on a Mac (a full SMC sweep costs more than the rest of a reading), the battery every 30 seconds on a Mac, and the full process list every 10 seconds. Between listings only the agent sessions already known are read, so a session that ends drops out at the next reading and a new one is counted within 10 seconds.

An NVIDIA GPU is read with `nvidia-smi` every two minutes, since its driver's library can't be loaded by a static binary. Facts (hostname, OS version, model, chip, address) are read at start and every ten minutes. Config state is checked every two minutes; `chezmoi verify` runs again only when its source files, its commit or the files it manages changed, and at least hourly.

Agent sessions are decided once per process from its arguments and remembered while the pid lives. Only the verdict is kept, never the arguments; what leaves the machine is two counts and their combined CPU and memory.

### Readings and the ring

A reading is 128 bytes, little-endian, with its sequence number at the start and the end:

```text
0   u64 seq            8   i64 taken_at_ms       16  u16 cores
18  u16 cpu ‰          20  u16 load1 ×100        22  u16 load5 ×100
24  u16 load15 ×100    26  u16 facts generation  28  u32 uptime s
32  u64 mem total kB   40  u64 mem available kB  48  u64 swap total kB
56  u64 swap used kB   64  u64 disk total kB     72  u64 disk used kB
80  u64 net rx bytes   88  u64 net tx bytes      96  i16 cpu temp ×10
98  i16 gpu temp ×10   100 u16 gpu load ‰        102 u8 battery %
103 u8 battery state   104 u32 gpu memory MB     108 u16 claude
110 u16 codex          112 u16 agent cpu ‰       116 u32 agent memory MB
120 u64 seq again
```

A value the machine can't give is all ones (or the minimum, for signed fields), never zero. The probe's folder is its owner's alone: the folder and its `data` are `0700` and the ring, facts and pid files `0600`, tightened when the probe starts and when an install lands over an older one. The ring file is a 64-byte header (format version, capacity, interval, the newest sequence number, a ring id chosen when the file is created, and the probe's own CPU time and memory) and 1,800 slots: an hour, 230 KB. The writer fills a slot and only then publishes its sequence number in the header, so a reader never sees a reading that isn't whole; one that races the writer sees the two sequence numbers disagree and stops.

### Streams

`grove-probe follow --since <seq>` writes the header, the facts, every reading after `seq`, and then each new reading as it lands, waiting on inotify or kqueue rather than polling. It also watches stdin, answering each echo token there with an echo frame, until stdin closes. Facts are sent again only when their generation changes. `read` does the same once and exits.

`grove stream` keeps one stream per machine, over the system `ssh` with connection sharing, or directly for this machine. It measures each machine's clock offset over a short run before connecting and stores times on its own clock. Every ten seconds it writes an eight-byte echo token to the stream's stdin; the probe answers with an echo frame (eight `0xFF` bytes and the token) at once, and the median of the last five round trips is the reading's `latency_ms`. The echo rides the stream's own connection, so it needs no ICMP and no other process, and it measures a jump host or proxy command along with the machine. Only a probe whose facts name its release (`probe_version`) is asked, and echoes stop after three go unanswered in a row, so an older probe that never reads them can't fill the connection. After any gap it reconnects with backoff and resumes from the last sequence number it wrote; a new ring id means the probe's file was replaced, and reading starts over. Readings are written once a second in one transaction: the last hour into `live_readings`, one sample a minute into `samples` with the minute's mean CPU, alert evaluation on that sample, and the last reading for `show`, `status` and `sample`. A stream is held to the pace a probe keeps (its ring at once, then one reading an interval, with room to spare) and ended if it goes faster; a reading dated more than a minute ahead of when it arrived is dropped, one dated before the minute being gathered stays live but makes no sample, and the live hour is pruned by arrival time as well as by the reading's own. Hooks run after the write commits, and with `--jsonl` each write's events follow it: `connected`, `facts`, `reading` with the machine's new latest reading, and `disconnected` with why. A caller reading them sees each machine about once a second without asking the store.

`sample` answers from a streamed reading when one is under two intervals old, otherwise asks the probe for its newest reading, and falls back to the sample script where there is no probe.

## The store

`src/store/` owns `~/.grove/grove.db`, relocatable with `GROVE_HOME`. Its schema version lives in SQLite's `user_version` and moves forward through ordered migrations; a database newer than the binary is refused with `store_too_new`. WAL journaling and a busy timeout let a stream, a scheduled sampler and an interactive command use it at once, and every write that can move an alert runs inside `BEGIN IMMEDIATE`, so two writers can't fire the same alert twice. Samples older than the retention setting (90 days unless set) are deleted as new ones are stored, by `sample` and by `stream` alike. The folder is `0700` and the database `0600`.

## Alerts, hooks and policy

A threshold rule fires only when every stored reading in a lookback of 1.25 windows breaches it, with at least two readings and the oldest past 0.8 of the window, and clears the same way; anything mixed holds. A machine's own rule overrides the fleet's. A down rule is a timer kept in `machine_contact`: the first failed contact starts it, a success before the window cancels it. The fleet session warning is a level crossing on the sum of fresh session counts.

Hooks run through `sh -c` after the readings that caused them are committed, with the event in `GROVE_*` variables and as JSON on stdin, are killed after 30 seconds, and are recorded in `hook_runs`. Policy checks run in a fixed order (reachability, capacity, sessions, quiet hours, alerts) and fail closed: a reading older than ten minutes, or dated more than a minute ahead of this clock, is no evidence.
