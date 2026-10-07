# grove command reference

The commands of grove 0.1.4. This page is kept in step with `src/catalog.json`; `grove schema --json` and `grove describe <command>` print the same catalog from the binary.

Every command also takes the global flags: `--json` or `--jsonl` for machine output, `--compact`, `--color <when>` or `--no-color`, `--non-interactive`, `-q, --quiet` and `--verbose`.

## `grove add <name> <endpoint>`

Register a machine with its endpoint

Records a machine in the local registry. The endpoint is stored as given and is only used when a command reaches out, so unreachable machines can be registered ahead of time. Labels describe what the machine is: trust, privacy, power, and locality take free-form values and can be set here or later with 'grove label'. An endpoint of localhost, 127.0.0.1, or ::1 is this machine, which is read with sh directly instead of over SSH.

### Options

- `--port <port>`: SSH port used to reach the machine. Default: `22`.
- `--trust <value>`: Value for the trust label.
- `--privacy <value>`: Value for the privacy label.
- `--power <value>`: Value for the power label.
- `--locality <value>`: Value for the locality label.

### Examples

```bash
grove add cam-mbp cam-mbp.local
grove add cedar-01 100.64.0.7 --port 22 --json
grove add cedar-01 100.64.0.7 --trust shared --power mains --locality office
```

## `grove list`

List registered machines

Aliases: `ls`

### Examples

```bash
grove list
grove ls --json
```

## `grove label <name>`

Set trust, privacy, power, and locality on a machine

Sets labels on a registered machine. The four keys are fixed and the values are free-form: trust says who controls the machine, privacy says what may live on it, power says what it runs on, and locality says where it is. grove stores and reports the values without interpreting them, and policy questions read them alongside the readings. Only the labels you pass change, and an empty value clears one.

### Options

- `--trust <value>`: Value for the trust label.
- `--privacy <value>`: Value for the privacy label.
- `--power <value>`: Value for the power label.
- `--locality <value>`: Value for the locality label.

### Examples

```bash
grove label cam-mbp --trust personal --power battery
grove label cedar-01 --trust shared --json
grove label cedar-01 --privacy ''
```

## `grove rm <name>`

Remove a machine from the registry

### Options

- `-y, --yes`: Confirm the removal without prompting.

### Examples

```bash
grove rm cam-mini
grove rm cam-mini --yes --json
```

## `grove status [name]`

Check which machines are reachable right now

Runs a no-op command on each machine over SSH and times the round trip, so reachable means the same management path every other command uses, including ssh_config aliases. Where a stream is live, the latency is its echo round trip instead (latency_source says which). Exits non-zero when any checked machine is unreachable, so the command works as a health gate in scripts.

### Options

- `--timeout <ms>`: Per-machine timeout in milliseconds. Default: `10000`.

### Examples

```bash
grove status
grove status cam-mini
grove status --json
```

## `grove sample [name]`

Sample CPU, memory, disk, network, sensors, and agents over SSH

Connects to each machine over SSH (or runs sh directly for this machine), reads CPU, load, memory, disk, network, sensors, and running agents in one round trip, scores its health, and stores the result as a timestamped sample. Collection is best-effort: unreachable machines are reported in the result and recorded as failed contacts, which feed down alerts, and the exit code reflects the collection run rather than machine health. Use status or alerts state as gates. --min-store-interval always returns a fresh reading but stores it, and evaluates alerts on it, only when the last stored sample is at least that old. Nothing is installed on the machines.

### Options

- `--timeout <ms>`: Per-machine timeout in milliseconds. Default: `15000`.
- `--min-store-interval <duration>`: Store a reading only when the last stored one is at least this old.
- `--concurrency <count>`: Machines sampled at once. Default: `8`.

### Examples

```bash
grove sample
grove sample cam-mini
grove sample cam-mini --json --min-store-interval 55s
```

## `grove show <name>`

Inspect one machine and its last reading

### Examples

```bash
grove show cam-mini
grove show cam-mini --json
```

## `grove drift`

Compare each machine's applied config against the source head

Compares the config source commit each machine last applied against the head of the config source, and folds in whether the files that commit wrote still match it. The head is resolved on this machine, from the origin of the local config source clone, falling back to that clone's own commit when the remote cannot be reached; the answer says which was used. A machine is current when it applied the head and every file matches, outdated when it applied something else, and diverged when its files no longer match whatever it applied. Anything grove cannot see right now, an unreachable machine, a reading too old to describe now, or a machine with no config source at all, is unknown and reported as unknown rather than guessed at. Exit 0 means every machine is current, so the command composes as a gate before a rollout.

### Options

- `--timeout <ms>`: Timeout in milliseconds for resolving the source head. Default: `10000`.

### Examples

```bash
grove drift
grove drift --json
grove drift --quiet || echo 'fleet is not converged'
```

## `grove history <name>`

List stored samples for a machine

Lists stored samples newest first. Network rates are derived between consecutive samples, so two samples some seconds apart give a real transfer rate.

### Options

- `--limit <count>`: Maximum samples to return. Default: `20`.

### Examples

```bash
grove history cam-mini
grove history cam-mini --limit 5 --json
```

## `grove graph <name>`

Chart a machine's stored samples in the terminal

Renders stored samples as terminal charts, one panel per metric, bucketed across the requested window. Empty buckets stay blank so downtime is visible instead of interpolated. JSON output returns the bucketed series for other tools to plot.

### Options

- `--since <duration>`: Window to graph, for example 90m, 24h, 7d. Default: `24h`.
- `--metric <metric>`: Graph a single metric: cpu, mem, swap, disk, load, temp, battery, net, or agents. One of `cpu`, `mem`, `swap`, `disk`, `load`, `temp`, `battery`, `net`, `agents`.
- `--width <columns>`: Chart width in characters. Default: `60`.

### Examples

```bash
grove graph web-01
grove graph web-01 --since 1h --metric cpu
grove graph web-01 --since 7d --json
```

## `grove watch [name]`

Full-screen live dashboard for the fleet

Takes over the terminal with a full-screen fleet dashboard: rolling CPU, memory, and network graphs, usage meters, and load per machine, refreshed every second by default. SSH connections are multiplexed so the polling stays cheap. One sample per machine per minute is persisted to history; the live stream stays in memory. Interactive terminals only; press q or Ctrl+C to stop.

### Options

- `--interval <seconds>`: Seconds between refreshes. Default: `1`.
- `--timeout <ms>`: Per-machine timeout in milliseconds. Default: `15000`.

### Examples

```bash
grove watch
grove watch web-01
grove watch --interval 5
```

## `grove ps <name>`

Show the busiest processes on a machine

Runs ps on the machine over SSH and returns the busiest processes by CPU. A snapshot that works headless; use 'top' for the live interactive view.

### Options

- `--limit <count>`: Number of processes to return. Default: `15`.
- `--timeout <ms>`: Timeout in milliseconds. Default: `15000`.

### Examples

```bash
grove ps cam-mini
grove ps cam-mini --limit 5 --json
```

## `grove top <name>`

Attach an interactive top session on a machine

Attaches an interactive top session on the machine over SSH with a real TTY. Interactive terminals only; headless callers get the same data from 'ps'.

### Examples

```bash
grove top cam-mini
```

## `grove stream [name]`

Collect from every probe continuously

Keeps one SSH stream open to each machine's probe and writes every reading as it lands: the last hour at full resolution, one sample a minute into history (with its alerts and hooks), and the last reading for show, status and sample. After any gap it reconnects and resumes from the last reading it got, catching up from the probe's one-hour ring. Every ten seconds it times an echo over each stream, so each reading carries latency_ms, the median of the last five round trips, through any jump host or proxy command. Run it under your scheduler or service manager; it stops on Ctrl+C or SIGTERM. With --jsonl it also writes an event a line as each write lands: connected, facts (with the probe's release as probe_version), reading (the machine's new latest reading, the same record sample gives) and disconnected (with why), before the result.

### Options

- `--for <duration>`: Stop after this long instead of running until interrupted.

### Examples

```bash
grove stream
grove stream --for 10m --json
grove stream cedar-01 --jsonl
```

## `grove probe install <name>`

Install or update the resident probe on a machine

Finds the machine's platform, checks the matching grove-probe archive against SHA256SUMS on this machine, sends it over SSH, and installs it under launchd (macOS) or a systemd user unit (Linux) so it keeps sampling every two seconds. Running it again updates the probe in place.

### Options

- `--from <source>`: Folder or release URL holding the archives and SHA256SUMS.
- `--dir <path>`: Folder on the machine to install into (default ~/.grove-probe).
- `--no-service`: Install without starting it under launchd or systemd.

### Examples

```bash
grove probe install cedar-01
grove probe install cam-mbp --from ./dist --json
```

## `grove probe use <name> <dir>`

Read a probe already running on a machine

Records where a probe that was installed some other way lives, without changing anything on the machine.

### Examples

```bash
grove probe use cedar-01 /opt/grove-probe
```

## `grove probe uninstall <name>`

Stop and remove a machine's probe

Stops the probe's launchd agent or systemd unit, removes it, and deletes the probe's folder. History already collected stays.

### Examples

```bash
grove probe uninstall cedar-01 --json
```

## `grove probe status [name]`

Show each probe's position and what it costs

Asks each probe which release it is, how far its ring has got and how much CPU time and memory it has used, beside how far grove has read.

### Examples

```bash
grove probe status
grove probe status cedar-01 --json
```

## `grove alerts add <metric> [machine]`

Add or replace an alert rule

Creates or replaces an alert rule. Threshold rules fire only when every sample across the window breaches the threshold, and clear only when every sample across the window is back on the safe side, so one spike never fires and one dip never clears. Most metrics breach upwards and take --above; battery breaches downwards and takes --below. A down rule fires when a machine stays unreachable past the window and cancels if contact returns first. A sample with no reading for the metric is no evidence either way, so a machine without the sensor never fires. A machine-specific rule overrides a fleet-wide rule for the same metric. Evaluation happens when samples are recorded, so alerting is only as fresh as the sampling cadence.

### Options

- `--above <value>`: Threshold the metric must stay above (not used for down).
- `--below <value>`: Threshold the metric must stay below, for battery.
- `--for <duration>`: Window the condition must hold for, such as 90s or 10m.

### Examples

```bash
grove alerts add cpu --above 90 --for 10m
grove alerts add swap --above 90 --for 10m
grove alerts add cpu_temp --above 85 --for 5m cedar-01
grove alerts add battery --below 20 --for 5m
grove alerts add down --for 5m
```

## `grove alerts list`

List alert rules

### Examples

```bash
grove alerts list
grove alerts list --json
```

## `grove alerts rm <metric> [machine]`

Remove an alert rule

### Examples

```bash
grove alerts rm cpu
grove alerts rm down web-01 --json
```

## `grove alerts state [machine]`

Show current alert state; non-zero exit while firing

Reports the current alert state for every rule and machine it applies to. Exits non-zero when any alert is firing, so the command composes as a gate the same way status does.

### Examples

```bash
grove alerts state
grove alerts state web-01
grove alerts state --json || echo 'something is alerting'
```

## `grove alerts events`

List recent alert fires and clears

### Options

- `--limit <count>`: Maximum events to return. Default: `20`.

### Examples

```bash
grove alerts events
grove alerts events --limit 5 --json
```

## `grove hooks add <name> <command> [machine]`

Add or replace a command to run on alert transitions

Registers a command to run when an alert fires or clears. grove ships no delivery of its own: the command is yours, so notification, paging, ticketing, and logging are all the same mechanism. The command runs through sh -c with the event in environment variables (GROVE_EVENT, GROVE_MACHINE, GROVE_METRIC, GROVE_THRESHOLD, GROVE_VALUE, GROVE_WINDOW, GROVE_AT) and the same event as one JSON object on stdin. Quote the command in single quotes so those variables survive to run time. A machine that has its own hooks uses those instead of the fleet-wide ones.

### Options

- `--on <events>`: Which transitions run the hook: fire, clear, or both. One of `fire`, `clear`, `both`. Default: `both`.

### Examples

```bash
grove hooks add phone 'curl -fsS -d "$GROVE_MACHINE $GROVE_METRIC is $GROVE_VALUE" https://example.com/notify'
grove hooks add ops 'curl -fsS -H "content-type: application/json" --data-binary @- https://hooks.example.com/grove'
grove hooks add page-me '/usr/local/bin/page.sh' --on fire cam-mini
```

## `grove hooks list`

List configured hooks

### Examples

```bash
grove hooks list
grove hooks list --json
```

## `grove hooks rm <name>`

Remove a hook

### Examples

```bash
grove hooks rm phone
grove hooks rm ops --json
```

## `grove hooks test <name>`

Run one hook against a synthetic event

Runs one hook against a synthetic event so the whole path is provable before an alert depends on it. The command receives the same environment variables and the same JSON on stdin it would receive for a real fire. The run is recorded alongside real runs, and the exit code of the command becomes the exit code here.

### Options

- `--event <event>`: Which transition to simulate. One of `fire`, `clear`. Default: `fire`.

### Examples

```bash
grove hooks test phone
grove hooks test ops --json
```

## `grove hooks runs`

List recent hook runs

Lists hook runs newest first, successful and failed. Hooks are best-effort and never change the exit code of a sample, so this log is where a command that started failing shows up.

### Options

- `--limit <count>`: Maximum runs to return. Default: `20`.

### Examples

```bash
grove hooks runs
grove hooks runs --limit 5 --json
```

## `grove policy set [machine]`

Set caps, quiet hours, and the concurrency warning

Sets what the fleet is allowed to do. A cap limits how many unattended agent sessions may run on a machine, and a machine-specific cap overrides the fleet-wide one for that machine. Quiet hours are a local-time window the machine should be left alone in, and they scope the same way. The concurrency warning is fleet-wide only: when the fleet's total running sessions cross it, grove raises an alert event, so hooks deliver it like any other alert. Only the settings you pass change. Nothing is enforced; policy is what 'grove policy explain' reports on.

### Options

- `--max-sessions <count>`: Concurrent unattended agent sessions allowed.
- `--quiet-hours <window>`: Local-time window to leave alone, such as 22:00-07:00.
- `--warn-sessions <count>`: Fleet-wide session count that raises an alert when crossed.

### Examples

```bash
grove policy set --max-sessions 6
grove policy set --max-sessions 2 cam-mbp
grove policy set --quiet-hours 22:00-07:00
grove policy set --warn-sessions 10 --json
```

## `grove policy show`

Show configured policy and current fleet concurrency

Lists every policy row with the fleet's current session total beside it, so a cap and what is running under it read together.

### Examples

```bash
grove policy show
grove policy show --json
```

## `grove policy rm [machine]`

Remove a policy row

Drops a whole policy row. Dropping a machine's row leaves it under the fleet-wide policy; dropping the fleet-wide row leaves machines with only their own.

### Examples

```bash
grove policy rm cam-mbp
grove policy rm --json
```

## `grove policy explain <machine>`

Say whether a machine may take new unattended work

Answers whether a machine is eligible for new unattended work right now, and says why. It weighs reachability, how fresh the capacity reading is, the running session count against the cap that applies, quiet hours, and anything firing. Unknown fails closed: a machine grove cannot see is not eligible, because the honest answer when the evidence is missing is no. Exit 0 means eligible and exit 1 means ineligible or unknown, so the command composes as a gate. Nothing is enforced anywhere else; a caller that wants to respect policy comes through here.

### Options

- `--timeout <ms>`: Reachability timeout in milliseconds. Default: `10000`.

### Examples

```bash
grove policy explain cam-mini
grove policy explain cam-mbp --json
grove policy explain cedar-01 --quiet || echo 'not now'
```

## `grove prune`

Delete stored samples older than a cutoff

Deletes samples older than --older-than, or older than the retention setting when it is not given. Sampling applies the retention setting to each machine it stores a sample for, so prune is only needed to cut deeper or after changing the setting.

### Options

- `--older-than <duration>`: Delete samples older than this.

### Examples

```bash
grove prune
grove prune --older-than 30d --json
```

## `grove retention [duration]`

Show or set how long samples are kept

Without an argument, reports how long samples are kept (90 days unless set). With a duration, sets it; 'off' keeps samples forever.

### Examples

```bash
grove retention
grove retention 30d
grove retention off --json
```

## `grove version`

Show detailed version and runtime information

### Examples

```bash
grove version
grove version --json
```

## `grove doctor`

Check the local install and configuration

Runs fast, offline checks. It does not contact the configured service.

### Examples

```bash
grove doctor
grove doctor --json
```

## `grove schema`

Print the machine-readable command catalog

### Examples

```bash
grove schema --json
```

## `grove describe <command...>`

Describe one command and its contract

### Examples

```bash
grove describe alerts add
grove describe sample --json
```

## `grove completion <shell>`

Generate a shell completion script

### Examples

```bash
grove completion zsh > ~/.zfunc/_grove
grove completion fish > ~/.config/fish/completions/grove.fish
```
