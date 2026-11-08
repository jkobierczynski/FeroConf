//! Remove lines that change without anyone changing the config.
//!
//! Without this, every backup would look like a change because of timestamps, byte counts or
//! values the device recalculates by itself. Normalization also turns CRLF into LF, strips
//! trailing whitespace and ends the text with exactly one newline.

use std::sync::OnceLock;

use regex::RegexSet;

use crate::inventory::Vendor;

fn cisco_ios_rules() -> &'static RegexSet {
    static RULES: OnceLock<RegexSet> = OnceLock::new();
    RULES.get_or_init(|| {
        RegexSet::new([
            r"^Building configuration\.\.\.$",
            r"^Current configuration\s*:\s*\d+ bytes$",
            r"^! Last configuration change at ",
            r"^! NVRAM config last updated at ",
            r"^! No configuration change since last restart",
            // IOS recalculates this on its own; it is safe to drop on restore.
            r"^ntp clock-period \d+$",
        ])
        .expect("valid IOS rules")
    })
}

fn cisco_nxos_rules() -> &'static RegexSet {
    static RULES: OnceLock<RegexSet> = OnceLock::new();
    RULES.get_or_init(|| {
        RegexSet::new([
            r"^!Command: show running-config",
            r"^!Running configuration last done at:",
            r"^!Time: ",
        ])
        .expect("valid NX-OS rules")
    })
}

/// Normalize a config for storage and comparison.
pub fn normalize(vendor: Vendor, raw: &str) -> String {
    let rules = match vendor {
        Vendor::CiscoIos | Vendor::CiscoIosXe => cisco_ios_rules(),
        Vendor::CiscoNxos => cisco_nxos_rules(),
    };

    let mut out = String::with_capacity(raw.len());
    for line in raw.lines() {
        let line = line.trim_end();
        if rules.is_match(line) {
            continue;
        }
        // Drop blank lines before the first real line.
        if out.is_empty() && line.is_empty() {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    while out.ends_with("\n\n") {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_ios_noise() {
        let raw = "\r\nBuilding configuration...\r\n\r\nCurrent configuration : 1234 bytes\r\n!\r\n\
                   ! Last configuration change at 10:01:02 UTC Sun Nov 8 2026 by admin\r\n\
                   ! NVRAM config last updated at 09:00:00 UTC Sun Nov 8 2026 by admin\r\n\
                   version 15.9   \r\nhostname core-rtr1\r\nntp clock-period 17179870\r\nend\r\n\r\n";
        assert_eq!(
            normalize(Vendor::CiscoIos, raw),
            "!\nversion 15.9\nhostname core-rtr1\nend\n"
        );
    }

    #[test]
    fn timestamp_only_change_normalizes_equal() {
        let a = "! Last configuration change at 10:00:00 UTC\nhostname r1\nend\n";
        let b = "! Last configuration change at 11:30:00 UTC\nhostname r1\nend\n";
        assert_eq!(
            normalize(Vendor::CiscoIos, a),
            normalize(Vendor::CiscoIos, b)
        );
    }

    #[test]
    fn keeps_real_changes() {
        let a = "hostname r1\nend\n";
        let b = "hostname r2\nend\n";
        assert_ne!(
            normalize(Vendor::CiscoIos, a),
            normalize(Vendor::CiscoIos, b)
        );
    }

    #[test]
    fn strips_nxos_noise() {
        let raw = "!Command: show running-config\n!Running configuration last done at: Sun Nov  8 10:00:00 2026\n!Time: Sun Nov  8 10:05:00 2026\n\nversion 9.3(8)\nhostname n9k\n";
        assert_eq!(
            normalize(Vendor::CiscoNxos, raw),
            "version 9.3(8)\nhostname n9k\n"
        );
    }
}
