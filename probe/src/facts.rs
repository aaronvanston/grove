//! What the machine is (facts), and what config management says about it.
//! Facts barely change, so they are read at start and every ten minutes.
//! Config is checked every two minutes, as often as a two-minute sampler
//! would read it, but
//! `chezmoi verify` only runs again when its inputs changed: the source
//! clone's files and commit, or the files it manages. Readings carry a
//! generation number that moves whenever either does, so a stream sends
//! the facts again only then.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const FACTS_EVERY: Duration = Duration::from_secs(600);
const CONFIG_EVERY: Duration = Duration::from_secs(120);
/// Verify is rerun at least this often even when nothing seems changed.
const VERIFY_AT_LEAST: Duration = Duration::from_secs(3600);

fn run(program: &Path, args: &[&str], cwd: Option<&Path>) -> Option<String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let output = command.output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn which(program: &str, extra: &[PathBuf]) -> Option<PathBuf> {
    let path = std::env::var("PATH").unwrap_or_default();
    path.split(':')
        .filter(|dir| !dir.is_empty())
        .map(|dir| PathBuf::from(dir).join(program))
        .chain(extra.iter().cloned())
        .find(|candidate| candidate.is_file())
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from)
}

/// The commit the git checkout holding `path` is on, read from its files
/// rather than by running git. The source may be a folder inside the
/// checkout, so the search walks up to the nearest `.git`.
pub fn git_head(path: &Path) -> Option<String> {
    let checkout = path.ancestors().find(|dir| dir.join(".git").exists())?;
    let mut git = checkout.join(".git");
    if git.is_file() {
        let pointer = std::fs::read_to_string(&git).ok()?;
        git = checkout.join(pointer.strip_prefix("gitdir:")?.trim());
    }
    let head = std::fs::read_to_string(git.join("HEAD")).ok()?;
    let head = head.trim();
    let Some(reference) = head.strip_prefix("ref:").map(str::trim) else {
        return Some(head.to_owned()).filter(|sha| sha.len() == 40);
    };
    // A worktree's refs live in the common directory.
    let common = std::fs::read_to_string(git.join("commondir"))
        .ok()
        .map_or_else(|| git.clone(), |dir| git.join(dir.trim()));
    if let Ok(sha) = std::fs::read_to_string(common.join(reference)) {
        return Some(sha.trim().to_owned()).filter(|sha| sha.len() == 40);
    }
    let packed = std::fs::read_to_string(common.join("packed-refs")).ok()?;
    packed.lines().find_map(|line| {
        let (sha, name) = line.split_once(' ')?;
        (name == reference && sha.len() == 40).then(|| sha.to_owned())
    })
}

/// Paths, sizes and times of every file under `dir`, skipping .git.
fn walk(dir: &Path, hasher: &mut DefaultHasher) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        if entry.file_name() == ".git" {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        entry.path().hash(hasher);
        (meta.len(), meta.mtime(), meta.mtime_nsec()).hash(hasher);
        if meta.is_dir() {
            walk(&entry.path(), hasher);
        }
    }
}

#[derive(Default)]
struct Config {
    chezmoi: Option<PathBuf>,
    source: Option<PathBuf>,
    source_at: Option<Instant>,
    managed: Vec<PathBuf>,
    source_print: u64,
    fingerprint: u64,
    verified_at: Option<Instant>,
    commit: Option<String>,
    verify: Option<i32>,
}

impl Config {
    fn check(&mut self, now: Instant) {
        let Some(chezmoi) = self.chezmoi.clone() else {
            return;
        };
        if self
            .source_at
            .is_none_or(|at| now.duration_since(at) >= VERIFY_AT_LEAST)
        {
            self.source = run(&chezmoi, &["source-path"], None)
                .filter(|path| !path.is_empty())
                .map(PathBuf::from);
            self.source_at = Some(now);
        }
        let Some(source) = self.source.clone() else {
            (self.commit, self.verify) = (None, None);
            return;
        };
        self.commit = git_head(&source);
        if self.commit.is_none() {
            self.verify = None;
            return;
        }
        let mut source_hasher = DefaultHasher::new();
        self.commit.hash(&mut source_hasher);
        walk(&source, &mut source_hasher);
        let source_print = source_hasher.finish();
        if source_print != self.source_print || self.verified_at.is_none() {
            self.source_print = source_print;
            self.managed = run(
                &chezmoi,
                &["managed", "--include=files", "--path-style=absolute"],
                None,
            )
            .unwrap_or_default()
            .lines()
            .map(PathBuf::from)
            .collect();
        }
        let mut hasher = DefaultHasher::new();
        source_print.hash(&mut hasher);
        for path in &self.managed {
            if let Ok(meta) = std::fs::symlink_metadata(path) {
                (meta.len(), meta.mtime(), meta.mtime_nsec(), meta.mode()).hash(&mut hasher);
            } else {
                0_u8.hash(&mut hasher);
            }
        }
        let fingerprint = hasher.finish();
        if fingerprint != self.fingerprint
            || self
                .verified_at
                .is_none_or(|at| now.duration_since(at) >= VERIFY_AT_LEAST)
        {
            let status = Command::new(&chezmoi)
                .arg("verify")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            self.verify = status.ok().and_then(|status| status.code());
            self.fingerprint = fingerprint;
            self.verified_at = Some(now);
        }
    }
}

pub struct Facts {
    path: PathBuf,
    static_lines: String,
    facts_at: Option<Instant>,
    config: Config,
    config_at: Option<Instant>,
    checked_at_ms: i64,
    written: String,
    pub generation: u16,
}

impl Facts {
    pub fn new(dir: &Path) -> Self {
        let mut extra = vec![home().join(".local/bin/chezmoi")];
        extra.extend(["/opt/homebrew/bin/chezmoi", "/usr/local/bin/chezmoi"].map(PathBuf::from));
        Self {
            path: dir.join("facts"),
            static_lines: String::new(),
            facts_at: None,
            config: Config {
                chezmoi: which("chezmoi", &extra),
                ..Config::default()
            },
            config_at: None,
            checked_at_ms: 0,
            written: String::new(),
            generation: 0,
        }
    }

    /// Refreshes what is due and rewrites the facts file when anything
    /// changed. Returns the generation to stamp the reading with.
    pub fn tick(&mut self, now_ms: i64) -> u16 {
        let now = Instant::now();
        if self
            .facts_at
            .is_none_or(|at| now.duration_since(at) >= FACTS_EVERY)
        {
            self.static_lines = crate::platform_facts();
            self.facts_at = Some(now);
        }
        if self
            .config_at
            .is_none_or(|at| now.duration_since(at) >= CONFIG_EVERY)
        {
            self.config.check(now);
            self.config_at = Some(now);
            self.checked_at_ms = now_ms;
        }
        let mut text = self.static_lines.clone();
        if let Some(commit) = &self.config.commit {
            text.push_str(&format!("config_commit={commit}\n"));
        }
        if let Some(verify) = self.config.verify {
            text.push_str(&format!("config_verify={verify}\n"));
        }
        if self.config.chezmoi.is_some() {
            text.push_str(&format!("config_checked_at_ms={}\n", self.checked_at_ms));
        }
        if text != self.written {
            let temporary = self.path.with_extension("tmp");
            if std::fs::write(&temporary, &text).is_ok()
                && std::fs::rename(&temporary, &self.path).is_ok()
            {
                self.written = text;
                self.generation = self.generation.wrapping_add(1);
            }
        }
        self.generation
    }
}

/// `key=value` lines for the facts every machine has, plus `run` for the
/// rare one that needs a program.
pub fn lines(pairs: &[(&str, Option<String>)]) -> String {
    pairs
        .iter()
        .filter_map(|(key, value)| {
            value
                .as_ref()
                .filter(|value| !value.is_empty())
                .map(|value| format!("{key}={value}\n"))
        })
        .collect()
}

pub fn run_text(program: &str, args: &[&str]) -> Option<String> {
    let program = which(
        program,
        &[
            PathBuf::from("/usr/bin").join(program),
            PathBuf::from("/sbin").join(program),
        ],
    )?;
    run(&program, args, None).filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A checkout's commit is read from HEAD, a loose ref, or packed-refs.
    #[test]
    fn the_commit_is_read_without_running_git() {
        let dir = std::env::temp_dir().join(format!("grove-probe-git-{}", std::process::id()));
        let git = dir.join(".git");
        std::fs::create_dir_all(git.join("refs/heads")).unwrap();
        let sha = "0123456789abcdef0123456789abcdef01234567";
        std::fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(
            git.join("packed-refs"),
            format!("# pack-refs\n{sha} refs/heads/main\n"),
        )
        .unwrap();
        assert_eq!(git_head(&dir).as_deref(), Some(sha));
        let newer = "89abcdef0123456789abcdef0123456789abcdef";
        std::fs::write(git.join("refs/heads/main"), format!("{newer}\n")).unwrap();
        assert_eq!(git_head(&dir).as_deref(), Some(newer), "a loose ref wins");
        std::fs::create_dir_all(dir.join("home")).unwrap();
        assert_eq!(
            git_head(&dir.join("home")).as_deref(),
            Some(newer),
            "from a folder inside"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
