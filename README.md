# FeroConf

FeroConf backs up, versions and diffs router, switch and firewall configs. Every config change
becomes a commit in a plain Git repository, so the history works with any Git tool too.

**Status:** early development. The core (inventory, Git-backed store, diffs) and the CLI work.
Fetching configs from devices is next.

## Roadmap

1. **Core and storage** (done): TOML inventory, SNMP credentials, Git-backed store that commits
   only on real changes, normalized diffs that ignore timestamps and similar noise.
2. **Cisco backups over SNMP+TFTP**: a built-in TFTP server, plus SNMP SETs on
   CISCO-CONFIG-COPY-MIB so each router uploads its running-config.
3. **SNMP trap monitoring**: a `ciscoConfigManEvent` trap triggers an immediate backup.
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

# Until device backups land, configs can be imported from files:
feroconf import core-rtr1 core-rtr1.cfg
feroconf log core-rtr1
feroconf diff core-rtr1                   # previous snapshot vs latest
feroconf diff core-rtr1 HEAD~3            # a revision vs latest
feroconf show core-rtr1 --rev <commit>
```

`--inventory` and `--store` (or `FEROCONF_INVENTORY` and `FEROCONF_STORE`) choose the inventory
file and the store directory. The defaults are `feroconf.toml` and `feroconf-store`.

`feroconf diff` exits with 1 when the configs differ, like `diff(1)`, so it can be used in scripts.

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
- `crates/feroconf-cli`: the `feroconf` binary.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
