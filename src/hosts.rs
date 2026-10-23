//! The Windows `hosts` file, edited only within a block Wisp owns.
//!
//! `.test` domains have to resolve to `127.0.0.1`, and there is no wildcard in
//! a hosts file, so every project domain is written explicitly. Writing it needs
//! administrator rights, which is the one thing Wisp elevates for.

use std::fmt::Write as _;
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

/// Rewrite `existing` with Wisp's block replaced by an entry per domain.
///
/// Everything outside the markers is preserved verbatim; the block is removed
/// entirely when there are no domains. Pure so it can be tested without touching
/// the real file.
pub fn apply(existing: &str, domains: &[String]) -> String {
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
        let _ = write!(out, "127.0.0.1\t{domain}\r\n");
    }
    out.push_str(END);
    out.push_str("\r\n");
    out
}

/// Whether the hosts file already matches the desired domains.
pub fn needs_update(domains: &[String]) -> bool {
    let Ok(current) = read() else {
        return true;
    };
    apply(&current, domains) != current
}

/// Write the managed block. Needs administrator rights.
pub fn write(domains: &[String]) -> std::io::Result<()> {
    let current = read()?;
    let updated = apply(&current, domains);
    std::fs::write(hosts_path(), updated)
}

#[cfg(test)]
mod tests {
    use super::{apply, END, START};

    fn domains(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_string()).collect()
    }

    #[test]
    fn adds_a_block_to_an_existing_file() {
        let existing = "# Copyright\r\n127.0.0.1 localhost\r\n";
        let updated = apply(existing, &domains(&["a.test", "b.test"]));
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
        let updated = apply(existing, &domains(&["new.test"]));
        assert!(!updated.contains("old.test"));
        assert!(updated.contains("127.0.0.1\tnew.test"));
        assert_eq!(updated.matches(START).count(), 1);
        assert_eq!(updated.matches(END).count(), 1);
        assert!(updated.contains("127.0.0.1 localhost"));
    }

    #[test]
    fn removes_the_block_when_nothing_is_running() {
        let existing = "127.0.0.1 localhost\r\n# wisp:start\r\n127.0.0.1\ta.test\r\n# wisp:end\r\n";
        let updated = apply(existing, &[]);
        assert!(!updated.contains(START));
        assert!(!updated.contains("a.test"));
        assert!(updated.contains("127.0.0.1 localhost"));
    }

    #[test]
    fn an_empty_file_with_no_domains_stays_empty() {
        assert_eq!(apply("", &[]), "");
    }
}
