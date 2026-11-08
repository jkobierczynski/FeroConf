#!/usr/bin/env bash
# Net-SNMP pass_persist handler that pretends to be a Cisco router's CISCO-CONFIG-COPY-MIB.
#
# When a ccCopyTable row is created with createAndGo it uploads a small running-config to the
# TFTP server named in the row with curl, then reports success in ccCopyState. Only meant for
# testing FeroConf without real hardware. State lives in $FAKE_CISCO_STATE.
#
# Write the interface line to "$FAKE_CISCO_STATE/iface" to change the "config", e.g.
#   echo "no shutdown" > "$FAKE_CISCO_STATE/iface"
set -u
ST=${FAKE_CISCO_STATE:?set FAKE_CISCO_STATE}
TFTP_PORT=${FAKE_CISCO_TFTP_PORT:-69}
mkdir -p "$ST"
BASE=.1.3.6.1.4.1.9.9.96.1.1.1.1

upload() {
  sleep 1
  printf 'Building configuration...\r\n! Last configuration change at %s\r\nhostname fake-rtr\r\ninterface Gi0/0\r\n %s\r\nend\r\n' \
    "$(date)" "$(cat "$ST/iface" 2>/dev/null || echo shutdown)" > "$ST/cfg"
  if curl -s -T "$ST/cfg" "tftp://$(cat "$ST/ip"):$TFTP_PORT/$(cat "$ST/file")"; then
    echo 3 > "$ST/state"
  else
    echo 4 > "$ST/state"
  fi
}

while read -r cmd; do
  case "$cmd" in
    PING) echo PONG ;;
    get)
      read -r oid
      rest=${oid#"$BASE".}; col=${rest%%.*}
      # snmpd GETs every object before it SETs it, so every column has to exist.
      case $col in
        10) if [ -f "$ST/state" ]; then printf '%s\ninteger\n%s\n' "$oid" "$(cat "$ST/state")"; else echo NONE; fi ;;
        5) printf '%s\nipaddress\n0.0.0.0\n' "$oid" ;;
        6) printf '%s\nstring\n\n' "$oid" ;;
        *) printf '%s\ninteger\n1\n' "$oid" ;;
      esac ;;
    getnext) read -r _; echo NONE ;;
    set)
      read -r oid; read -r _type val
      rest=${oid#"$BASE".}; col=${rest%%.*}
      case $col in
        5) echo "$val" > "$ST/ip" ;;
        6) echo "$val" | tr -d '"' > "$ST/file" ;;
        14)
          if [ "$val" = 4 ]; then
            echo 2 > "$ST/state"
            # Detach completely, or snmpd waits for the upload before answering the SET.
            ( upload ) </dev/null >/dev/null 2>&1 &
          elif [ "$val" = 6 ]; then
            rm -f "$ST/state"
          fi ;;
      esac
      echo DONE ;;
    "") exit 0 ;;
  esac
done
