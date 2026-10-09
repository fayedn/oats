# Calling oats from an agent

oats schedules one-time local commands while macOS is awake and connected to AC. It never wakes the computer. Install once as the normal user using `scripts/install.sh`; no administrator access is needed.

The executable is `$HOME/Library/Application Support/oats/bin/oats`. Quote that path when invoking it, or add its directory to PATH.

1. Call `oats doctor` for read-only power diagnostics.
2. Choose a future RFC3339 timestamp with offset, a bounded runtime, an absolute executable, and an absolute working directory.
3. Call `oats schedule --at TIME --timeout SECONDS --grace SECONDS --cwd DIRECTORY -- EXECUTABLE ARGUMENTS...`.
4. Parse JSON and retain `job.id`. The response confirms queueing only.
5. Use `oats status`, `oats logs ID`, and `oats cancel ID` to track work.

Jobs sleeping past their grace period are missed. Jobs due on battery are skipped. Surface missing installation, unhealthy agent, or power errors to the owner. Do not automatically retry side-effecting work.

Commands run with your user's permissions in a minimal environment, without interactive stdin. They are not sandboxed. Review executable, arguments, and working directory before scheduling. Pass work that finishes unattended; deliberately daemonizing commands are unsupported. oats does not resume existing chat sessions or provide root access.
