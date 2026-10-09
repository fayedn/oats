#!/bin/bash
set -euo pipefail
cd "$(dirname "$0")/.."
if [[ "${1:-}" != --root ]]; then
  [[ "$(uname -s)" == Darwin ]] || { echo 'macOS is required.' >&2; exit 1; }
  [[ "$EUID" != 0 ]] || { echo 'Run this script as your normal user; it will request sudo.' >&2; exit 1; }
  cargo build --release --locked
  exec sudo /bin/bash "$PWD/scripts/install.sh" --root
fi
[[ "$EUID" == 0 && "${SUDO_UID:-0}" != 0 ]] || { echo 'Install through sudo from your account.' >&2; exit 1; }
helper=/Library/PrivilegedHelperTools/dev.oats
state='/Library/Application Support/oats'
plist=/Library/LaunchDaemons/dev.oats.plist
rule=/etc/sudoers.d/oats
for path in "$helper" "$state" "$plist" "$rule" /usr/local/bin/oats; do
  [[ ! -e "$path" && ! -L "$path" ]] || { echo "Refusing to overwrite $path; uninstall the previous installation first." >&2; exit 1; }
done
stage=$(mktemp -d /private/tmp/oats-install.XXXXXX)
install_started=0
install_complete=0
rollback() {
  result=$?
  trap - EXIT
  if [[ "$install_started" == 1 && "$install_complete" == 0 ]]; then
    # Revoke new requests before stopping the service and recovering its state.
    rm -f "$rule"
    if /bin/launchctl print system/dev.oats >/dev/null 2>&1; then
      /bin/launchctl bootout system/dev.oats || {
        echo 'Rollback could not stop the daemon; helper and state retained.' >&2
        rm -rf "$stage"; exit 1
      }
    fi
    if [[ -x "$helper" && -f "$state/config.json" ]]; then
      "$helper" cleanup || {
        echo 'Rollback needs manual recovery; helper and state retained.' >&2
        rm -rf "$stage"; exit 1
      }
    fi
    if [[ -L /usr/local/bin/oats && "$(readlink /usr/local/bin/oats)" == "$helper" ]]; then
      rm /usr/local/bin/oats
    fi
    rm -f "$helper" "$plist"
    # Preserve diagnostics and allow a clean installation attempt next time.
    if [[ -d "$state" ]]; then mv "$state" "${state}.failed.$(date +%s).$$"; fi
  fi
  rm -rf "$stage"
  exit "$result"
}
trap rollback EXIT
/usr/bin/python3 -I - "$stage" "$SUDO_UID" <<'PY'
import json, os, plistlib, pwd, sys
stage, uid = sys.argv[1], int(sys.argv[2])
p = pwd.getpwuid(uid)
assert p.pw_uid > 0 and p.pw_gid > 0
with open(stage+'/config.json', 'w') as f:
    json.dump(dict(uid=p.pw_uid, gid=p.pw_gid, user=p.pw_name, home=p.pw_dir), f)
with open(stage+'/daemon.plist', 'wb') as f:
    plistlib.dump(dict(Label='dev.oats', ProgramArguments=['/Library/PrivilegedHelperTools/dev.oats','daemon'], RunAtLoad=True, KeepAlive=True, ThrottleInterval=5, ExitTimeOut=15, ProcessType='Background', Umask=63, StandardOutPath='/Library/Application Support/oats/daemon.log', StandardErrorPath='/Library/Application Support/oats/daemon.log'), f)
with open(stage+'/sudoers', 'w') as f:
    # Numeric UID avoids sudoers escaping problems with account names.
    f.write(f'#{uid} ALL=(root) NOPASSWD: /Library/PrivilegedHelperTools/dev.oats --request\n')
PY
/usr/sbin/visudo -cf "$stage/sudoers"
/usr/bin/plutil -lint "$stage/daemon.plist"
install_started=1
/usr/bin/install -d -o root -g wheel -m 755 /Library/PrivilegedHelperTools
/usr/bin/install -d -o root -g wheel -m 700 "$state" "$state/logs"
/usr/bin/install -o root -g wheel -m 600 "$stage/config.json" "$state/config.json"
/usr/bin/install -o root -g wheel -m 755 target/release/oats "$helper"
/usr/bin/install -o root -g wheel -m 644 "$stage/daemon.plist" "$plist"
/bin/launchctl bootstrap system "$plist"
# The request endpoint becomes passwordless only after the root service has started.
ready=0
for attempt in {1..10}; do
  if printf '%s' '{"action":"status"}' | "$helper" --request | /usr/bin/python3 -I -c 'import json,sys; sys.exit(not json.load(sys.stdin)["daemon_healthy"])'; then ready=1; break; fi
  sleep 1
done
[[ "$ready" == 1 ]] || { echo 'Service did not start. Inspect the daemon log; run scripts/uninstall.sh before reinstalling.' >&2; exit 1; }
/usr/bin/install -d -o root -g wheel -m 755 /etc/sudoers.d
/usr/bin/install -o root -g wheel -m 440 "$stage/sudoers" "$rule"
# Validate the complete configuration; remove our rule if the check fails.
/usr/sbin/visudo -c || { rm "$rule"; exit 1; }
mkdir -p /usr/local/bin
ln -s "$helper" /usr/local/bin/oats
install_complete=1
printf 'Installed. Run: /usr/local/bin/oats doctor\nThen: /usr/local/bin/oats probe --after 120\n'
