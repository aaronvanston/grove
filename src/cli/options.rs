//! Reading parsed option values, and checking them: `invalid_options`
//! errors carry zod-style message text and issue objects, which `--json`
//! prints as `details`.

use std::collections::HashMap;

use serde_json::{Value, json};

use super::commander::OptValue;
use crate::errors::AppError;

pub type Options = HashMap<String, OptValue>;

/// A value option given on the command line (a string).
pub fn string(options: &Options, name: &str) -> Option<String> {
    match options.get(name) {
        Some(OptValue::Str(value)) => Some(value.clone()),
        Some(OptValue::Default(Value::String(value))) => Some(value.clone()),
        _ => None,
    }
}

/// A boolean flag such as `--yes`.
pub fn flag(options: &Options, name: &str) -> bool {
    matches!(options.get(name), Some(OptValue::Bool(true)))
}

/// zod's invalid_options error from its issues: each issue's message,
/// prefixed with its path, joined with "; ".
pub fn invalid_options(issues: Vec<Value>) -> AppError {
    let message = issues
        .iter()
        .map(|issue| {
            let path: Vec<String> = issue["path"]
                .as_array()
                .map(|parts| {
                    parts
                        .iter()
                        .map(|part| part.as_str().unwrap_or_default().to_owned())
                        .collect()
                })
                .unwrap_or_default();
            let prefix = if path.is_empty() {
                String::new()
            } else {
                format!("{}: ", path.join("."))
            };
            format!("{prefix}{}", issue["message"].as_str().unwrap_or_default())
        })
        .collect::<Vec<_>>()
        .join("; ");
    AppError::usage("invalid_options", message).details(Value::Array(issues))
}

/// JavaScript's `Number(text)`.
pub fn js_number(text: &str) -> f64 {
    let trimmed = text.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    if trimmed.is_empty() {
        return 0.0;
    }
    match trimmed {
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    for (prefix, radix) in [
        ("0x", 16),
        ("0X", 16),
        ("0o", 8),
        ("0O", 8),
        ("0b", 2),
        ("0B", 2),
    ] {
        if let Some(digits) = trimmed.strip_prefix(prefix) {
            if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
                return f64::NAN;
            }
            return digits.chars().fold(0.0, |total, c| {
                total * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0))
            });
        }
    }
    let body = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(index) => (&body[..index], Some(&body[index + 1..])),
        None => (body, None),
    };
    let digits = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    let mantissa_ok = match mantissa.split_once('.') {
        Some((whole, fraction)) => {
            digits(whole) && digits(fraction) && !(whole.is_empty() && fraction.is_empty())
        }
        None => !mantissa.is_empty() && digits(mantissa),
    };
    let exponent_ok = exponent.is_none_or(|part| {
        let unsigned = part.strip_prefix(['+', '-']).unwrap_or(part);
        !unsigned.is_empty() && digits(unsigned)
    });
    if !mantissa_ok || !exponent_ok {
        return f64::NAN;
    }
    trimmed.parse().unwrap_or(f64::NAN)
}

const MAX_SAFE: f64 = 9_007_199_254_740_991.0;

/// A lower or upper bound on a number, as zod words it.
#[derive(Clone, Copy)]
pub struct Bound {
    pub value: i64,
    pub inclusive: bool,
}

/// `z.coerce.number().int()` with optional bounds, on a `--limit`-style
/// option. Returns the number, or zod's error.
pub fn coerce_int(
    name: &str,
    value: &OptValue,
    minimum: Option<Bound>,
    maximum: Option<Bound>,
) -> Result<i64, AppError> {
    let number = match value {
        OptValue::Str(text) => js_number(text),
        OptValue::Default(Value::Number(number)) => number.as_f64().unwrap_or(f64::NAN),
        OptValue::Default(Value::String(text)) => js_number(text),
        OptValue::Bool(true) => 1.0,
        _ => 0.0,
    };
    if number.is_nan() {
        return Err(invalid_options(vec![json!({
            "expected": "number",
            "code": "invalid_type",
            "received": "NaN",
            "path": [name],
            "message": "Invalid input: expected number, received NaN",
        })]));
    }
    if number.is_infinite() {
        return Err(invalid_options(vec![json!({
            "expected": "number",
            "code": "invalid_type",
            "received": "Infinity",
            "path": [name],
            "message": "Invalid input: expected number, received number",
        })]));
    }
    if number.fract() != 0.0 {
        return Err(invalid_options(vec![json!({
            "expected": "int",
            "format": "safeint",
            "code": "invalid_type",
            "path": [name],
            "message": "Invalid input: expected int, received number",
        })]));
    }
    let mut issues = Vec::new();
    if number > MAX_SAFE {
        issues.push(json!({
            "code": "too_big",
            "maximum": 9_007_199_254_740_991_i64,
            "note": "Integers must be within the safe integer range.",
            "origin": "int",
            "inclusive": true,
            "path": [name],
            "message": "Too big: expected int to be <=9007199254740991",
        }));
    } else if number < -MAX_SAFE {
        issues.push(json!({
            "code": "too_small",
            "minimum": -9_007_199_254_740_991_i64,
            "note": "Integers must be within the safe integer range.",
            "origin": "int",
            "inclusive": true,
            "path": [name],
            "message": "Too small: expected int to be >=-9007199254740991",
        }));
    }
    if let Some(bound) = minimum {
        let below = if bound.inclusive {
            number < bound.value as f64
        } else {
            number <= bound.value as f64
        };
        if below {
            let sign = if bound.inclusive { ">=" } else { ">" };
            issues.push(json!({
                "origin": "number",
                "code": "too_small",
                "minimum": bound.value,
                "inclusive": bound.inclusive,
                "path": [name],
                "message": format!("Too small: expected number to be {sign}{}", bound.value),
            }));
        }
    }
    if let Some(bound) = maximum {
        let above = if bound.inclusive {
            number > bound.value as f64
        } else {
            number >= bound.value as f64
        };
        if above {
            let sign = if bound.inclusive { "<=" } else { "<" };
            issues.push(json!({
                "origin": "number",
                "code": "too_big",
                "maximum": bound.value,
                "inclusive": bound.inclusive,
                "path": [name],
                "message": format!("Too big: expected number to be {sign}{}", bound.value),
            }));
        }
    }
    if issues.is_empty() {
        // Within the safe range, so the conversion is exact.
        Ok(number as i64)
    } else {
        Err(invalid_options(issues))
    }
}

/// A whole-number option between `min` and `max` inclusive, as zod's
/// `z.coerce.number().int().min(min).max(max)` checked it.
pub fn bounded(options: &Options, name: &str, min: i64, max: i64) -> Result<i64, AppError> {
    let value = options.get(name).cloned().unwrap_or(OptValue::Bool(false));
    coerce_int(
        name,
        &value,
        Some(Bound {
            value: min,
            inclusive: true,
        }),
        Some(Bound {
            value: max,
            inclusive: true,
        }),
    )
}

/// A duration such as 90s, 30m, 24h, 7d or 1w, in milliseconds. A bare 0
/// is zero.
pub fn duration(text: &str) -> Result<i64, AppError> {
    let trimmed = text.trim();
    if trimmed == "0" {
        return Ok(0);
    }
    let (split, unit) = trimmed.char_indices().last().unwrap_or((0, ' '));
    let scale = match unit {
        's' => 1000,
        'm' => 60_000,
        'h' => 3_600_000,
        'd' => 86_400_000,
        'w' => 604_800_000,
        _ => 0,
    };
    let amount = &trimmed[..split];
    let parsed =
        (scale > 0 && !amount.is_empty() && amount.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| amount.parse::<i64>().ok()?.checked_mul(scale))
            .flatten();
    parsed.ok_or_else(|| {
        AppError::usage(
            "invalid_duration",
            format!("\"{text}\" is not a valid duration."),
        )
        .hint("Use a number with a unit: 90s, 30m, 24h, 7d, 1w.")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `add --port 0`, `--port 70000` and `--port abc` are usage errors
    /// naming the option, with the issue as details.
    #[test]
    fn whole_numbers_outside_their_bounds_are_refused() {
        let port = |text: &str| {
            let options = Options::from([("port".to_owned(), OptValue::Str(text.into()))]);
            bounded(&options, "port", 1, 65_535)
        };
        assert_eq!(port("22").ok(), Some(22));
        let error = port("0").unwrap_err();
        assert_eq!(
            (error.code.as_str(), error.exit_code, error.message.as_str()),
            (
                "invalid_options",
                2,
                "port: Too small: expected number to be >=1"
            )
        );
        assert_eq!(
            error.details,
            Some(json!([{
                "origin": "number",
                "code": "too_small",
                "minimum": 1,
                "inclusive": true,
                "path": ["port"],
                "message": "Too small: expected number to be >=1",
            }]))
        );
        assert_eq!(
            port("70000").unwrap_err().message,
            "port: Too big: expected number to be <=65535"
        );
        assert_eq!(
            port("abc").unwrap_err().message,
            "port: Invalid input: expected number, received NaN"
        );
    }
}
