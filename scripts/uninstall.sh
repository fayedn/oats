#!/bin/bash
set -euo pipefail
[[ "$EUID" != 0 ]] || { echo 'Run as your normal user, without sudo.' >&2; exit 1; }
state="$HOME/Library/Application Support/oats"
plist="$HOME/Library/LaunchAgents/dev.oats.plist"
if /bin/launchctl print "gui/$EUID/dev.oats" >/dev/null 2>&1; then
  /bin/launchctl bootout "gui/$EUID/dev.oats"
fi
if [[ -x "$state/bin/oats" ]]; then
  # bootout may return before shutdown has released the daemon lock.
  cleaned=0
  for attempt in {1..20}; do
    if "$state/bin/oats" cleanup; then cleaned=1; break; fi
    sleep 1
  done
  [[ "$cleaned" == 1 ]] || { echo 'Cleanup failed; installation retained.' >&2; exit 1; }
fi
rm -f "$plist"
if [[ -d "$state" ]]; then
  archive="${state}.uninstalled.$(date +%s).$$"
  mv "$state" "$archive"
  printf 'Uninstalled. History and logs preserved in %s\n' "$archive"
fi
