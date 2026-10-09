# Calling oats from an agent

oats schedules one-time local commands on macOS. The owner must install it once using `scripts/install.sh`. It is AC-only and cannot guarantee closed-lid wake until the owner physically tests a probe.

1. Call `/usr/local/bin/oats doctor` for read-only power diagnostics.
2. Choose an explicit future RFC3339 timestamp with offset, a bounded runtime, an absolute executable, and an absolute working directory.
3. Call `/usr/local/bin/oats schedule --at TIME --timeout SECONDS --cwd DIRECTORY -- EXECUTABLE ARGUMENTS...`.
4. Parse JSON and retain `job.id`. Scheduling registers the wake; it is not evidence that the command has run.
5. Use `oats status`, `oats logs ID`, and `oats cancel ID` to track work.

Do not schedule root commands, remove another app's wake events, change global power settings yourself, or keep the machine awake while waiting. Do not automatically retry a job with side effects. If the helper reports missing installation, an unhealthy service, or no AC power, surface that error to the owner.

Commands run as the installation owner's user, in a minimal environment and without interactive stdin. Pass a script or command that can finish unattended. This tool does not resume existing chat sessions. A command that deliberately daemonizes is unsupported.
