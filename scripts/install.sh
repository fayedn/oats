#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
[[ "$(uname -s)" == Darwin ]] || { echo 'macOS is required.' >&2; exit 1; }
[[ "$EUID" != 0 ]] || { echo 'Run as your normal user, without sudo.' >&2; exit 1; }
# Never leave an older privileged service running alongside the user agent.
for old in /Library/PrivilegedHelperTools/dev.oats /Library/LaunchDaemons/dev.oats.plist /etc/sudoers.d/oats; do
  [[ ! -e "$old" ]] || { echo 'Remove the privileged preview using its original uninstall script first. See README.' >&2; exit 1; }
done
state="$HOME/Library/Application Support/oats"
plist="$HOME/Library/LaunchAgents/dev.oats.plist"
[[ ! -e "$state" && ! -L "$state" && ! -e "$plist" && ! -L "$plist" ]] || { echo 'Uninstall the existing user installation first; history will be archived.' >&2; exit 1; }
cargo build --release --locked
umask 077
mkdir -p "$HOME/Library/LaunchAgents"
mkdir "$state"
complete=0
rollback() {
  result=$?
  trap - EXIT
  if [[ "$complete" == 0 ]]; then
    if /bin/launchctl print "gui/$EUID/dev.oats" >/dev/null 2>&1; then
      /bin/launchctl bootout "gui/$EUID/dev.oats" || { echo 'Could not stop agent; files retained.' >&2; exit 1; }
    fi
    if [[ -x "$state/bin/oats" && -d "$state/logs" ]]; then
      cleaned=0
      for attempt in {1..20}; do
        if "$state/bin/oats" cleanup; then cleaned=1; break; fi
        sleep 1
      done
      [[ "$cleaned" == 1 ]] || { echo 'Rollback cleanup failed; files retained.' >&2; exit 1; }
    fi
    rm -f "$plist"
    mv "$state" "${state}.failed.$(date +%s).$$"
  fi
  exit "$result"
}
trap rollback EXIT
mkdir "$state/bin" "$state/logs"
/usr/bin/install -m 700 target/release/oats "$state/bin/oats"
/usr/bin/python3 -I - "$state" "$plist" <<'PY'
import plistlib, sys
state, plist = sys.argv[1:]
with open(plist, 'wb') as f:
    plistlib.dump(dict(Label='dev.oats', ProgramArguments=[state+'/bin/oats','daemon'], RunAtLoad=True, KeepAlive=True, ThrottleInterval=5, ExitTimeOut=15, ProcessType='Background', Umask=63, EnvironmentVariables={'HOME': __import__('os').path.expanduser('~')}, StandardOutPath=state+'/agent.log', StandardErrorPath=state+'/agent.log'), f)
PY
/usr/bin/plutil -lint "$plist"
/bin/launchctl bootstrap "gui/$EUID" "$plist"
ready=0
for attempt in {1..10}; do
  if "$state/bin/oats" status | /usr/bin/python3 -I -c 'import json,sys; sys.exit(not json.load(sys.stdin)["daemon_healthy"])'; then ready=1; break; fi
  sleep 1
done
[[ "$ready" == 1 ]] || { echo 'Agent did not become healthy.' >&2; exit 1; }
complete=1
printf 'Installed without administrator access. Run: "%s/bin/oats" doctor\nAdd "%s/bin" to PATH to use oats directly.\n' "$state" "$state"
