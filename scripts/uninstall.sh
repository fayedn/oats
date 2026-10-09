#!/bin/bash
set -euo pipefail
if [[ "$EUID" != 0 ]]; then exec sudo /bin/bash "$0"; fi
helper=/Library/PrivilegedHelperTools/dev.oats
state='/Library/Application Support/oats'
plist=/Library/LaunchDaemons/dev.oats.plist
rm -f /etc/sudoers.d/oats
if /bin/launchctl print system/dev.oats >/dev/null 2>&1; then
  /bin/launchctl bootout system/dev.oats
fi
# Cleanup must succeed before deleting the helper that restores sleep and cancels its wakes.
if [[ -x "$helper" && -f "$state/config.json" ]]; then "$helper" cleanup; fi
rm -f /etc/sudoers.d/oats "$plist" "$helper"
if [[ -L /usr/local/bin/oats && "$(readlink /usr/local/bin/oats)" == "$helper" ]]; then rm /usr/local/bin/oats; fi
if [[ -d "$state" ]]; then
  archive="${state}.uninstalled.$(date +%s)"
  mv "$state" "$archive"
  printf 'Uninstalled. Job history and logs preserved in %s\n' "$archive"
fi
