//! Reading the tail of a log file without loading the whole thing.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// The last `max_lines` lines of `path`, reading at most `max_bytes` from the
/// end so a large log is not read in full on every poll.
pub fn tail(path: &Path, max_bytes: u64, max_lines: usize) -> Vec<String> {
    let Ok(mut file) = std::fs::File::open(path) else {
        return Vec::new();
    };

    let length = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = length.saturating_sub(max_bytes);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return Vec::new();
    }

    let mut buffer = Vec::new();
    if file.read_to_end(&mut buffer).is_err() {
        return Vec::new();
    }
    let text = String::from_utf8_lossy(&buffer);

    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    // When we started mid-file the first line is probably partial.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    let drop = lines.len().saturating_sub(max_lines);
    lines.drain(..drop);
    lines
}

#[cfg(test)]
mod tests {
    use super::tail;

    #[test]
    fn returns_the_last_lines() {
        let path = std::env::temp_dir().join("wisp-tail-test.log");
        let body: String = (0..500).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&path, body).unwrap();

        let lines = tail(&path, 1024 * 1024, 10);
        assert_eq!(lines.len(), 10);
        assert_eq!(lines.last().unwrap(), "line 499");
        assert_eq!(lines.first().unwrap(), "line 490");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_missing_file_is_empty() {
        let path = std::env::temp_dir().join("wisp-tail-missing.log");
        let _ = std::fs::remove_file(&path);
        assert!(tail(&path, 1024, 10).is_empty());
    }

    #[test]
    fn a_partial_first_line_is_dropped() {
        let path = std::env::temp_dir().join("wisp-tail-partial.log");
        std::fs::write(&path, "aaaaaaaaaa\nbbbbbbbbbb\ncccccccccc\n").unwrap();

        // A 15-byte window starts mid-way through "bbbbbbbbbb".
        let lines = tail(&path, 15, 10);
        assert_eq!(lines, vec!["cccccccccc"]);

        let _ = std::fs::remove_file(&path);
    }
}
