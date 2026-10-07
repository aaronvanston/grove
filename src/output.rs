//! The machine-readable surface: success and error envelopes, the time
//! format every record uses, and numbers written the way JSON.stringify
//! writes them. Scripts that consume the output parse the text, so spacing, key
//! order and number formatting are part of the contract.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::errors::AppError;

/// Every envelope, success or error, carries this version. It changes
/// whenever a public record shape changes incompatibly; additive fields
/// keep it.
pub const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Human,
    Json,
    Jsonl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    Auto,
    Always,
    Never,
}

/// The global flags, as one invocation resolved them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Globals {
    pub color: ColorMode,
    pub compact: bool,
    pub mode: Mode,
    pub quiet: bool,
}

/// Milliseconds since the epoch, the unit every stored time uses.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
        })
}

/// `Date.prototype.toISOString`: UTC, always three decimals and a `Z`.
pub fn iso_ms(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let in_day = ms.rem_euclid(86_400_000);
    let (year, month, day) = civil_from_days(days);
    let hours = in_day / 3_600_000;
    let minutes = in_day / 60_000 % 60;
    let seconds = in_day / 1000 % 60;
    let millis = in_day % 1000;
    format!("{year:04}-{month:02}-{day:02}T{hours:02}:{minutes:02}:{seconds:02}.{millis:03}Z")
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// algorithm).
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// A number as JavaScript writes it: whole values without a fraction
/// (`33`, not `33.0`), anything not finite as null.
pub fn num(value: f64) -> Value {
    if value.is_finite() && value.fract() == 0.0 && value.abs() < 9_007_199_254_740_992.0 {
        // Whole and within the safe range, so the conversion is exact.
        Value::from(value as i64)
    } else {
        serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
    }
}

pub fn opt_num(value: Option<f64>) -> Value {
    value.map_or(Value::Null, num)
}

/// Rounds to one decimal, as `Math.round(x * 10) / 10` does.
pub fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// A duration written in the largest unit it divides into.
pub fn format_duration(ms: i64) -> String {
    for (size, label) in [
        (86_400_000, "d"),
        (3_600_000, "h"),
        (60_000, "m"),
        (1000, "s"),
    ] {
        if ms >= size && ms % size == 0 {
            return format!("{}{label}", ms / size);
        }
    }
    format!("{ms}ms")
}

pub fn opt_iso(ms: Option<i64>) -> Value {
    ms.map_or(Value::Null, |ms| Value::from(iso_ms(ms)))
}

/// `JSON.stringify(value, null, 2)`, or one line with `compact`.
pub fn to_json(value: &Value, compact: bool) -> String {
    let rendered = if compact {
        serde_json::to_string(value)
    } else {
        serde_json::to_string_pretty(value)
    };
    rendered.unwrap_or_default()
}

/// What a command hands back on success, before it is rendered.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub data: Value,
    pub exit_code: i32,
    pub hint: Option<String>,
    pub warnings: Vec<String>,
}

impl Outcome {
    pub fn new(data: Value) -> Self {
        Self {
            data,
            exit_code: crate::errors::exit::OK,
            hint: None,
            warnings: Vec::new(),
        }
    }
}

/// The success envelope for `--json` or `--jsonl`.
pub fn success_envelope(command: &str, outcome: &Outcome, globals: &Globals) -> String {
    let mut envelope = Map::new();
    if globals.mode == Mode::Jsonl {
        envelope.insert("timestamp".into(), Value::from(iso_ms(now_ms())));
        envelope.insert("type".into(), Value::from("result"));
    }
    envelope.insert("command".into(), Value::from(command));
    envelope.insert("data".into(), outcome.data.clone());
    envelope.insert("ok".into(), Value::Bool(true));
    envelope.insert("schemaVersion".into(), Value::from(SCHEMA_VERSION));
    if !outcome.warnings.is_empty() {
        envelope.insert("warnings".into(), json!(outcome.warnings));
    }
    if let Some(hint) = &outcome.hint {
        envelope.insert("hint".into(), Value::from(hint.as_str()));
    }
    let compact = globals.compact || globals.mode == Mode::Jsonl;
    to_json(&Value::Object(envelope), compact)
}

/// One `--jsonl` event, written while a command runs: its type and data
/// beside the schema version and the time it was written.
pub fn event_record(kind: &str, data: Value) -> String {
    let mut record = Map::new();
    record.insert("data".into(), data);
    record.insert("schemaVersion".into(), Value::from(SCHEMA_VERSION));
    record.insert("timestamp".into(), Value::from(iso_ms(now_ms())));
    record.insert("type".into(), Value::from(kind));
    to_json(&Value::Object(record), true)
}

/// The error envelope written to stderr in every machine mode.
pub fn error_envelope(error: &AppError, compact: bool) -> String {
    let mut body = Map::new();
    body.insert("code".into(), Value::from(error.code.as_str()));
    body.insert("message".into(), Value::from(error.message.as_str()));
    if let Some(hint) = &error.hint {
        body.insert("hint".into(), Value::from(hint.as_str()));
    }
    if let Some(details) = &error.details {
        body.insert("details".into(), details.clone());
    }
    let envelope = json!({
        "error": Value::Object(body),
        "ok": false,
        "schemaVersion": SCHEMA_VERSION,
    });
    to_json(&envelope, compact)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Numbers read as JSON.stringify writes them: a reading of 33.0
    /// degrees is `33`, and 54.9 stays `54.9`.
    #[test]
    fn numbers_and_times_read_as_javascript_writes_them() {
        assert_eq!(num(33.0).to_string(), "33");
        assert_eq!(num(54.9).to_string(), "54.9");
        assert_eq!(num(f64::NAN), Value::Null);
        assert_eq!(iso_ms(1_791_343_943_100), "2026-10-07T03:32:23.100Z");
        assert_eq!(iso_ms(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn envelopes_keep_their_key_order_and_spacing() {
        let globals = Globals {
            color: ColorMode::Auto,
            compact: false,
            mode: Mode::Json,
            quiet: false,
        };
        let mut outcome = Outcome::new(json!({ "name": "cam-mbp", "removed": true }));
        outcome.hint = Some("start it".into());
        assert_eq!(
            success_envelope("rm", &outcome, &globals),
            "{\n  \"command\": \"rm\",\n  \"data\": {\n    \"name\": \"cam-mbp\",\n    \"removed\": true\n  },\n  \"ok\": true,\n  \"schemaVersion\": 1,\n  \"hint\": \"start it\"\n}"
        );
        let error = AppError::new(
            "machine_not_found",
            "No machine named \"nobody\" is registered.",
        )
        .hint("See what is registered with 'grove list'.");
        assert_eq!(
            error_envelope(&error, true),
            "{\"error\":{\"code\":\"machine_not_found\",\"message\":\"No machine named \\\"nobody\\\" is registered.\",\"hint\":\"See what is registered with 'grove list'.\"},\"ok\":false,\"schemaVersion\":1}"
        );
    }
}
