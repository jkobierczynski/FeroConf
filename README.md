# FeroConf

FeroConf backs up, versions and diffs router, switch and firewall configs. Every config change
becomes a commit in a plain Git repository, so the history works with any Git tool too.

**Status:** early development. Cisco backups over SNMP+TFTP work, on demand or triggered by
the router's own config-change SNMP traps.

## Roadmap

1. **Core and storage** (done): TOML inventory, SNMP credentials, Git-backed store that commits
   only on real changes, normalized diffs that ignore timestamps and similar noise.
2. **Cisco backups over SNMP+TFTP** (done): a built-in TFTP server, plus SNMP SETs on
   CISCO-CONFIG-COPY-MIB (or OLD-CISCO-SYS-MIB on old IOS) so each router uploads its
   running-config.
3. **SNMP trap monitoring** (done): `feroconf watch` backs up a router when it sends a
   `ciscoConfigManEvent` trap.
4. **Later**: SSH, NETCONF and gNMI collectors, syslog triggers, a native GUI (egui).

## Build

```sh
cargo build --release
./target/release/feroconf --help
```

## Usage

```sh
cp examples/feroconf.toml feroconf.toml   # then edit it
feroconf check                            # validate the inventory

feroconf watch                            # keep running: back up on config-change traps
feroconf backup                           # back up every device in the inventory once
feroconf backup core-rtr1                 # or only some
feroconf import core-rtr1 core-rtr1.cfg   # store a config from a file instead
feroconf log core-rtr1
feroconf diff core-rtr1                   # previous snapshot vs latest
feroconf diff core-rtr1 HEAD~3            # a revision vs latest
feroconf show core-rtr1 --rev <commit>
```

`--inventory` and `--store` (or `FEROCONF_INVENTORY` and `FEROCONF_STORE`) choose the inventory
file and the store directory. The defaults are `feroconf.toml` and `feroconf-store`.

`feroconf backup` exits with 1 when any device failed, and `feroconf diff` exits with 1 when the configs differ, like `diff(1)`, so it can be used in scripts.

## Backing up Cisco devices (SNMP+TFTP)

For each device, `feroconf backup`:

1. starts its own TFTP server (`backup.tftp_listen`, port 69 by default). It only accepts an
   upload under a random, single-use file name, refuses every download, and keeps the data in
   memory;
2. creates a row in the device's `ccCopyTable` (CISCO-CONFIG-COPY-MIB) that copies
   `running-config` to that file on `tftp_server`, and polls `ccCopyState` until it is done.
   Devices without that MIB get an OLD-CISCO-SYS-MIB `writeNet` SET instead (IPv4 only);
3. normalizes the upload and commits it when it differs from the last stored config.

Port 69 needs root or `CAP_NET_BIND_SERVICE`:

```sh
sudo setcap cap_net_bind_service=+ep "$(which feroconf)"
```

### Router configuration

FeroConf needs SNMP write access to CISCO-CONFIG-COPY-MIB, and the router must be able to reach
`tftp_server` over UDP 69. Allow only the FeroConf host and limit the view to that MIB. In these
examples FeroConf runs on 192.0.2.10.

SNMPv3 (recommended):

```
access-list 99 permit host 192.0.2.10
snmp-server view FEROCONF ciscoConfigCopyMIB included
snmp-server view FEROCONF 1.3.6.1.4.1.9.2.1.55 included
snmp-server group FEROCONF v3 priv read FEROCONF write FEROCONF access 99
snmp-server user feroconf FEROCONF v3 auth sha <auth-password> priv aes 128 <priv-password>
snmp-server tftp-server-list 99
```

with, in `feroconf.toml`:

```toml
[device.snmp]
version = "v3"
user = "feroconf"
auth_protocol = "sha"        # sha256 etc. where the IOS release supports it
auth_password = { file = "/etc/feroconf/snmp-auth" }
priv_protocol = "aes128"
priv_password = { file = "/etc/feroconf/snmp-priv" }
```

SNMPv2c:

```
access-list 99 permit host 192.0.2.10
snmp-server view FEROCONF ciscoConfigCopyMIB included
snmp-server view FEROCONF 1.3.6.1.4.1.9.2.1.55 included
snmp-server community <write-community> view FEROCONF RW 99
snmp-server tftp-server-list 99
```

`snmp-server tftp-server-list` limits which TFTP servers SNMP-started copies may use. A wrong v2c
community usually shows up as "no SNMP response", because IOS silently drops such requests.

## Watching for config changes (SNMP traps)

`feroconf watch` keeps running. It backs up every device at start, then listens for SNMP traps
(`watch.trap_listen`, UDP 162 by default) and backs up a router shortly after it reports a
config change:

- `ciscoConfigManEvent` (CISCO-CONFIG-MAN-MIB) with destination `running` or `startup`, or
  `ccmCLIRunningConfigChanged`, schedules a backup `watch.debounce_secs` later (15 by default).
  More traps in that time push it back, so one editing session becomes one backup.
- A `ciscoConfigManEvent` whose destination is a network server (TFTP, FTP, RCP, SCP) only means
  someone copied the config out. FeroConf's own backups cause exactly that trap, so it is logged
  and ignored instead of starting a loop.
- Other traps (linkDown, coldStart, ...) are logged.
- Traps are matched to devices by their source address (or the v1 agent address). Traps from
  unknown hosts, or with the wrong `watch.trap_community`, are ignored.
- `watch.interval_mins` adds a full backup on a fixed interval, as a safety net for lost traps.

SNMP v1 and v2c traps are supported; informs and SNMPv3 traps are not yet. On the router, next to
the configuration above:

```
snmp-server enable traps config
snmp-server host 192.0.2.10 version 2c <trap-community> config
```

To run it as a service, a systemd unit like this works:

```ini
[Service]
ExecStart=/usr/local/bin/feroconf watch -i /etc/feroconf/feroconf.toml -s /var/lib/feroconf/store
Environment=FEROCONF_RW_COMMUNITY=... FEROCONF_TRAP_COMMUNITY=...
AmbientCapabilities=CAP_NET_BIND_SERVICE
Restart=on-failure
```

### Trying it without a router

`scripts/fake-cisco/run.sh` starts Net-SNMP's `snmpd` as a fake Cisco router on
127.0.0.1:16161 and runs backups against it, then `feroconf watch` with config-change traps
sent by `snmptrap` (needs `snmpd`, `snmpget` and `curl`; on
Debian/Ubuntu `apt install snmpd snmp curl`). CI runs it on every push.

### What counts as a change

Before storing, FeroConf removes lines that change without anyone changing the config:
`! Last configuration change at ...`, `! NVRAM config last updated at ...`,
`Current configuration : N bytes`, `ntp clock-period ...` on IOS, and the `!Time:` style headers on
NX-OS. It also turns CRLF into LF and strips trailing whitespace. A backup that only differs in
those lines makes no commit.

### Secrets

SNMP communities and SNMPv3 passwords can be inline strings, `{ env = "VAR" }` or
`{ file = "/path" }`. `feroconf check` warns about inline secrets.

## Layout

- `crates/feroconf-core`: library shared by the CLI and the future GUI.
- `crates/feroconf-collect`: collectors that fetch configs from devices (SNMP+TFTP today).
- `crates/feroconf-cli`: the `feroconf` binary.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
