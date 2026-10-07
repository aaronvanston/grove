//! Alert rules, their state and events, contact-driven down alerts, the
//! fleet concurrency warning, hooks and their runs, and policy rows.
//! Transitions are evaluated inside the caller's `write`, so two samplers
//! sharing the store can never fire the same alert twice.

use rusqlite::{OptionalExtension, Row, params};

use super::{Result, Store};
use crate::alerts::{
    Event, FLEET, LOOKBACK_FRACTION, RULE_METRICS, Rule, State, Verdict, breaches_below,
    evaluate_window, metric_value,
};
use crate::output::now_ms;
use crate::reading::{STALE_AFTER_MS, Sample};

#[derive(Debug, Clone, PartialEq)]
pub struct Hook {
    pub name: String,
    pub command: String,
    pub machine: String,
    /// `fire`, `clear` or `both`.
    pub on: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HookRun {
    pub hook: String,
    pub machine: String,
    pub metric: String,
    pub event: String,
    pub exit_code: i64,
    pub stderr: String,
    pub at: i64,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PolicyRow {
    pub machine: String,
    pub max_sessions: Option<i64>,
    pub quiet: Option<(i64, i64)>,
    pub warn_sessions: Option<i64>,
    pub updated_at: i64,
}

/// What `policy set` changes; `None` leaves a setting as it is.
#[derive(Debug, Clone, Default)]
pub struct PolicyPatch {
    pub max_sessions: Option<i64>,
    pub quiet: Option<(i64, i64)>,
    pub warn_sessions: Option<i64>,
}

/// `machine` or `fleet`: where a setting that applies came from.
pub type Scoped<T> = Option<(T, &'static str)>;

/// The policy that applies to one machine: its own settings win over the
/// fleet's, and the concurrency warning is fleet-wide only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resolved {
    pub max_sessions: Scoped<i64>,
    pub quiet: Scoped<(i64, i64)>,
    pub warn_sessions: Option<i64>,
}

/// The fleet total is a floor: machines with no fresh count are unknown
/// and left out of the sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FleetSessions {
    pub total: Option<i64>,
    pub known: i64,
    pub unknown: i64,
}

fn rule_from_row(row: &Row) -> rusqlite::Result<Rule> {
    Ok(Rule {
        metric: row.get("metric")?,
        machine: row.get("machine")?,
        threshold: row.get("threshold")?,
        window_ms: row.get("window_ms")?,
        created_at: row.get("created_at")?,
    })
}

fn state_from_row(row: &Row) -> rusqlite::Result<State> {
    Ok(State {
        machine: row.get("machine")?,
        metric: row.get("metric")?,
        triggered: row.get::<_, i64>("triggered")? == 1,
        since: row.get("since")?,
        last_value: row.get("last_value")?,
    })
}

fn hook_from_row(row: &Row) -> rusqlite::Result<Hook> {
    Ok(Hook {
        name: row.get("name")?,
        command: row.get("command")?,
        machine: row.get("machine")?,
        on: row.get("on_trigger")?,
        created_at: row.get("created_at")?,
    })
}

fn policy_from_row(row: &Row) -> rusqlite::Result<PolicyRow> {
    let start: Option<i64> = row.get("quiet_start_min")?;
    let end: Option<i64> = row.get("quiet_end_min")?;
    Ok(PolicyRow {
        machine: row.get("machine")?,
        max_sessions: row.get("max_sessions")?,
        quiet: start.zip(end),
        warn_sessions: row.get("warn_sessions")?,
        updated_at: row.get("updated_at")?,
    })
}

impl Store {
    fn all<T>(&self, sql: &str, map: fn(&Row) -> rusqlite::Result<T>) -> Result<Vec<T>> {
        let mut statement = self.db.prepare(sql)?;
        let rows = statement.query_map([], map)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn add_rule(
        &self,
        metric: &str,
        machine: &str,
        threshold: Option<f64>,
        window_ms: i64,
    ) -> Result<Rule> {
        let now = now_ms();
        self.db.execute(
            "INSERT INTO alert_rules (metric, machine, threshold, window_ms, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (metric, machine) DO UPDATE SET
               threshold = excluded.threshold,
               window_ms = excluded.window_ms",
            params![metric, machine, threshold, window_ms, now],
        )?;
        Ok(Rule {
            metric: metric.to_owned(),
            machine: machine.to_owned(),
            threshold,
            window_ms,
            created_at: now,
        })
    }

    /// Removes the rule and the state it kept.
    pub fn remove_rule(&self, metric: &str, machine: &str) -> Result<bool> {
        let removed = self.db.execute(
            "DELETE FROM alert_rules WHERE metric = ? AND machine = ?",
            params![metric, machine],
        )? > 0;
        self.db.execute(
            "DELETE FROM alert_states WHERE metric = ? AND machine = ?",
            params![metric, machine],
        )?;
        Ok(removed)
    }

    pub fn list_rules(&self) -> Result<Vec<Rule>> {
        self.all(
            "SELECT * FROM alert_rules ORDER BY machine, metric",
            rule_from_row,
        )
    }

    /// The machine's own rule wins over the fleet's.
    pub fn rule_for(&self, metric: &str, machine: &str) -> Result<Option<Rule>> {
        Ok(self
            .db
            .query_row(
                "SELECT * FROM alert_rules WHERE metric = ?1 AND machine IN (?2, '')
                 ORDER BY machine = '' LIMIT 1",
                params![metric, machine],
                rule_from_row,
            )
            .optional()?)
    }

    pub fn list_states(&self) -> Result<Vec<State>> {
        self.all(
            "SELECT * FROM alert_states ORDER BY machine, metric",
            state_from_row,
        )
    }

    fn state(&self, machine: &str, metric: &str) -> Result<Option<State>> {
        Ok(self
            .db
            .query_row(
                "SELECT * FROM alert_states WHERE machine = ? AND metric = ?",
                params![machine, metric],
                state_from_row,
            )
            .optional()?)
    }

    fn set_state(&self, state: &State, at: i64) -> Result<()> {
        self.db.execute(
            "INSERT INTO alert_states (machine, metric, triggered, since, last_value, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (machine, metric) DO UPDATE SET
               triggered = excluded.triggered,
               since = excluded.since,
               last_value = excluded.last_value,
               updated_at = excluded.updated_at",
            params![
                state.machine,
                state.metric,
                i64::from(state.triggered),
                state.since,
                state.last_value,
                at
            ],
        )?;
        Ok(())
    }

    fn record_event(&self, event: Event) -> Result<Event> {
        self.db.execute(
            "INSERT INTO alert_events (machine, metric, kind, value, threshold, window_ms, at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                event.machine,
                event.metric,
                event.kind,
                event.value,
                event.threshold,
                event.window_ms,
                event.at
            ],
        )?;
        Ok(event)
    }

    pub fn list_events(&self, limit: i64) -> Result<Vec<Event>> {
        let mut statement = self.db.prepare(
            "SELECT machine, metric, kind, value, threshold, window_ms, at
             FROM alert_events ORDER BY at DESC LIMIT ?",
        )?;
        let rows = statement.query_map([limit], |row| {
            let kind: String = row.get(2)?;
            Ok(Event {
                machine: row.get(0)?,
                metric: row.get(1)?,
                kind: if kind == "fired" { "fired" } else { "cleared" },
                value: row.get(3)?,
                threshold: row.get(4)?,
                window_ms: row.get(5)?,
                at: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Oldest first.
    pub fn samples_since(&self, machine: &str, since: f64) -> Result<Vec<Sample>> {
        let mut statement = self.db.prepare(
            "SELECT * FROM samples WHERE machine = ? AND taken_at >= ? ORDER BY taken_at ASC",
        )?;
        let rows = statement.query_map(params![machine, since], super::samples::sample_from_row)?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// The transitions a just-stored sample causes. A metric the machine
    /// reported nothing for can neither fire nor clear.
    pub fn evaluate_thresholds(&self, sample: &Sample) -> Result<Vec<Event>> {
        let now = sample.taken_at;
        let mut events = Vec::new();
        for metric in RULE_METRICS.iter().filter(|metric| **metric != "down") {
            let Some(rule) = self.rule_for(metric, &sample.machine)? else {
                continue;
            };
            let (Some(threshold), Some(value)) = (rule.threshold, metric_value(sample, metric))
            else {
                continue;
            };
            let since = now as f64 - rule.window_ms as f64 * LOOKBACK_FRACTION;
            let points: Vec<(i64, f64)> = self
                .samples_since(&sample.machine, since)?
                .iter()
                .filter_map(|row| metric_value(row, metric).map(|value| (row.taken_at, value)))
                .collect();
            let verdict = evaluate_window(
                &points,
                threshold,
                rule.window_ms,
                now,
                breaches_below(metric),
            );
            let state = self.state(&sample.machine, metric)?;
            let triggered = state.as_ref().is_some_and(|state| state.triggered);
            let event = |kind| Event {
                machine: sample.machine.clone(),
                metric: (*metric).to_owned(),
                kind,
                value: Some(value),
                threshold: Some(threshold),
                window_ms: rule.window_ms,
                at: now,
            };
            let mut next = State {
                machine: sample.machine.clone(),
                metric: (*metric).to_owned(),
                triggered,
                since: state.as_ref().and_then(|state| state.since),
                last_value: Some(value),
            };
            match verdict {
                Verdict::Fire if !triggered => {
                    next.triggered = true;
                    next.since = Some(points.first().map_or(now, |(at, _)| *at));
                    self.set_state(&next, now)?;
                    events.push(self.record_event(event("fired"))?);
                }
                Verdict::Clear if triggered => {
                    next.triggered = false;
                    next.since = None;
                    self.set_state(&next, now)?;
                    events.push(self.record_event(event("cleared"))?);
                }
                // A rule with no state yet waits for its first transition.
                _ if state.is_some() => self.set_state(&next, now)?,
                _ => {}
            }
        }
        Ok(events)
    }

    /// Notes when the machine last answered and since when it hasn't, and
    /// returns the down alert's transition, if any. Down is a timer kept
    /// as data: the first failure starts it, a success before the window
    /// cancels it, and it fires at the first failure past the window.
    pub fn record_contact(&self, machine: &str, at: i64, ok: bool) -> Result<Vec<Event>> {
        let previous: Option<(Option<i64>, Option<i64>)> = self
            .db
            .query_row(
                "SELECT last_ok_at, down_since FROM machine_contact WHERE machine = ?",
                [machine],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (last_ok, down_since) = previous.unwrap_or_default();
        let down_since = if ok {
            None
        } else {
            Some(down_since.unwrap_or(at))
        };
        self.db.execute(
            "INSERT INTO machine_contact (machine, last_ok_at, down_since) VALUES (?1, ?2, ?3)
             ON CONFLICT (machine) DO UPDATE SET last_ok_at = ?2, down_since = ?3",
            params![machine, if ok { Some(at) } else { last_ok }, down_since],
        )?;
        let Some(rule) = self.rule_for("down", machine)? else {
            return Ok(Vec::new());
        };
        let triggered = self
            .state(machine, "down")?
            .is_some_and(|state| state.triggered);
        let event = |kind| Event {
            machine: machine.to_owned(),
            metric: "down".into(),
            kind,
            value: None,
            threshold: rule.threshold,
            window_ms: rule.window_ms,
            at,
        };
        let mut state = State {
            machine: machine.to_owned(),
            metric: "down".into(),
            triggered: false,
            since: None,
            last_value: None,
        };
        if ok && triggered {
            self.set_state(&state, at)?;
            return Ok(vec![self.record_event(event("cleared"))?]);
        }
        if let Some(since) = down_since.filter(|since| !triggered && at - since >= rule.window_ms) {
            state.triggered = true;
            state.since = Some(since);
            self.set_state(&state, at)?;
            return Ok(vec![self.record_event(event("fired"))?]);
        }
        Ok(Vec::new())
    }

    /// The newest session count still fresh enough to describe now: the
    /// last reading's when it has one, else the newest stored sample that
    /// carried one. Nothing fresh is unknown.
    pub fn agent_sessions_for(&self, machine: &str, now: i64) -> Result<Option<i64>> {
        if let Some(latest) = self.latest(machine)?
            && now - latest.taken_at <= STALE_AFTER_MS
            && let Some(sessions) = latest.reading["agent_sessions"].as_f64()
        {
            return Ok(Some(sessions as i64));
        }
        Ok(self
            .db
            .query_row(
                "SELECT agent_sessions FROM samples
                 WHERE machine = ? AND taken_at >= ? AND agent_sessions IS NOT NULL
                 ORDER BY taken_at DESC LIMIT 1",
                params![machine, now - STALE_AFTER_MS],
                |row| row.get::<_, f64>(0),
            )
            .optional()?
            .map(|sessions| sessions as i64))
    }

    pub fn fleet_sessions(&self, now: i64) -> Result<FleetSessions> {
        let mut fleet = FleetSessions {
            total: None,
            known: 0,
            unknown: 0,
        };
        let mut total = 0;
        for machine in self.list()? {
            match self.agent_sessions_for(&machine.name, now)? {
                Some(sessions) => {
                    total += sessions;
                    fleet.known += 1;
                }
                None => fleet.unknown += 1,
            }
        }
        fleet.total = (fleet.known > 0).then_some(total);
        Ok(fleet)
    }

    /// The fleet concurrency warning: a level crossing rather than a
    /// window, since a count of running agents is over the line or not.
    pub fn evaluate_fleet_sessions(&self, at: i64) -> Result<Vec<Event>> {
        let Some(threshold) = self.policy(FLEET)?.and_then(|row| row.warn_sessions) else {
            return Ok(Vec::new());
        };
        let Some(total) = self.fleet_sessions(at)?.total else {
            return Ok(Vec::new());
        };
        let state = self.state(FLEET, "agent_sessions")?;
        let triggered = state.as_ref().is_some_and(|state| state.triggered);
        let event = |kind| Event {
            machine: FLEET.into(),
            metric: "agent_sessions".into(),
            kind,
            value: Some(total as f64),
            threshold: Some(threshold as f64),
            window_ms: 0,
            at,
        };
        let mut next = State {
            machine: FLEET.into(),
            metric: "agent_sessions".into(),
            triggered,
            since: state.and_then(|state| state.since),
            last_value: Some(total as f64),
        };
        let kind = if total > threshold && !triggered {
            next.triggered = true;
            next.since = Some(at);
            Some("fired")
        } else if total <= threshold && triggered {
            next.triggered = false;
            next.since = None;
            Some("cleared")
        } else {
            None
        };
        self.set_state(&next, at)?;
        match kind {
            Some(kind) => Ok(vec![self.record_event(event(kind))?]),
            None => Ok(Vec::new()),
        }
    }

    pub fn add_hook(&self, name: &str, command: &str, machine: &str, on: &str) -> Result<Hook> {
        let now = now_ms();
        self.db.execute(
            "INSERT INTO hooks (name, command, machine, on_trigger, created_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT (name) DO UPDATE SET
               command = excluded.command,
               machine = excluded.machine,
               on_trigger = excluded.on_trigger",
            params![name, command, machine, on, now],
        )?;
        Ok(Hook {
            name: name.to_owned(),
            command: command.to_owned(),
            machine: machine.to_owned(),
            on: on.to_owned(),
            created_at: now,
        })
    }

    pub fn remove_hook(&self, name: &str) -> Result<bool> {
        Ok(self
            .db
            .execute("DELETE FROM hooks WHERE name = ?", [name])?
            > 0)
    }

    pub fn list_hooks(&self) -> Result<Vec<Hook>> {
        self.all("SELECT * FROM hooks ORDER BY machine, name", hook_from_row)
    }

    /// A machine with hooks of its own takes those and only those, else
    /// the fleet's apply; then the event narrows what is left.
    pub fn hooks_for(&self, machine: &str, event: &str) -> Result<Vec<Hook>> {
        let all = self.list_hooks()?;
        let own = all.iter().any(|hook| hook.machine == machine);
        let scope = if own { machine } else { FLEET };
        Ok(all
            .into_iter()
            .filter(|hook| hook.machine == scope && (hook.on == "both" || hook.on == event))
            .collect())
    }

    pub fn record_hook_run(&self, run: &HookRun) -> Result<()> {
        self.db.execute(
            "INSERT INTO hook_runs (hook, machine, metric, event, exit_code, stderr, at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            params![
                run.hook,
                run.machine,
                run.metric,
                run.event,
                run.exit_code,
                run.stderr,
                run.at
            ],
        )?;
        Ok(())
    }

    pub fn list_hook_runs(&self, limit: i64) -> Result<Vec<HookRun>> {
        let mut statement = self.db.prepare(
            "SELECT hook, machine, metric, event, exit_code, stderr, at
             FROM hook_runs ORDER BY at DESC, id DESC LIMIT ?",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok(HookRun {
                hook: row.get(0)?,
                machine: row.get(1)?,
                metric: row.get(2)?,
                event: row.get(3)?,
                exit_code: row.get(4)?,
                stderr: row.get(5)?,
                at: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn policy(&self, machine: &str) -> Result<Option<PolicyRow>> {
        Ok(self
            .db
            .query_row(
                "SELECT * FROM policy WHERE machine = ?",
                [machine],
                policy_from_row,
            )
            .optional()?)
    }

    pub fn list_policies(&self) -> Result<Vec<PolicyRow>> {
        self.all("SELECT * FROM policy ORDER BY machine", policy_from_row)
    }

    /// Changes only what the patch names, so raising a cap keeps the quiet
    /// hours set beside it.
    pub fn set_policy(&self, machine: &str, patch: &PolicyPatch) -> Result<PolicyRow> {
        let current = self.policy(machine)?.unwrap_or_default();
        let row = PolicyRow {
            machine: machine.to_owned(),
            max_sessions: patch.max_sessions.or(current.max_sessions),
            quiet: patch.quiet.or(current.quiet),
            warn_sessions: patch.warn_sessions.or(current.warn_sessions),
            updated_at: now_ms(),
        };
        let (start, end) = row.quiet.unzip();
        self.db.execute(
            "INSERT INTO policy (machine, max_sessions, quiet_start_min, quiet_end_min, warn_sessions, updated_at)
             VALUES (?, ?, ?, ?, ?, ?)
             ON CONFLICT (machine) DO UPDATE SET
               max_sessions = excluded.max_sessions,
               quiet_start_min = excluded.quiet_start_min,
               quiet_end_min = excluded.quiet_end_min,
               warn_sessions = excluded.warn_sessions,
               updated_at = excluded.updated_at",
            params![row.machine, row.max_sessions, start, end, row.warn_sessions, row.updated_at],
        )?;
        Ok(row)
    }

    pub fn remove_policy(&self, machine: &str) -> Result<bool> {
        Ok(self
            .db
            .execute("DELETE FROM policy WHERE machine = ?", [machine])?
            > 0)
    }

    pub fn resolve_policy(&self, machine: &str) -> Result<Resolved> {
        let own = self.policy(machine)?;
        let fleet = self.policy(FLEET)?;
        let pick = |read: fn(&PolicyRow) -> Option<i64>| -> Scoped<i64> {
            own.as_ref()
                .and_then(read)
                .map(|value| (value, "machine"))
                .or_else(|| fleet.as_ref().and_then(read).map(|value| (value, "fleet")))
        };
        Ok(Resolved {
            max_sessions: pick(|row| row.max_sessions),
            quiet: own
                .as_ref()
                .and_then(|row| row.quiet)
                .map(|quiet| (quiet, "machine"))
                .or_else(|| {
                    fleet
                        .as_ref()
                        .and_then(|row| row.quiet)
                        .map(|quiet| (quiet, "fleet"))
                }),
            warn_sessions: fleet.and_then(|row| row.warn_sessions),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::tests::TempStore;
    use super::*;
    use crate::store::Labels;

    fn sample(machine: &str, at: i64, cpu: f64) -> Sample {
        Sample {
            machine: machine.into(),
            taken_at: at,
            hostname: machine.into(),
            os: "Linux".into(),
            arch: "x86_64".into(),
            cores: 4.0,
            cpu_pct: cpu,
            mem_total_kb: 100.0,
            mem_available_kb: 50.0,
            disk_total_kb: 100.0,
            disk_used_kb: 10.0,
            agent_sessions: Some(3.0),
            ..Sample::default()
        }
    }

    /// A machine's own cpu rule wins over the fleet's: at 80% the fleet
    /// rule (above 70) would fire, the machine's (above 90) doesn't, until
    /// readings past 90 cover its window; then it clears the same way.
    #[test]
    fn stored_samples_fire_and_clear_the_rule_that_applies() {
        let temp = TempStore::new();
        let store = &temp.store;
        store
            .add("cedar-01", "cedar-01", 22, &Labels::default())
            .unwrap();
        store.add_rule("cpu", FLEET, Some(70.0), 600_000).unwrap();
        store
            .add_rule("cpu", "cedar-01", Some(90.0), 600_000)
            .unwrap();
        let mut fired = Vec::new();
        for (minute, cpu) in [
            (0, 80.0),
            (10, 80.0),
            (20, 95.0),
            (25, 96.0),
            (30, 97.0),
            (40, 50.0),
            (50, 40.0),
        ] {
            let reading = sample("cedar-01", minute * 60_000, cpu);
            store.insert_sample(&reading).unwrap();
            for event in store.evaluate_thresholds(&reading).unwrap() {
                fired.push((minute, event.kind, event.value));
            }
        }
        assert_eq!(
            fired,
            [(30, "fired", Some(97.0)), (50, "cleared", Some(40.0))]
        );
        let state = store.state("cedar-01", "cpu").unwrap().expect("kept");
        assert_eq!((state.triggered, state.last_value), (false, Some(40.0)));
    }

    /// A down rule with a 5-minute window: a failure starts the clock, a
    /// success before the window cancels it, a failure past it fires, and
    /// the next success clears.
    #[test]
    fn contact_drives_the_down_alert() {
        let temp = TempStore::new();
        let store = &temp.store;
        store.add_rule("down", FLEET, None, 300_000).unwrap();
        let minute = 60_000;
        let kinds = |at: i64, ok: bool| -> Vec<&str> {
            store
                .record_contact("cam-mbp", at, ok)
                .unwrap()
                .iter()
                .map(|event| event.kind)
                .collect()
        };
        assert!(kinds(0, false).is_empty());
        assert!(
            kinds(2 * minute, true).is_empty(),
            "contact came back in time"
        );
        assert!(kinds(3 * minute, false).is_empty());
        assert_eq!(kinds(8 * minute, false), ["fired"]);
        assert!(kinds(9 * minute, false).is_empty(), "fires once");
        assert_eq!(kinds(10 * minute, true), ["cleared"]);
    }

    /// The fleet warning crosses at the level, using each machine's fresh
    /// count: two machines at 3 sessions against a warning at 5.
    #[test]
    fn the_fleet_warning_is_a_level_crossing() {
        let temp = TempStore::new();
        let store = &temp.store;
        let now = 1_000_000_000;
        for name in ["cam-mbp", "cedar-01"] {
            store.add(name, name, 22, &Labels::default()).unwrap();
            store.insert_sample(&sample(name, now, 10.0)).unwrap();
        }
        store
            .set_policy(
                FLEET,
                &PolicyPatch {
                    warn_sessions: Some(5),
                    ..PolicyPatch::default()
                },
            )
            .unwrap();
        assert_eq!(
            store.fleet_sessions(now).unwrap(),
            FleetSessions {
                total: Some(6),
                known: 2,
                unknown: 0
            }
        );
        let fired = store.evaluate_fleet_sessions(now).unwrap();
        assert_eq!(
            fired
                .iter()
                .map(|event| (event.kind, event.value))
                .collect::<Vec<_>>(),
            [("fired", Some(6.0))]
        );
        assert!(store.evaluate_fleet_sessions(now).unwrap().is_empty());
        // Ten minutes on, nothing is fresh: unknown, so no verdict.
        assert_eq!(store.fleet_sessions(now + 700_000).unwrap().total, None);
    }

    /// Hooks scope like rules: a machine with its own hooks gets only those.
    #[test]
    fn a_machine_with_its_own_hooks_gets_only_those() {
        let temp = TempStore::new();
        let store = &temp.store;
        store.add_hook("fleet-both", "true", FLEET, "both").unwrap();
        store
            .add_hook("own-clear", "true", "cedar-01", "clear")
            .unwrap();
        let names = |machine: &str, event: &str| -> Vec<String> {
            store
                .hooks_for(machine, event)
                .unwrap()
                .into_iter()
                .map(|hook| hook.name)
                .collect()
        };
        assert_eq!(names("cam-mbp", "fire"), ["fleet-both"]);
        assert!(names("cedar-01", "fire").is_empty());
        assert_eq!(names("cedar-01", "clear"), ["own-clear"]);
    }
}
