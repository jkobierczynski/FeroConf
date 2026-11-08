#!/usr/bin/env bash
# End-to-end test without hardware: starts Net-SNMP's snmpd as a fake Cisco router on
# 127.0.0.1:16161, runs the SNMP client tests against it, then runs `feroconf backup` three times
# and checks the store. Needs snmpd, curl and a built feroconf (cargo build).
#
#   scripts/fake-cisco/run.sh            # run the checks, then stop snmpd
#   KEEP=1 scripts/fake-cisco/run.sh     # leave snmpd running to experiment
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
work=$(mktemp -d)
export FAKE_CISCO_STATE=$work/state FAKE_CISCO_TFTP_PORT=6969
export FAKE_CISCO_TRAP_TARGET=127.0.0.1:16162 FAKE_CISCO_TRAP_COMMUNITY=trapcomm
feroconf=${FEROCONF:-$root/target/debug/feroconf}

cat > "$work/snmpd.conf" <<EOF
agentAddress udp:127.0.0.1:16161
rwcommunity private 127.0.0.1
createUser fero SHA-256 authpass123 AES privpass123
rwuser fero priv
pass_persist .1.3.6.1.4.1.9.9.96 $here/cc-copy.sh
EOF
mkdir -p "$work/persist"
snmpd -C -c "$work/snmpd.conf" -Lf "$work/snmpd.log" -p "$work/snmpd.pid" --persistentDir="$work/persist"
stop() { [ -n "${KEEP:-}" ] || kill "$(cat "$work/snmpd.pid")" 2>/dev/null || true; }
trap stop EXIT
for _ in $(seq 20); do
  snmpget -v2c -c private -t 0.5 -r 0 127.0.0.1:16161 1.3.6.1.2.1.1.3.0 >/dev/null 2>&1 && break
  sleep 0.5
done

echo "== SNMP client tests against snmpd"
(cd "$root" && FEROCONF_TEST_SNMPD=127.0.0.1:16161 cargo test -q -p feroconf-collect --test snmpd)

echo "== feroconf backup against the fake router"
cat > "$work/feroconf.toml" <<EOF
[defaults]
tftp_server = "127.0.0.1"

[defaults.snmp]
version = "v2c"
write_community = { env = "FAKE_RW" }

[backup]
tftp_listen = "127.0.0.1:6969"
timeout_secs = 20

[watch]
trap_listen = "127.0.0.1:16162"
trap_community = "trapcomm"
debounce_secs = 2

[[device]]
name = "fake-rtr"
host = "127.0.0.1"
snmp_port = 16161
EOF
export FAKE_RW=private FEROCONF_INVENTORY=$work/feroconf.toml FEROCONF_STORE=$work/store

out=$("$feroconf" backup); echo "$out"
grep -q "fake-rtr: .* Add fake-rtr" <<<"$out"

# Only the timestamp line differs: no new commit.
out=$("$feroconf" backup); echo "$out"
grep -q "fake-rtr: no change" <<<"$out"

echo "no shutdown" > "$FAKE_CISCO_STATE/iface"
out=$("$feroconf" backup); echo "$out"
grep -q "fake-rtr: .* Update fake-rtr (+1 -1)" <<<"$out"

"$feroconf" diff fake-rtr | tee "$work/diff" || true
grep -q "^+ no shutdown" "$work/diff"

echo "== feroconf watch: a config-change trap triggers a backup"
"$feroconf" watch > "$work/watch.log" 2>&1 &
watch_pid=$!
stop() { kill "$watch_pid" 2>/dev/null || true; [ -n "${KEEP:-}" ] || kill "$(cat "$work/snmpd.pid")" 2>/dev/null || true; }
wait_for() { # pattern, seconds
  for _ in $(seq $(($2 * 4))); do grep -q "$1" "$work/watch.log" && return 0; sleep 0.25; done
  echo "timed out waiting for: $1"; cat "$work/watch.log"; return 1
}
wait_for "fake-rtr: no change" 20                    # startup backup, nothing new
wait_for "config copied to a server" 10              # the router's trap about our own copy

echo "shutdown" > "$FAKE_CISCO_STATE/iface"
snmptrap -v2c -c wrong 127.0.0.1:16162 '' 1.3.6.1.4.1.9.9.43.2.0.1 1.3.6.1.4.1.9.9.43.1.1.6.1.5.2 i 3
wait_for "wrong community" 10
snmptrap -v2c -c trapcomm 127.0.0.1:16162 '' 1.3.6.1.4.1.9.9.43.2.0.1 1.3.6.1.4.1.9.9.43.1.1.6.1.5.2 i 3
wait_for "ciscoConfigManEvent trap (running), backup in 2s" 10
wait_for "fake-rtr: .* Update fake-rtr (+1 -1)" 20
sleep 4
cat "$work/watch.log"
# One backup at startup and one after the trap: our own copies must not trigger more.
backups=$(grep -cE "fake-rtr: (no change|[0-9a-f]{10} )" "$work/watch.log")
[ "$backups" -eq 2 ] || { echo "expected 2 backups, got $backups"; exit 1; }
echo "== all good ($work)"
