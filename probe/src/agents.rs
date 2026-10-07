//! Which processes are agent sessions, decided once per process from its
//! arguments and remembered while the pid lives. The rules are the sample
//! script's: a claude or codex executable (or a Claude build under
//! `claude/versions/`), or node, bun or deno running one of their
//! packages; not Claude's browser bridge, and not a plumbing subcommand.
//! Only the verdict is kept, never the arguments.

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

const PLUMBING: [&str; 12] = [
    "app-server",
    "sandbox",
    "mcp",
    "mcp-server",
    "serve",
    "login",
    "logout",
    "update",
    "doctor",
    "install",
    "config",
    "completion",
];

fn base(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// The first word from `start` that isn't a flag, stepping over the values
/// of -c and --config.
fn subcommand<'a>(words: &[&'a str], start: usize) -> &'a str {
    let mut index = start;
    while let Some(word) = words.get(index) {
        if *word == "-c" || *word == "--config" {
            index += 2;
            continue;
        }
        if word.starts_with('-') {
            index += 1;
            continue;
        }
        return word;
    }
    ""
}

/// `(^|/)(claude|codex)$` or a path through claude-code, @anthropic-ai or
/// @openai/codex.
fn is_agent_package(word: &str) -> bool {
    matches!(base(word), "claude" | "codex")
        || ["/claude-code/", "/@anthropic-ai/", "/@openai/codex/"]
            .iter()
            .any(|part| word.contains(part))
}

fn is_claude_package(word: &str) -> bool {
    base(word) == "claude" || word.contains("/claude-code/") || word.contains("/@anthropic-ai/")
}

/// `/claude/versions/<version>`, the way Claude's native installer names
/// its builds.
fn versioned_claude(path: &str) -> bool {
    path.rfind("/claude/versions/")
        .map(|at| &path[at + "/claude/versions/".len()..])
        .is_some_and(|version| !version.is_empty() && !version.contains('/'))
}

/// Classifies one process from its arguments joined by spaces, as `ps`
/// prints them.
pub fn classify(line: &str) -> Option<Agent> {
    let bridge = " --chrome-native-host";
    if line
        .match_indices(bridge)
        .any(|(at, _)| matches!(line[at + bridge.len()..].chars().next(), None | Some(' ')))
    {
        return None;
    }
    let words: Vec<&str> = line.split_whitespace().collect();
    let first = *words.first()?;
    let exe = base(first);
    let plumbing = |start| {
        let word = subcommand(&words, start);
        !word.is_empty() && PLUMBING.contains(&word)
    };
    if exe == "claude" || versioned_claude(first) {
        return (!plumbing(1)).then_some(Agent::Claude);
    }
    if exe == "codex" {
        return (!plumbing(1)).then_some(Agent::Codex);
    }
    if matches!(exe, "node" | "bun" | "deno") {
        let (index, word) = words
            .iter()
            .enumerate()
            .skip(1)
            .find(|(_, word)| !word.starts_with('-'))?;
        if is_agent_package(word) && !plumbing(index + 1) {
            return Some(if is_claude_package(word) {
                Agent::Claude
            } else {
                Agent::Codex
            });
        }
    }
    None
}

/// What the cache remembers of a pid between readings.
#[derive(Debug, Clone, Copy)]
pub struct Known {
    pub agent: Option<Agent>,
    /// CPU time at the previous reading, for the agent's share since.
    pub cpu_ns: u64,
}

/// Verdicts by pid. A pid that leaves the process list is forgotten, so a
/// reused pid is classified afresh.
#[derive(Default)]
pub struct Cache {
    pub known: HashMap<i32, Known>,
}

impl Cache {
    /// Keeps only the pids still running.
    pub fn retain(&mut self, running: &[i32]) {
        let alive: std::collections::HashSet<i32> = running.iter().copied().collect();
        self.known.retain(|pid, _| alive.contains(pid));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same lines and verdicts as the sample script's own test, which
    /// were checked by hand.
    #[test]
    fn the_probe_classifies_as_the_script_counts() {
        let lines: [(Option<Agent>, &str); 17] = [
            (
                Some(Agent::Claude),
                "/home/cam/.local/bin/claude --dangerously-skip-permissions --resume 0b6c8f1e",
            ),
            (
                Some(Agent::Claude),
                "claude --output-format stream-json --verbose --model claude-opus-5-5[1m]",
            ),
            (Some(Agent::Claude), "claude"),
            (None, "/Users/cam/.local/bin/claude --chrome-native-host"),
            (
                Some(Agent::Codex),
                "/Users/cam/.local/bin/codex -c model_provider=\"hub\" -c model_providers.hub.wire_api=\"responses\"",
            ),
            (
                Some(Agent::Codex),
                "/Users/cam/.local/bin/codex exec -c model_provider=\"hub\"",
            ),
            (
                Some(Agent::Codex),
                "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex exec-server --remote https://example.com/api",
            ),
            (
                None,
                "/home/cam/.local/bin/codex -c features.code_mode_host=true app-server --listen unix://",
            ),
            (None, "codex app-server proxy"),
            (
                None,
                "/home/cam/.codex/packages/standalone/releases/0.160.1-x86_64-unknown-linux-musl/bin/codex-code-mode-host",
            ),
            (
                None,
                "/opt/homebrew/Cellar/node/26.7.0/bin/node /Users/cam/.claude/plugins/cache/openai-codex/codex/1.0.5/scripts/app-server-broker.mjs serve",
            ),
            (
                None,
                "/bin/zsh -c source /Users/cam/.claude/shell-snapshots/snapshot-zsh-1.sh 2>/dev/null || true",
            ),
            (
                None,
                "/Applications/Claude.app/Contents/Helpers/chrome-native-host chrome-extension://abc/",
            ),
            (
                None,
                "/Applications/ChatGPT.app/Contents/Frameworks/Codex Framework.framework/Helpers/Codex (Renderer).app/Contents/MacOS/Codex (Renderer) --type=renderer",
            ),
            (
                Some(Agent::Claude),
                "/usr/bin/node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js -p hi",
            ),
            (Some(Agent::Codex), "node /opt/homebrew/bin/codex"),
            (
                Some(Agent::Claude),
                "/Users/cam/.local/share/claude/versions/2.1.281 --resume",
            ),
        ];
        for (expected, line) in lines {
            assert_eq!(classify(line), expected, "{line}");
        }
    }
}
