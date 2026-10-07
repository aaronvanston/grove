//! The sample script: one portable POSIX sh program, fed to `sh` on stdin,
//! that reads a machine in a single round trip and prints `key=value`
//! lines, so nothing on the machine needs to quote JSON or have anything
//! installed.
//!
//! LC_ALL=C pins number formatting and tool output, because a comma
//! decimal separator or a translated label would change what the parser
//! reads. Core readings are mandatory and a failure to produce one fails
//! the sample (`set -e`). Everything else is a sensor a machine may not
//! have, so each optional probe is guarded and prints no line when it
//! finds nothing: absent means unknown, never zero.
//!
//! - CPU is the `ps` decaying average summed over processes and divided by
//!   the core count; on Linux the /proc/stat jiffies are printed too, so
//!   two readings in a row give the real busy share between them.
//! - Disk is measured on the APFS Data volume on macOS, because the root
//!   mount is the sealed system snapshot, and used is total minus
//!   available so it reflects what can still be written.
//! - Temperatures and GPU load come from macmon on a Mac when it is
//!   installed, and from hwmon, thermal zones, nvidia-smi or the DRM
//!   device on Linux.
//! - On a Mac only the `model` and `product-name` lines of ioreg's output
//!   are kept: the serial number and UUID it prints beside them never
//!   leave the machine.
//! - Agents are counted from the process table, which only ever leaves the
//!   machine as two numbers, never as command lines.
//! - Config management (chezmoi) reports the commit its source clone is
//!   on and whether the files it wrote still match it.

/// Counts agent sessions from `ps` argument lines on stdin and prints
/// "claude codex".
///
/// A session is a claude or codex executable (including a Claude build
/// installed under a `claude/versions/<version>` path), or node, bun or
/// deno running one of their packages. The runner case matches the
/// package path rather than any mention of the name, because agents keep
/// worktrees under folders like .claude and every dev server started
/// inside one would otherwise count.
///
/// Claude's browser bridge (`--chrome-native-host`) is not a session,
/// nor are subcommands that serve an editor or a
/// desktop app, which are plumbing: the first non-flag argument decides, and `-c`/`--config`
/// take a value, so it is stepped over rather than read as the subcommand.
macro_rules! agents_awk {
    () => {
        r#"function base(p) { sub(".*/", "", p); return p }
function subcommand(start,   i) {
  for (i = start; i <= NF; i++) {
    if ($i == "-c" || $i == "--config") { i++; continue }
    if (substr($i, 1, 1) == "-") continue
    return $i
  }
  return ""
}
BEGIN {
  split("app-server sandbox mcp mcp-server serve login logout update doctor install config completion", names, " ")
  for (name in names) plumbing[names[name]] = 1
  pkg = "(^|/)(claude|codex)$|/(claude-code|@anthropic-ai|@openai/codex)/"
  claude_pkg = "(^|/)claude$|/(claude-code|@anthropic-ai)/"
}
/ --chrome-native-host( |$)/ { next }
{
  exe = base($1)
  if (exe == "claude" || $1 ~ /\/claude\/versions\/[^\/]+$/) {
    if (!(subcommand(2) in plumbing)) claude++
    next
  }
  if (exe == "codex") {
    if (!(subcommand(2) in plumbing)) codex++
    next
  }
  if (exe == "node" || exe == "bun" || exe == "deno") {
    for (i = 2; i <= NF; i++) {
      if (substr($i, 1, 1) == "-") continue
      if ($i ~ pkg && !(subcommand(i + 1) in plumbing)) {
        if ($i ~ claude_pkg) claude++; else codex++
      }
      break
    }
  }
}
END { printf "%d %d", claude + 0, codex + 0 }"#
    };
}

#[cfg(test)]
pub const AGENTS_AWK: &str = agents_awk!();

/// The whole script is one brace group, so `sh` reads all of it from
/// stdin before running any of it, and no probe can read the rest of the
/// script as its own input.
pub const SAMPLE_SCRIPT: &str = concat!(
    r#"{
set -eu
export LC_ALL=C
opt() { if [ -n "$2" ]; then printf '%s=%s\n' "$1" "$2"; fi; }
os=$(uname -s); arch=$(uname -m); host=$(uname -n)
cores=$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 1)
if [ -r /proc/loadavg ]; then
  loadavg=$(cat /proc/loadavg)
  l1=$(printf '%s' "$loadavg" | awk '{ print $1 }')
  l5=$(printf '%s' "$loadavg" | awk '{ print $2 }')
  l15=$(printf '%s' "$loadavg" | awk '{ print $3 }')
else
  loadavg=$(sysctl -n vm.loadavg)
  l1=$(printf '%s' "$loadavg" | awk '{ print $2 }')
  l5=$(printf '%s' "$loadavg" | awk '{ print $3 }')
  l15=$(printf '%s' "$loadavg" | awk '{ print $4 }')
fi
cpu=$(ps -A -o %cpu= | awk -v c="$cores" '{ s += $1 } END { if (c < 1) c = 1; v = s / c; if (v > 100) v = 100; printf "%.1f", v }')
swap=""; uptime_s=""; batt=""; cpu_temp=""; gpu_temp=""; cpu_total=""; cpu_idle=""
ip=""; model=""; product=""; chip=""; os_version=""
gpu_name=""; gpu_util=""; gpu_mem_used=""; gpu_mem_total=""
if [ "$os" = "Darwin" ]; then
  mem_total=$(( $(sysctl -n hw.memsize) / 1024 ))
  page=$(sysctl -n hw.pagesize)
  mem_avail=$(vm_stat | awk -v p="$page" -F'[: .]+' '/^Pages free/ { f = $3 } /^Pages inactive/ { i = $3 } /^Pages speculative/ { s = $3 } END { printf "%d", (f + i + s) * p / 1024 }')
  net=$(netstat -ibn | awk '$3 ~ /Link/ && $1 != "lo0" { rx += $(NF-4); tx += $(NF-1) } END { printf "%d %d", rx, tx }')
  boot=$(sysctl -n kern.boottime 2>/dev/null | awk '{ for (i = 1; i <= NF; i++) if ($i == "sec") { gsub(/[^0-9]/, "", $(i + 2)); print $(i + 2); exit } }' || true)
  if [ -n "$boot" ]; then uptime_s=$(awk -v b="$boot" -v n="$(date +%s)" 'BEGIN { printf "%d", n - b }'); fi
  swap=$(sysctl -n vm.swapusage 2>/dev/null | awk '{ t = $3; u = $6; sub(/[A-Za-z]$/, "", t); sub(/[A-Za-z]$/, "", u); printf "%d %d", t * 1024, u * 1024 }' || true)
  batt=$(pmset -g batt 2>/dev/null | awk -F'[;\t]' '/InternalBattery/ { gsub(/%/, "", $2); gsub(/^[ \t]+|[ \t]+$/, "", $3); printf "%s %s", $2, $3; exit }' || true)
  model=$(sysctl -n hw.model 2>/dev/null || true)
  product=$(ioreg -rd1 -n product 2>/dev/null | awk -F'"' '$2 == "product-name" { print $4; exit }' || true)
  chip=$(sysctl -n machdep.cpu.brand_string 2>/dev/null || true)
  os_version=$(sw_vers -productVersion 2>/dev/null || true)
  if [ "$arch" = "arm64" ]; then gpu_name="$chip"; fi
  mm=""
  for c in macmon /opt/homebrew/bin/macmon /usr/local/bin/macmon; do
    if command -v "$c" >/dev/null 2>&1; then mm=$("$c" pipe -s 1 -i 200 2>/dev/null | head -n 1 || true); break; fi
  done
  if [ -n "$mm" ]; then
    cpu_temp=$(printf '%s' "$mm" | sed -n 's/.*"cpu_temp_avg":\([0-9][0-9]*\(\.[0-9]*\)\{0,1\}\).*/\1/p' | awk '{ printf "%.1f", $1 }')
    gpu_temp=$(printf '%s' "$mm" | sed -n 's/.*"gpu_temp_avg":\([0-9][0-9]*\(\.[0-9]*\)\{0,1\}\).*/\1/p' | awk '{ printf "%.1f", $1 }')
    gpu_util=$(printf '%s' "$mm" | sed -n 's/.*"gpu_active_ratio":\([0-9][0-9]*\(\.[0-9]*\)\{0,1\}\).*/\1/p' | awk '{ v = $1 * 100; if (v > 100) v = 100; printf "%.1f", v }')
  fi
  iface=$(route -n get default 2>/dev/null | awk '/interface:/ { print $2; exit }' || true)
  if [ -z "$iface" ]; then iface=en0; fi
  ip=$(ipconfig getifaddr "$iface" 2>/dev/null || true)
else
  mem_total=$(awk '/^MemTotal/ { print $2 }' /proc/meminfo)
  mem_avail=$(awk '/^MemAvailable/ { print $2 }' /proc/meminfo)
  if [ -r /proc/stat ]; then
    cpu_line=$(head -n 1 /proc/stat)
    cpu_total=$(printf '%s' "$cpu_line" | awk '{ printf "%d", $2 + $3 + $4 + $5 + $6 + $7 + $8 + $9 }')
    cpu_idle=$(printf '%s' "$cpu_line" | awk '{ printf "%d", $5 + $6 }')
  fi
  net=$(awk 'NR > 2 { gsub(/:/, " "); if ($1 != "lo") { rx += $2; tx += $10 } } END { printf "%d %d", rx, tx }' /proc/net/dev)
  uptime_s=$(awk '{ printf "%d", $1 }' /proc/uptime 2>/dev/null || true)
  swap=$(awk '/^SwapTotal/ { t = $2 } /^SwapFree/ { f = $2 } END { if (t == "") exit 1; printf "%d %d", t, t - f }' /proc/meminfo || true)
  for b in /sys/class/power_supply/BAT*; do
    if [ -r "$b/capacity" ]; then
      cap=$(cat "$b/capacity" 2>/dev/null || true)
      state=$(cat "$b/status" 2>/dev/null || echo unknown)
      if [ -n "$cap" ]; then batt="$cap $state"; break; fi
    fi
  done
  for h in /sys/class/hwmon/hwmon*; do
    [ -r "$h/name" ] || continue
    case "$(cat "$h/name" 2>/dev/null || true)" in
      coretemp|k10temp|zenpower|cpu_thermal|soc_thermal)
        v=$(cat "$h/temp1_input" 2>/dev/null || true)
        if [ -n "$v" ]; then cpu_temp=$(awk -v v="$v" 'BEGIN { printf "%.1f", v / 1000 }'); break; fi
        ;;
    esac
  done
  if [ -z "$cpu_temp" ]; then
    for z in /sys/class/thermal/thermal_zone*; do
      [ -r "$z/type" ] || continue
      case "$(cat "$z/type" 2>/dev/null || true)" in
        x86_pkg_temp|cpu-thermal|cpu_thermal)
          v=$(cat "$z/temp" 2>/dev/null || true)
          if [ -n "$v" ]; then cpu_temp=$(awk -v v="$v" 'BEGIN { printf "%.1f", v / 1000 }'); break; fi
          ;;
      esac
    done
  fi
  model=$(cat /sys/class/dmi/id/product_name 2>/dev/null || cat /sys/firmware/devicetree/base/model 2>/dev/null || true)
  model=$(printf '%s' "$model" | tr -d '\000' | awk 'NR == 1 { print }')
  chip=$(awk -F': ' '/^model name/ { print $2; exit } /^Model/ { print $2; exit }' /proc/cpuinfo 2>/dev/null || true)
  os_version=$(awk -F= '/^PRETTY_NAME=/ { gsub(/"/, "", $2); print $2; exit }' /etc/os-release 2>/dev/null || true)
  if [ -z "$os_version" ]; then os_version=$(uname -r); fi
  ip=$(ip -4 route get 1.1.1.1 2>/dev/null | awk '{ for (i = 1; i < NF; i++) if ($i == "src") { print $(i + 1); exit } }' || true)
  if [ -z "$ip" ]; then ip=$(hostname -I 2>/dev/null | awk '{ print $1 }' || true); fi
  if command -v nvidia-smi >/dev/null 2>&1; then
    nv=$(nvidia-smi --query-gpu=name,utilization.gpu,temperature.gpu,memory.used,memory.total --format=csv,noheader,nounits 2>/dev/null | head -n 1 || true)
    if [ -n "$nv" ]; then
      gpu_name=$(printf '%s' "$nv" | awk -F', ' '{ print $1 }')
      gpu_util=$(printf '%s' "$nv" | awk -F', ' '$2 ~ /^[0-9]/ { print $2 }')
      gpu_temp=$(printf '%s' "$nv" | awk -F', ' '$3 ~ /^[0-9]/ { print $3 }')
      gpu_mem_used=$(printf '%s' "$nv" | awk -F', ' '$4 ~ /^[0-9]/ { print $4 }')
      gpu_mem_total=$(printf '%s' "$nv" | awk -F', ' '$5 ~ /^[0-9]/ { print $5 }')
    fi
  fi
  if [ -z "$gpu_name" ]; then
    for c in /sys/class/drm/card[0-9]; do
      [ -r "$c/device/vendor" ] || continue
      vendor=$(cat "$c/device/vendor" 2>/dev/null || true)
      case "$vendor" in
        0x1002|0x10de) ;;
        *) continue ;;
      esac
      busy=$(cat "$c/device/gpu_busy_percent" 2>/dev/null || true)
      if [ -n "$busy" ]; then gpu_util="$busy"; fi
      for h in "$c"/device/hwmon/hwmon*; do
        v=$(cat "$h/temp1_input" 2>/dev/null || true)
        if [ -n "$v" ]; then gpu_temp=$(awk -v v="$v" 'BEGIN { printf "%.1f", v / 1000 }'); break; fi
      done
      case "$vendor" in 0x1002) brand="AMD"; match="AMD|ATI" ;; *) brand="NVIDIA"; match="NVIDIA" ;; esac
      if command -v lspci >/dev/null 2>&1; then
        gpu_name=$(lspci -mm 2>/dev/null | awk -F'" "' -v m="$match" -v b="$brand" 'tolower($1) ~ /vga|3d|display/ && $2 ~ m { n = $3; sub(/".*/, "", n); if (match(n, /\[[^]]+\]/)) n = substr(n, RSTART + 1, RLENGTH - 2); print b " " n; exit }' || true)
      fi
      if [ -z "$gpu_name" ]; then gpu_name="$brand GPU"; fi
      break
    done
  fi
  if [ -z "$gpu_temp" ]; then
    for h in /sys/class/hwmon/hwmon*; do
      [ -r "$h/name" ] || continue
      if [ "$(cat "$h/name" 2>/dev/null || true)" = "amdgpu" ]; then
        v=$(cat "$h/temp1_input" 2>/dev/null || true)
        if [ -n "$v" ]; then gpu_temp=$(awk -v v="$v" 'BEGIN { printf "%.1f", v / 1000 }'); break; fi
      fi
    done
  fi
fi
rx=$(printf '%s' "$net" | awk '{ print $1 }')
tx=$(printf '%s' "$net" | awk '{ print $2 }')
swap_total=$(printf '%s' "$swap" | awk '{ print $1 }')
swap_used=$(printf '%s' "$swap" | awk '{ print $2 }')
batt_pct=$(printf '%s' "$batt" | awk '{ print $1 }')
batt_state=$(printf '%s' "$batt" | awk '{ $1 = ""; sub(/^ +/, ""); print }')
disk_path=/
if [ "$os" = "Darwin" ] && [ -d /System/Volumes/Data ]; then
  disk_path=/System/Volumes/Data
fi
disk=$(df -kP "$disk_path" | awk 'NR == 2 { printf "%d %d", $2, $2 - $4 }')
dtotal=$(printf '%s' "$disk" | awk '{ print $1 }')
dused=$(printf '%s' "$disk" | awk '{ print $2 }')
agents=""; claude_sessions=""; codex_sessions=""; agent_sessions=""
if procs=$(ps ax -ww -o args= 2>/dev/null); then
  agents=$(printf '%s\n' "$procs" | awk '"#,
    agents_awk!(),
    r#"' || true)
fi
if [ -n "$agents" ]; then
  claude_sessions=$(printf '%s' "$agents" | awk '{ print $1 }')
  codex_sessions=$(printf '%s' "$agents" | awk '{ print $2 }')
  agent_sessions=$((claude_sessions + codex_sessions))
fi
config_commit=""; config_verify=""; chezmoi=""
for candidate in chezmoi "$HOME/.local/bin/chezmoi" /opt/homebrew/bin/chezmoi /usr/local/bin/chezmoi; do
  if command -v "$candidate" >/dev/null 2>&1; then chezmoi="$candidate"; break; fi
done
if [ -n "$chezmoi" ]; then
  source_dir=$("$chezmoi" source-path 2>/dev/null || true)
  if [ -n "$source_dir" ]; then
    config_commit=$(git -C "$source_dir" rev-parse HEAD 2>/dev/null || true)
  fi
  if [ -n "$config_commit" ]; then
    if "$chezmoi" verify >/dev/null 2>&1; then config_verify=0; else config_verify=$?; fi
  fi
fi
printf 'hostname=%s\nos=%s\narch=%s\ncores=%s\nload1=%s\nload5=%s\nload15=%s\ncpu_pct=%s\nmem_total_kb=%s\nmem_available_kb=%s\ndisk_total_kb=%s\ndisk_used_kb=%s\nnet_rx_bytes=%s\nnet_tx_bytes=%s\n' \
  "$host" "$os" "$arch" "$cores" "$l1" "$l5" "$l15" "$cpu" "$mem_total" "$mem_avail" "$dtotal" "$dused" "$rx" "$tx"
opt cpu_total_jiffies "$cpu_total"
opt cpu_idle_jiffies "$cpu_idle"
opt ip "$ip"
opt model "$model"
opt product_name "$product"
opt chip "$chip"
opt os_version "$os_version"
opt uptime_s "$uptime_s"
opt swap_total_kb "$swap_total"
opt swap_used_kb "$swap_used"
opt cpu_temp_c "$cpu_temp"
opt gpu_name "$gpu_name"
opt gpu_temp_c "$gpu_temp"
opt gpu_util_pct "$gpu_util"
opt gpu_mem_used_mb "$gpu_mem_used"
opt gpu_mem_total_mb "$gpu_mem_total"
opt battery_pct "$batt_pct"
opt battery_state "$batt_state"
opt agent_sessions "$agent_sessions"
opt claude_sessions "$claude_sessions"
opt codex_sessions "$codex_sessions"
opt config_commit "$config_commit"
opt config_verify "$config_verify"
}
"#
);

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::process::{Command, Stdio};

    use super::*;

    fn feed(program: &str, args: &[&str], input: &str) -> String {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("starts");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input.as_bytes())
            .expect("writes");
        let output = child.wait_with_output().expect("finishes");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// `ps` lines captured from real machines, with names replaced, and
    /// whether each is a session, split by which agent the line runs.
    #[test]
    fn agents_are_counted_and_split_by_agent() {
        let lines: [(&str, &str); 17] = [
            (
                "1 0",
                "/home/cam/.local/bin/claude --dangerously-skip-permissions --resume 0b6c8f1e",
            ),
            (
                "1 0",
                "claude --output-format stream-json --verbose --model claude-opus-5-5[1m]",
            ),
            ("1 0", "claude"),
            ("0 0", "/Users/cam/.local/bin/claude --chrome-native-host"),
            (
                "0 1",
                "/Users/cam/.local/bin/codex -c model_provider=\"hub\" -c model_providers.hub.wire_api=\"responses\"",
            ),
            (
                "0 1",
                "/Users/cam/.local/bin/codex exec -c model_provider=\"hub\"",
            ),
            (
                "0 1",
                "/Applications/ChatGPT.app/Contents/Resources/codex-cli/CodexCLI.app/Contents/MacOS/codex exec-server --remote https://example.com/api",
            ),
            (
                "0 0",
                "/home/cam/.local/bin/codex -c features.code_mode_host=true app-server --listen unix://",
            ),
            ("0 0", "codex app-server proxy"),
            (
                "0 0",
                "/home/cam/.codex/packages/standalone/releases/0.160.1-x86_64-unknown-linux-musl/bin/codex-code-mode-host",
            ),
            (
                "0 0",
                "/opt/homebrew/Cellar/node/26.7.0/bin/node /Users/cam/.claude/plugins/cache/openai-codex/codex/1.0.5/scripts/app-server-broker.mjs serve",
            ),
            (
                "0 0",
                "/bin/zsh -c source /Users/cam/.claude/shell-snapshots/snapshot-zsh-1.sh 2>/dev/null || true",
            ),
            (
                "0 0",
                "/Applications/Claude.app/Contents/Helpers/chrome-native-host chrome-extension://abc/",
            ),
            (
                "0 0",
                "/Applications/ChatGPT.app/Contents/Frameworks/Codex Framework.framework/Helpers/Codex (Renderer).app/Contents/MacOS/Codex (Renderer) --type=renderer",
            ),
            (
                "1 0",
                "/usr/bin/node /usr/lib/node_modules/@anthropic-ai/claude-code/cli.js -p hi",
            ),
            ("0 1", "node /opt/homebrew/bin/codex"),
            (
                "1 0",
                "/Users/cam/.local/share/claude/versions/2.1.281 --resume",
            ),
        ];
        for (expected, line) in lines {
            assert_eq!(
                feed("awk", &[AGENTS_AWK], &format!("{line}\n")),
                expected,
                "{line}"
            );
        }
        let all: Vec<&str> = lines.iter().map(|(_, line)| *line).collect();
        assert_eq!(feed("awk", &[AGENTS_AWK], &all.join("\n")), "5 4");
    }

    /// The script runs on this machine under sh, and under dash where it is
    /// installed (Debian and Ubuntu run sh scripts with it), fed on stdin.
    #[test]
    fn the_script_reads_this_machine_under_every_shell() {
        let shells = ["sh", "dash"].into_iter().filter(|shell| {
            Command::new("sh")
                .args(["-c", &format!("command -v {shell}")])
                .output()
                .is_ok_and(|out| out.status.success())
        });
        for shell in shells {
            let output = feed(shell, &[], SAMPLE_SCRIPT);
            let parsed = crate::reading::parse(&output, "here", 0)
                .unwrap_or_else(|key| panic!("{shell}: {key}\n{output}"));
            assert!(parsed.sample.mem_total_kb > 0.0, "{shell}");
            assert!(
                parsed.sample.agent_sessions.is_some(),
                "{shell}: agents are counted even when none run"
            );
        }
    }
}
