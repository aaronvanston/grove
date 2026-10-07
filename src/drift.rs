//! Where each machine stands against the config source everyone shares.
//! Outdated and diverged are different problems: one machine hasn't caught
//! up yet, the other stopped matching what it caught up to. Anything grove
//! can't see is unknown, reported rather than guessed at.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::errors::AppError;
use crate::policy::human_duration;
use crate::reading::STALE_AFTER_MS;
use crate::store::{ConfigState, user_home};
use crate::transport::run;

pub struct Verdict {
    pub state: &'static str,
    pub reason: Option<&'static str>,
    pub detail: String,
}

fn short(commit: &str) -> String {
    commit.chars().take(12).collect()
}

/// Freshness is decided before content, because a commit nobody checked
/// lately says nothing about now; verify before the commit, because hand
/// edits are the more urgent problem.
pub fn derive(config: &ConfigState, reference: &str, now: i64) -> Verdict {
    let verdict = |state, reason, detail: String| Verdict {
        state,
        reason,
        detail,
    };
    let Some(checked_at) = config.checked_at else {
        return verdict(
            "unknown",
            Some("never_read"),
            "No sample has read this machine's config state.".into(),
        );
    };
    let age = now - checked_at;
    if age > STALE_AFTER_MS {
        return verdict(
            "unknown",
            Some("stale"),
            format!(
                "The last config reading is {} old, past the {} freshness window.",
                human_duration(age as f64 / 1000.0),
                human_duration(STALE_AFTER_MS as f64 / 1000.0)
            ),
        );
    }
    let (Some(commit), Some(verify)) = (&config.commit, config.verify) else {
        return verdict(
            "unknown",
            Some("unmanaged"),
            "The machine reported no config source commit.".into(),
        );
    };
    if verify != 0 {
        return verdict(
            "diverged",
            Some("verify_failed"),
            format!("Applied files no longer match the source; verify exited {verify}."),
        );
    }
    if commit != reference {
        return verdict(
            "outdated",
            Some("commit_mismatch"),
            format!("Applied {}, which is not the source head.", short(commit)),
        );
    }
    verdict(
        "current",
        None,
        format!("Applied {} and every file matches it.", short(commit)),
    )
}

/// The head of the config source and where it was read: the remote of
/// this machine's chezmoi source clone, or the clone itself when the
/// remote can't be reached.
pub struct Reference {
    pub commit: String,
    pub source: &'static str,
}

fn is_commit(text: &str) -> bool {
    text.len() == 40
        && text
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn output(mut command: Command, timeout: Duration) -> Option<String> {
    let ran = run(&mut command, "chezmoi", None, timeout);
    ran.ok().then(|| ran.stdout.trim().to_owned())
}

/// chezmoi from GROVE_CHEZMOI_COMMAND, else PATH and the usual install
/// folders, since a scheduled run doesn't get an interactive PATH.
pub fn resolve_reference(timeout: Duration) -> Result<Reference, AppError> {
    let candidates: Vec<PathBuf> = match std::env::var_os("GROVE_CHEZMOI_COMMAND") {
        Some(command) if !command.is_empty() => vec![command.into()],
        _ => vec![
            "chezmoi".into(),
            user_home().join(".local/bin/chezmoi"),
            "/opt/homebrew/bin/chezmoi".into(),
            "/usr/local/bin/chezmoi".into(),
        ],
    };
    let directory = candidates
        .iter()
        .find_map(|candidate| {
            let mut command = Command::new(candidate);
            command.arg("source-path");
            output(command, timeout).filter(|path| !path.is_empty())
        })
        .ok_or_else(|| {
            AppError::new(
                "config_source_unavailable",
                "This machine has no config source directory to compare against.",
            )
            .hint("Install chezmoi and initialize it from the config repository, or point GROVE_CHEZMOI_COMMAND at the binary.")
        })?;
    let git = |args: &[&str]| {
        let mut command = Command::new("git");
        command.args(args).current_dir(&directory);
        output(command, timeout)
    };
    if let Some(remote) = git(&["ls-remote", "origin", "HEAD"])
        && let Some(commit) = remote
            .split_whitespace()
            .next()
            .filter(|commit| is_commit(commit))
    {
        return Ok(Reference {
            commit: commit.to_owned(),
            source: "remote",
        });
    }
    if let Some(local) = git(&["rev-parse", "HEAD"]).filter(|commit| is_commit(commit)) {
        return Ok(Reference {
            commit: local,
            source: "local",
        });
    }
    Err(AppError::new(
        "config_head_unresolved",
        "Could not resolve the head commit of the config source.",
    )
    .hint(format!(
        "Check the repository in {directory} has a commit and a reachable origin."
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The drift states, in the order they are decided.
    #[test]
    fn each_machine_is_current_outdated_diverged_or_unknown() {
        let head = "89783681daad0fa6d10703404d872999c24fcd03";
        let other = "0123456789abcdef0123456789abcdef01234567";
        let at = |commit: Option<&str>, verify: Option<i64>, checked: Option<i64>| ConfigState {
            commit: commit.map(str::to_owned),
            verify,
            checked_at: checked,
        };
        let now = 1_000_000;
        let cases = [
            (at(Some(head), Some(0), None), "unknown", Some("never_read")),
            (
                at(Some(head), Some(0), Some(now - 601_000)),
                "unknown",
                Some("stale"),
            ),
            (at(None, None, Some(now)), "unknown", Some("unmanaged")),
            (
                at(Some(other), Some(1), Some(now)),
                "diverged",
                Some("verify_failed"),
            ),
            (
                at(Some(other), Some(0), Some(now)),
                "outdated",
                Some("commit_mismatch"),
            ),
            (at(Some(head), Some(0), Some(now)), "current", None),
        ];
        for (config, state, reason) in cases {
            let verdict = derive(&config, head, now);
            assert_eq!((verdict.state, verdict.reason), (state, reason));
        }
        assert_eq!(
            derive(&at(Some(other), Some(0), Some(now)), head, now).detail,
            "Applied 0123456789ab, which is not the source head."
        );
    }
}
