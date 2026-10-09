# oats

**oats is a fast computer scheduler and toolkit. All in one.**

oats is an unprivileged Rust CLI for one-time macOS jobs. Schedule commands, set runtime limits, cancel work, and inspect JSON results and logs.

**Awake-only, plugged-in execution.** oats never wakes the Mac or changes global sleep settings. Sleeping jobs wait until the Mac wakes; jobs outside their grace period are marked missed. Your user must be logged in for the LaunchAgent to run.

## Install

Requires macOS, Rust/Cargo, and Apple Command Line Tools (including Python 3 for generating the LaunchAgent plist).

```sh
git clone https://github.com/fayedn/oats.git
cd oats
./scripts/install.sh
export PATH="$HOME/Library/Application Support/oats/bin:$PATH"
oats doctor
```

Run as your normal user, without sudo. Installation creates a private directory at `~/Library/Application Support/oats` containing the binary, state, and logs, plus `~/Library/LaunchAgents/dev.oats.plist`. Add the PATH export to your shell configuration if desired. No root helper or sudoers rule is installed. Installation refuses existing components and archives partial installations if startup fails.

## Schedule work

Plug in the Mac, then:

```sh
oats schedule \
  --at "$(date -u -v+1H '+%Y-%m-%dT%H:%M:%SZ')" \
  --timeout 900 --grace 300 --cwd "$PWD" \
  -- /usr/bin/git status --short
```

Retain `job.id` from the JSON response. Scheduling acknowledges the queue entry, not successful execution.

| Command | Purpose |
| --- | --- |
| `schedule` | Queue a command at an RFC3339 timestamp with timezone |
| `status` | Read agent health, command details, and job history |
| `logs ID` | Read the last 64 KiB of captured output |
| `cancel ID` | Cancel queued work or stop running work |
| `doctor` | Read installation and power diagnostics |
| `probe --after 120` | Queue a harmless date command; keep the Mac awake to test |

Executables and working directories must be absolute paths. Arguments are literal; no shell interpolation occurs. For scripts, pass `/bin/zsh /absolute/path/to/script.zsh`. Commands return JSON; help/version and argument-parser diagnostics are plain text. Errors exit nonzero.

[Agent usage instructions](AGENT_USAGE.md)

## Execution and sleep policy

- One-time schedules, 30 seconds to 366 days ahead; jobs run serially.
- Runtime defaults to 900 seconds, with a 1–14400 second range. Grace defaults to 300 seconds, with a 0–3600 second range.
- While a worker runs, a process-owned IOKit assertion prevents idle system sleep. macOS releases it when the worker exits or crashes. It does not prevent lid-close or explicit sleep, and no assertion is held while waiting for future jobs.
- Sleeping the Mac pauses execution. On wake, overdue queued jobs run only within their grace period. Overlapping work can also cause jobs to miss their grace period. This is not an exact-time scheduler.
- Scheduling requires AC. Jobs due on battery become `skipped_battery`; unplugging a running job causes `stopped_battery`. The agent checks every two seconds while awake; power queries have a five-second execution deadline, so failures can delay detection. Unknown power status fails closed.
- Interrupted jobs are not automatically retried. Cancellation and runtime limits clean up the worker's process group. Commands that deliberately detach or submit work to other services are unsupported.
- A control pipe ties workers to the agent. The agent cleans the worker group even if its supervisor crashes.
- Up to 32 pending jobs and 1,000 retained jobs. Uninstall/reinstall archives history to reset these limits.
- Output capture is capped at 1 MiB per job; excess output is drained and discarded.

Commands inherit your user identity and group access, with a minimal environment: `HOME`, `TMPDIR`, and a fixed PATH including Homebrew. They do not inherit your caller's environment or interactive shell configuration.

**Unprivileged does not mean sandboxed.** Jobs can access files, credentials, and network resources available to your account. Schedule only commands you trust. The state directory is user-owned; other processes running as your user can modify it. oats does not unlock Keychain items, bypass macOS privacy permissions, or resume GUI chat sessions.

## Remove or update

```sh
launchctl print "gui/$(id -u)/dev.oats"
tail -n 100 "$HOME/Library/Application Support/oats/agent.log"
./scripts/uninstall.sh
```

Uninstall stops the user agent, cancels queued jobs, and archives the entire installation beside its original directory with an `.uninstalled.TIMESTAMP.PID` suffix. If cleanup fails, files remain for recovery. To update, uninstall, pull the new source, then install again. Queued jobs are not migrated.

### Migrating from the privileged preview

The current installer refuses to coexist with the previous root service. If you installed that preview, use its original uninstall script **before** installing this version. That older cleanup needs administrator access to restore its recorded sleep setting and remove its hardware wake events. The new binary cannot perform that cleanup.

Retrieve the original script from the private repository history for commit `d435973630f9bc12f23bef08be68f41ef8b9ee43` (`scripts/uninstall.sh`), review it, and run it while the old helper is still installed. Do not manually delete the old helper first. No migration action is necessary if the preview was never installed.

## Development

```sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
bash -n scripts/install.sh scripts/uninstall.sh
```

Tests cover request validation, grace and battery policy, persistence, process cleanup, completion status, runtime limits, output bounds, and temporary user-state integration. Physical sleep/wake and unplug behavior still need on-device verification.

Inspired by [Modafinil](https://github.com/narcotic-sh/modafinil). Independent implementation. MIT licensed; see [LICENSE](LICENSE).
