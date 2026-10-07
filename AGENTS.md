# Grove

Grove is a CLI that keeps a registry of machines reachable over SSH and reports their health and capacity: CPU,
memory, disk, network, sensors and running agents. It is a Cargo workspace with two binaries, `grove` at the root and
`grove-probe` (the resident sampler shipped to each machine) in `probe/`, with the toolchain pinned in
`rust-toolchain.toml`. Read `docs/architecture.md` before changing the command surface, the store, the probe or the
stream format.

## Hard rules

- Every command is described once, in `src/catalog.json`. The parser, help, `schema`, `describe` and completions
  derive from it, and a test fails when the parser and the catalog disagree.
- Commands return a `Done` (data plus human text) or an `AppError`. Only `src/cli/mod.rs` writes to stdout or stderr.
- Structured data goes to stdout; diagnostics go to stderr. JSON, CI, piped and non-interactive runs never prompt.
- The envelopes, record shapes (snake_case, ISO times, documented key order), error codes and exit codes are the public
  contract, versioned by `schemaVersion`. Fields are only ever added; a reading a machine can't give is null, never 0.
- The probe's reading, ring and stream formats are versioned in `probe/src/lib.rs`. A change to their layout bumps
  `FORMAT_VERSION`.
- The probe's sampling path starts no other process. Anything that needs one (nvidia-smi, chezmoi) runs on its own
  slower cadence, outside the two-second loop.
- Keep the probe small: no dependency beyond `libc`, and nothing in it that only the collector needs.
- The store at `$GROVE_HOME/grove.db` is shared by every caller. Schema changes are new steps appended to
  `src/store/schema.rs`; released steps are never edited.
- Grove is one-shot: no daemon. Cadence belongs to whatever scheduler runs `grove sample`.
- Grove is its own product. Never name, or shape behavior around, any program that happens to call it.
- Only counts and facts leave a machine: never command lines, serial numbers or file contents.
- Keep the dependency set small. A new crate needs a concrete cross-cutting benefit.
- Keep committed examples, docs, tests and history public-safe: made-up hosts (`cam-mbp`, `cedar-01`), documentation
  addresses (`192.0.2.x`, `100.64.0.x`), no real usernames or paths.

## Tests

- Logic is tested in `#[cfg(test)]` modules beside the code. The installed command line is tested once, through the
  built binary, in `tests/cli.rs`, with `GROVE_SSH_COMMAND` pointing at a stub so nothing leaves the machine.
- Each contract has one owner. Expected values come from a captured real reading or a hand check, never
  from Grove's own output. No golden files or fixture dumps: short inline values only.
- No test-only production seams and no process-wide state (`set_var`, shared temp folders).
- Tests and manual runs use a throwaway `GROVE_HOME` and `HOME`. Never touch a real `~/.grove`, and never register a
  real launchd agent or systemd unit from a test: the install test puts stub service managers first on `PATH`.

## Quality gate

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

Check each exit code; never pipe the gate through `tail` before committing. CI runs the same on Ubuntu and macOS.

Hooks in tests run only harmless commands that write inside the test's temp folder.
