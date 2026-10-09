# oats

**oats is a fast computer scheduler and toolkit. All in one.**

Schedule a wake. Run a command. Let your Mac sleep.

oats is a Rust CLI for agents and people who want to run scheduled work without keeping a laptop awake all day. It combines wake scheduling, command execution, runtime limits, cancellation, logs, and power diagnostics behind one interface.

> **macOS preview:** one-time schedules, plugged-in execution, and JSON output. Closed-lid wake depends on your Mac and setup. Run the probe before relying on unattended jobs.

## Get started

Requires macOS, Rust/Cargo, and Apple Command Line Tools. The installer uses the Command Line Tools' Python 3 to generate configuration; the installed service is a compiled Rust binary.

~~~sh
git clone https://github.com/fayedn/oats.git
cd oats
cargo test --locked
./scripts/install.sh
~~~

Run the installer as your normal user. It builds the binary, then asks for your administrator password to install the helper. Subsequent agent calls do not need a password. If `/usr/local/bin` is not in your PATH, use `/usr/local/bin/oats`.

Plug in your Mac, then schedule a command an hour from now:

~~~sh
oats schedule \
  --at "$(date -u -v+1H '+%Y-%m-%dT%H:%M:%SZ')" \
  --timeout 900 \
  --cwd "$PWD" \
  -- /usr/bin/git status --short
~~~

The response contains `job.id`. Scheduling a job registers its wake; it does not mean the command has run.

~~~sh
oats status
oats logs JOB_ID
oats cancel JOB_ID
oats doctor
~~~

## One CLI, the complete job lifecycle

| Command | Purpose |
| --- | --- |
| `schedule` | Register an absolute wake time and a bounded command |
| `status` | Read service health and job history |
| `logs` | Read captured command output |
| `cancel` | Cancel queued work or stop a running command |
| `doctor` | Inspect power settings and installation status |
| `probe` | Schedule a harmless command to test waking your Mac |

Commands return JSON. Help and version output are plain text. Errors use stderr and a nonzero exit status; sudo can also emit its own diagnostic.

Dates must be RFC3339 timestamps with an explicit timezone offset. Executables and working directories must use absolute paths. Arguments are passed literally, without shell interpolation.

To run a script, pass its interpreter:

~~~sh
oats schedule \
  --at "$(date -u -v+1H '+%Y-%m-%dT%H:%M:%SZ')" \
  --timeout 600 \
  -- /bin/zsh /absolute/path/to/job.zsh
~~~

Use an explicit shell command such as `/bin/zsh -lc '...'` only when shell expansion is needed.

[Agent usage instructions](AGENT_USAGE.md)

## Sleep between jobs

oats registers wake events through macOS's native power-management API using absolute timestamps. When a job is due, the background service checks AC power, temporarily disables sleep, and runs the command as your user. When no due work remains, it restores the previous sleep setting.

The service stays loaded but holds no sleep override while idle. It does not change your display, hibernation, or idle-sleep timers, and it does not force sleep over other apps or user activity. If another app had already disabled sleep, oats preserves that setting.

V1 accepts new schedules only on AC power. A command due while on battery is skipped. Disconnecting AC during execution stops the command after the service detects the change, ordinarily on its next two-second poll. Power-management calls have bounded deadlines, so detection can take longer during a failing control operation.

A previously registered hardware event may still wake the Mac on battery. AC-only is an execution policy, not a condition attached to the firmware alarm.

## Test closed-lid waking

A registered wake does not guarantee useful execution with the lid closed. Firmware and macOS may ignore the event or return to sleep before the service runs. A sleep override applied after waking cannot solve a wake that never happens.

1. Plug in the Mac and run `oats doctor`.
2. Run `oats probe --after 120`; save the job ID and scheduled time.
3. Close the lid promptly. Leave it closed for at least four minutes, beyond the scheduled time and the probe's 60-second grace period.
4. Open it and inspect `oats status` and `oats logs JOB_ID`.
5. Confirm the command succeeded near the scheduled time, before you reopened the lid. Check `pmset -g log` for sleep before the wake; an already-awake Mac does not prove wake support.
6. Check `pmset -g`: `SleepDisabled` should be absent or zero unless it was enabled before the test.

A missed job or a command that only runs after you reopen the lid does not demonstrate closed-lid support. Test the dock, display, and power configuration you actually intend to use. oats does not fall back to keeping the Mac awake indefinitely.

`doctor` deliberately reports `closed_lid_verified: false`: software alone cannot verify that you physically closed the lid and left the Mac asleep.

## Execution rules

- One-time schedules only, 30 seconds to 366 days ahead.
- Up to 32 queued jobs and 1,000 retained jobs. Archive with uninstall/reinstall when the history quota is reached; existing history is not silently deleted.
- Runtime defaults to 15 minutes; allowed range is 1 second to 4 hours.
- Grace defaults to 5 minutes; allowed range is 0–3600 seconds.
- Jobs run serially, ordered by scheduled time. Overlapping jobs may miss their grace period while waiting.
- Jobs outside their grace period become `missed`. Interrupted jobs are not automatically retried.
- On battery when due: `skipped_battery`. Unplugged during execution: `stopped_battery`.
- Cancellation and timeout terminate the command's process group. Commands that deliberately detach or submit work to another service are unsupported.
- A supervisor monitors the daemon's control pipe. The daemon also cleans up the supervisor's process group if the supervisor fails.
- Command output has a shared 1 MiB capture limit. Excess stdout/stderr is drained and discarded. `logs` returns the last 64 KiB of the captured portion.
- Wake registration and cancellation use persisted intent and retryable cleanup. A cancellation can succeed while hardware-event cleanup is still pending.
- Power-control subprocesses have a five-second execution deadline and bounded output collection.

The command environment contains `HOME`, `USER`, `LOGNAME`, `TMPDIR`, and a fixed PATH including Homebrew. It does not inherit interactive shell configuration, caller environment variables, or secrets. Commands run with your UID and primary GID; supplementary groups are cleared.

oats runs terminal commands. It does not resume existing GUI agent conversations, power on a shut-down Mac, bypass FileVault, unlock Keychain items, or grant macOS privacy permissions. An agent must schedule work before the Mac sleeps.

## Installation and removal

The installer creates:

| Component | Location |
| --- | --- |
| CLI | `/usr/local/bin/oats` |
| Root-owned helper | `/Library/PrivilegedHelperTools/dev.oats` |
| LaunchDaemon | `/Library/LaunchDaemons/dev.oats.plist` |
| Private state and logs | `/Library/Application Support/oats` |
| Restricted sudoers rule | `/etc/sudoers.d/oats` |

The sudoers rule allows only the installation owner's UID to invoke the exact helper's request endpoint. Requests are bounded and validated. Agents cannot select another execution UID or gain general passwordless access to shells or `pmset`.

Installation rolls back partial changes where possible. If stopping the service or restoring its state fails, recovery files are retained. Installation refuses to overwrite existing components.

~~~sh
sudo launchctl print system/dev.oats
sudo tail -n 100 '/Library/Application Support/oats/daemon.log'
./scripts/uninstall.sh
~~~

Uninstall revokes the request endpoint, stops the service, restores pending sleep changes, cancels its wake events, and removes installed components. History and logs are archived under `/Library/Application Support/oats.uninstalled.TIMESTAMP`. Cleanup errors stop removal so the recovery helper remains available.

For an update, uninstall, pull the new source, and install again.

## Development and verification

~~~sh
cargo fmt --check
cargo test --locked
cargo clippy --all-targets --locked -- -D warnings
cargo build --release --locked
bash -n scripts/install.sh scripts/uninstall.sh
~~~

Tests cover request validation, power policy, missed jobs, persistent intent, sleep-restoration retries, process-group cleanup, completion status, timeouts, daemon-pipe loss, output limits, and idle database writes.

Privileged installation, native wake registration/cancellation, physical unplugging, and closed-lid wake still require integration testing on the target Mac. Passing unit tests does not establish hardware support.

## Inspiration and references

Inspired by [Modafinil](https://github.com/narcotic-sh/modafinil). oats is an independent implementation.

- [Apple: schedule your Mac with pmset](https://support.apple.com/en-kw/guide/mac-help/mchl40376151/mac)
- [Apple: IOPMSchedulePowerEvent](https://developer.apple.com/documentation/iokit/1557076-iopmschedulepowerevent)
- Local `man pmset` and `man launchd.plist`

MIT licensed. See [LICENSE](LICENSE).
