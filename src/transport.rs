//! How a script reaches a machine: the system `ssh` with the user's own
//! config for a remote machine, `sh` for this one, the script on stdin
//! either way so it never depends on the login shell. Runs are bounded by
//! a timeout and spread over a few threads at a time.

use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::store::Machine;

/// What one run produced.
#[derive(Debug)]
pub struct Ran {
    /// What ran, for failures with nothing on stderr: `ssh` or `sh`.
    pub program: &'static str,
    /// The exit code, or None when the run was killed or died by a signal.
    pub code: Option<i32>,
    pub stdout: String,
    /// What it printed, as bytes, for binary output.
    pub stdout_bytes: Vec<u8>,
    pub stderr: String,
    pub timed_out: Option<Duration>,
    pub elapsed: Duration,
}

impl Ran {
    pub fn ok(&self) -> bool {
        self.code == Some(0)
    }

    /// Why a run failed: the last thing it (or SSH) wrote to stderr, else
    /// how it ended.
    pub fn failure(&self) -> String {
        let program = self.program;
        if let Some(limit) = self.timed_out {
            return format!("timed out after {}ms", limit.as_millis());
        }
        match self
            .stderr
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
        {
            Some(line) => line.to_owned(),
            None => match self.code {
                Some(code) => format!("{program} exited {code}"),
                None => format!("{program} was killed"),
            },
        }
    }
}

/// localhost, 127.0.0.1 and ::1 are this machine, reached without SSH.
pub fn is_local(endpoint: &str) -> bool {
    matches!(
        endpoint.trim().to_ascii_lowercase().as_str(),
        "localhost" | "127.0.0.1" | "::1"
    )
}

/// `GROVE_SSH_COMMAND` split on whitespace, or `ssh`. It is a prefix: ssh
/// keeps the first value it sees for an option, so a caller can put its
/// own `-o ControlPath=…` here and it wins over grove's.
pub fn ssh_prefix() -> Vec<String> {
    let command = std::env::var("GROVE_SSH_COMMAND").unwrap_or_default();
    let words: Vec<String> = command.split_whitespace().map(str::to_owned).collect();
    if words.is_empty() {
        vec!["ssh".into()]
    } else {
        words
    }
}

/// Unix socket paths are capped near 104 bytes, and ssh appends a random
/// suffix to the path while it sets the socket up, so connection sharing
/// is only offered when the home folder is short enough to hold it.
const MAX_CONTROL_DIR: usize = 40;

/// The options every remote run passes, before the endpoint.
pub fn ssh_options(machine: &Machine, home: &Path) -> Vec<String> {
    let mut args: Vec<String> = vec!["-p".into(), machine.port.to_string(), "-T".into()];
    let mut option = |value: String| {
        args.push("-o".into());
        args.push(value);
    };
    option("BatchMode=yes".into());
    option("ConnectTimeout=5".into());
    option("StrictHostKeyChecking=accept-new".into());
    option("ServerAliveInterval=15".into());
    if home.as_os_str().len() <= MAX_CONTROL_DIR {
        option("ControlMaster=auto".into());
        option(format!("ControlPath={}", home.join("cm-%C").display()));
        option("ControlPersist=60".into());
    }
    args
}

/// The command that runs a script read from stdin on `machine`.
fn script_command(machine: &Machine, home: &Path) -> (Command, &'static str) {
    if is_local(&machine.endpoint) {
        return (Command::new("sh"), "sh");
    }
    let prefix = ssh_prefix();
    let mut command = Command::new(&prefix[0]);
    command
        .args(&prefix[1..])
        .args(ssh_options(machine, home))
        .arg("--")
        .arg(machine.endpoint.trim())
        .arg("sh");
    (command, "ssh")
}

/// Single-quotes a word for the remote login shell.
pub fn shell_quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', "'\\''"))
}

/// A command that runs `words` on `machine`: directly for this machine,
/// else as the remote command of an ssh session.
pub fn command_on(machine: &Machine, home: &Path, words: &[String]) -> (Command, &'static str) {
    if is_local(&machine.endpoint) {
        let mut command = Command::new(&words[0]);
        command.args(&words[1..]);
        return (command, "sh");
    }
    let prefix = ssh_prefix();
    let mut command = Command::new(&prefix[0]);
    let remote: Vec<String> = words.iter().map(|word| shell_quote(word)).collect();
    command
        .args(&prefix[1..])
        .args(ssh_options(machine, home))
        .args(["-o", "ServerAliveCountMax=3", "--", machine.endpoint.trim()])
        .arg(format!("exec {}", remote.join(" ")));
    (command, "ssh")
}

/// Runs `script` on `machine`, killing it (and anything it started) when
/// it outlives `timeout`.
pub fn run_script(machine: &Machine, home: &Path, script: &str, timeout: Duration) -> Ran {
    let (mut command, program) = script_command(machine, home);
    run(&mut command, program, Some(script.as_bytes()), timeout)
}

/// Runs a command with optional stdin, collecting its output, within
/// `timeout`.
pub fn run(
    command: &mut Command,
    program: &'static str,
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Ran {
    let started = Instant::now();
    command
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so a timeout takes its children too.
        .process_group(0);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            return Ran {
                program,
                code: None,
                stdout: String::new(),
                stdout_bytes: Vec::new(),
                stderr: error.to_string(),
                timed_out: None,
                elapsed: started.elapsed(),
            };
        }
    };
    let writer = stdin.map(|text| {
        let mut pipe = child.stdin.take();
        let text = text.to_vec();
        std::thread::spawn(move || {
            if let Some(pipe) = pipe.as_mut() {
                let _ = pipe.write_all(&text);
            }
        })
    });
    let stdout = reader(child.stdout.take());
    let stderr = reader(child.stderr.take());
    let (code, timed_out) = wait(&mut child, timeout);
    if let Some(writer) = writer {
        let _ = writer.join();
    }
    let stdout_bytes = stdout.join().unwrap_or_default();
    Ran {
        program,
        code,
        stdout: String::from_utf8_lossy(&stdout_bytes).into_owned(),
        stdout_bytes,
        stderr: String::from_utf8_lossy(&stderr.join().unwrap_or_default()).into_owned(),
        timed_out: timed_out.then_some(timeout),
        elapsed: started.elapsed(),
    }
}

fn reader(pipe: Option<impl Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut bytes);
        }
        bytes
    })
}

fn wait(child: &mut Child, timeout: Duration) -> (Option<i32>, bool) {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return (status.code(), false),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => break,
        }
    }
    if let Ok(pid) = libc::pid_t::try_from(child.id()) {
        // SAFETY: signals only the process group this run started.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    let _ = child.wait();
    (None, true)
}

/// Runs `work` on every item, at most `limit` at a time, and returns the
/// results in the items' order.
pub fn each<T: Sync, R: Send>(items: &[T], limit: usize, work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    use std::sync::Mutex;
    let next = Mutex::new(0_usize);
    let results: Mutex<Vec<Option<R>>> = Mutex::new(items.iter().map(|_| None).collect());
    std::thread::scope(|scope| {
        for _ in 0..limit.clamp(1, items.len().max(1)) {
            scope.spawn(|| {
                loop {
                    let index = {
                        let mut next = next.lock().expect("unpoisoned");
                        let index = *next;
                        *next += 1;
                        index
                    };
                    let Some(item) = items.get(index) else {
                        break;
                    };
                    let result = work(item);
                    results.lock().expect("unpoisoned")[index] = Some(result);
                }
            });
        }
    });
    results
        .into_inner()
        .expect("unpoisoned")
        .into_iter()
        .map(|result| result.expect("every item ran"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run past its timeout is stopped, children and all, and says so.
    #[test]
    fn a_run_past_its_timeout_is_killed_with_its_children() {
        let sh = |script: &str, timeout: Duration| {
            run(
                &mut Command::new("sh"),
                "sh",
                Some(script.as_bytes()),
                timeout,
            )
        };
        let ran = sh("sleep 5 & sleep 5\n", Duration::from_millis(200));
        assert!(ran.elapsed < Duration::from_secs(3), "{:?}", ran.elapsed);
        assert_eq!(ran.failure(), "timed out after 200ms");
        let ran = sh(
            "echo out; echo first >&2; echo last >&2; exit 3\n",
            Duration::from_secs(5),
        );
        assert_eq!((ran.code, ran.stdout.as_str()), (Some(3), "out\n"));
        assert_eq!(ran.failure(), "last");
        assert_eq!(
            sh("exit 4\n", Duration::from_secs(5)).failure(),
            "sh exited 4"
        );
    }
}
