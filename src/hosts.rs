//! The Windows `hosts` file, edited only within a block Wisp owns.
//!
//! `.test` domains have to resolve to this computer (`127.0.0.1`, or the
//! loopback address chosen in Settings), and there is no wildcard in a hosts
//! file, so every project domain is written explicitly. Writing it needs
//! administrator rights, which is the one thing Wisp elevates for.

use std::fmt::Write as _;
use std::net::Ipv4Addr;
use std::path::PathBuf;

pub const START: &str = "# wisp:start";
pub const END: &str = "# wisp:end";

/// `%SystemRoot%\System32\drivers\etc\hosts`.
pub fn hosts_path() -> PathBuf {
    crate::platform::system32()
        .join("drivers")
        .join("etc")
        .join("hosts")
}

/// Read the current hosts file, treating a missing file as empty.
pub fn read() -> std::io::Result<String> {
    match std::fs::read_to_string(hosts_path()) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error),
    }
}

/// Rewrite `existing` with Wisp's block replaced by an entry per domain,
/// each pointing at `address`.
///
/// Everything outside the markers is preserved verbatim; the block is removed
/// entirely when there are no domains. Pure so it can be tested without touching
/// the real file.
pub fn apply(existing: &str, address: Ipv4Addr, domains: &[String]) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut in_block = false;
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed == START {
            in_block = true;
            continue;
        }
        if trimmed == END {
            in_block = false;
            continue;
        }
        if !in_block {
            kept.push(line);
        }
    }
    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }

    let mut out = kept.join("\r\n");
    if domains.is_empty() {
        if !out.is_empty() {
            out.push_str("\r\n");
        }
        return out;
    }

    if !out.is_empty() {
        out.push_str("\r\n\r\n");
    }
    out.push_str(START);
    out.push_str("\r\n");
    for domain in domains {
        // `write!` into the buffer, not a throwaway `format!` per domain.
        let _ = write!(out, "{address}\t{domain}\r\n");
    }
    out.push_str(END);
    out.push_str("\r\n");
    out
}

/// Whether the hosts file already matches the desired domains.
pub fn needs_update(address: Ipv4Addr, domains: &[String]) -> bool {
    let Ok(current) = read() else {
        return true;
    };
    apply(&current, address, domains) != current
}

/// The `domains` that a line outside Wisp's block sends somewhere other than
/// `address`.
///
/// Windows hands a browser every address a name has in the file, so an entry
/// left by another tool can still take the request to the wrong place. Wisp
/// leaves those lines alone and reports them instead.
pub fn conflicts(existing: &str, address: Ipv4Addr, domains: &[String]) -> Vec<String> {
    let wanted = address.to_string();
    let mut found: Vec<String> = Vec::new();
    let mut in_block = false;
    for line in existing.lines() {
        let trimmed = line.trim();
        if trimmed == START {
            in_block = true;
            continue;
        }
        if trimmed == END {
            in_block = false;
            continue;
        }
        if in_block {
            continue;
        }
        let entry = trimmed.split('#').next().unwrap_or_default();
        let mut fields = entry.split_whitespace();
        let Some(target) = fields.next() else {
            continue;
        };
        if target == wanted {
            continue;
        }
        for name in fields {
            let hit = domains
                .iter()
                .find(|domain| domain.eq_ignore_ascii_case(name));
            if let Some(domain) = hit {
                if !found.contains(domain) {
                    found.push(domain.clone());
                }
            }
        }
    }
    found
}

/// Write the managed block. Needs administrator rights.
pub fn write(address: Ipv4Addr, domains: &[String]) -> std::io::Result<()> {
    let current = read()?;
    let updated = apply(&current, address, domains);
    std::fs::write(hosts_path(), updated)
}

#[cfg(test)]
mod tests {
    use super::{apply, conflicts, END, START};
    use std::net::Ipv4Addr;

    const LOCAL: Ipv4Addr = Ipv4Addr::LOCALHOST;

    fn domains(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn adds_a_block_to_an_existing_file() {
        let existing = "# Copyright\r\n127.0.0.1 localhost\r\n";
        let updated = apply(existing, LOCAL, &domains(&["a.test", "b.test"]));
        assert!(updated.contains("# Copyright"));
        assert!(updated.contains("127.0.0.1 localhost"));
        assert!(updated.contains(START));
        assert!(updated.contains("127.0.0.1\ta.test"));
        assert!(updated.contains("127.0.0.1\tb.test"));
        assert!(updated.contains(END));
    }

    #[test]
    fn replaces_an_existing_block_without_duplicating() {
        let existing =
            "127.0.0.1 localhost\r\n\r\n# wisp:start\r\n127.0.0.1\told.test\r\n# wisp:end\r\n";
        let updated = apply(existing, LOCAL, &domains(&["new.test"]));
        assert!(!updated.contains("old.test"));
        assert!(updated.contains("127.0.0.1\tnew.test"));
        assert_eq!(updated.matches(START).count(), 1);
        assert_eq!(updated.matches(END).count(), 1);
        assert!(updated.contains("127.0.0.1 localhost"));
    }

    #[test]
    fn removes_the_block_when_nothing_is_running() {
        let existing = "127.0.0.1 localhost\r\n# wisp:start\r\n127.0.0.1\ta.test\r\n# wisp:end\r\n";
        let updated = apply(existing, LOCAL, &[]);
        assert!(!updated.contains(START));
        assert!(!updated.contains("a.test"));
        assert!(updated.contains("127.0.0.1 localhost"));
    }

    #[test]
    fn the_block_points_at_the_chosen_address() {
        let existing = "# wisp:start\r\n127.0.0.1\ta.test\r\n# wisp:end\r\n";
        let updated = apply(existing, Ipv4Addr::new(127, 0, 0, 2), &domains(&["a.test"]));
        assert!(updated.contains("127.0.0.2\ta.test"));
        assert!(!updated.contains("127.0.0.1\ta.test"));
    }

    #[test]
    fn entries_from_other_tools_that_point_elsewhere_are_reported() {
        let existing = concat!(
            "127.0.0.1 a.test #laragon magic!\r\n",
            "::1\tA.TEST\r\n",
            "127.0.0.2 b.test\r\n",
            "# 127.0.0.1 c.test\r\n",
            "# wisp:start\r\n127.0.0.1\tc.test\r\n# wisp:end\r\n",
        );
        let wanted = domains(&["a.test", "b.test", "c.test"]);
        assert_eq!(
            conflicts(existing, Ipv4Addr::new(127, 0, 0, 2), &wanted),
            domains(&["a.test"]),
            "b.test already points there; c.test is commented out or Wisp's own"
        );
        assert!(conflicts(existing, LOCAL, &domains(&["c.test"])).is_empty());
    }

    #[test]
    fn an_empty_file_with_no_domains_stays_empty() {
        assert_eq!(apply("", LOCAL, &[]), "");
    }
}
