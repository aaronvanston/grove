//! The data commands: prune deletes old samples, and retention says how
//! long sampling keeps them.

use serde_json::{Value, json};

use super::machines::open_store;
use super::options::{duration, string};
use super::{Context, Done, NAME};
use crate::errors::AppError;
use crate::output::{format_duration, iso_ms, now_ms};

fn retention_record(retention: Option<i64>) -> Value {
    json!({
        "retention": retention.map(format_duration),
        "retention_ms": retention,
    })
}

pub fn prune(context: &Context) -> Result<Done, AppError> {
    let older_than = string(&context.options, "olderThan")
        .map(|text| duration(&text))
        .transpose()?;
    let store = open_store()?;
    let keep = match older_than {
        Some(ms) => Some(ms),
        None => store.retention()?,
    };
    let before = keep.map(|ms| now_ms() - ms);
    let deleted = match before {
        Some(before) => store.prune(None, before)?,
        None => 0,
    };
    let ui = &context.ui;
    let human = match before {
        Some(before) => format!(
            "{} Deleted {deleted} sample{} taken before {}",
            ui.success(ui.symbols.success),
            if deleted == 1 { "" } else { "s" },
            iso_ms(before)
        ),
        None => {
            ui.muted("Retention is off, so nothing was deleted. Pass --older-than to cut anyway.")
        }
    };
    Ok(Done::new(
        json!({ "before": before.map(iso_ms), "deleted": deleted }),
        human,
    ))
}

pub fn retention(context: &Context) -> Result<Done, AppError> {
    let store = open_store()?;
    if let Some(text) = context.argument(0) {
        let retention = if text == "off" {
            None
        } else {
            match duration(&text)? {
                0 => {
                    return Err(AppError::usage(
                        "invalid_duration",
                        "Keeping samples for no time at all would delete each one as it lands.",
                    )
                    .hint(format!(
                        "Use a duration such as 30d, or '{NAME} retention off'."
                    )));
                }
                ms => Some(ms),
            }
        };
        store.set_retention(retention)?;
    }
    let retention = store.retention()?;
    let human = match retention {
        Some(ms) => format!("Samples are kept for {}.", format_duration(ms)),
        None => "Samples are kept forever.".to_owned(),
    };
    Ok(Done::new(retention_record(retention), human))
}
