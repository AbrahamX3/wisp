//! Copy-ready client configuration for the MCP server.
//!
//! Every client launches the same thing — `wisp.exe --mcp` — but each has its
//! own config file and key names. The snippets are the **entry to add**, not a
//! whole config file: users already have their own `opencode.json`, Cursor
//! `mcp.json`, and so on, and only need the `wisp` block inside it.
//!
//! Generating them here keeps the Settings page and the documentation from
//! drifting apart, and makes the escaping testable.

use std::path::Path;

/// One client's configuration block, ready to paste.
#[derive(Clone)]
pub struct ClientSnippet {
    /// Stable key, also used for element ids.
    pub id: &'static str,
    pub name: &'static str,
    /// Compact label for the tab bar, where the full name may not fit.
    pub short_name: &'static str,
    /// Where the snippet belongs.
    pub location: &'static str,
    pub snippet: String,
}

/// Every supported client, in the order the Settings page shows them.
pub fn all(exe: &Path) -> Vec<ClientSnippet> {
    let command = exe.to_string_lossy().to_string();
    let quoted = json_string(&command);

    vec![
        ClientSnippet {
            id: "claude-code",
            name: "Claude Code",
            short_name: "Claude",
            location: "Run once in a terminal. Add `-s project` to keep it in this repository.",
            snippet: format!("claude mcp add --transport stdio wisp -- {quoted} --mcp"),
        },
        ClientSnippet {
            id: "opencode",
            name: "OpenCode",
            short_name: "OpenCode",
            location: "Add inside `mcp` in `opencode.json` (or `~/.config/opencode/opencode.json`).",
            snippet: format!(
                "\"wisp\": {{\n  \"type\": \"local\",\n  \"command\": [{quoted}, \"--mcp\"],\n  \"enabled\": true\n}}"
            ),
        },
        ClientSnippet {
            id: "codex",
            name: "Codex CLI",
            short_name: "Codex",
            location: "Merge into `~/.codex/config.toml`.",
            snippet: format!("[mcp_servers.wisp]\ncommand = {quoted}\nargs = [\"--mcp\"]"),
        },
        ClientSnippet {
            id: "cursor",
            name: "Cursor",
            short_name: "Cursor",
            location: "Add inside `mcpServers` in `.cursor/mcp.json` (or `~/.cursor/mcp.json`).",
            snippet: format!(
                "\"wisp\": {{\n  \"command\": {quoted},\n  \"args\": [\"--mcp\"]\n}}"
            ),
        },
        ClientSnippet {
            id: "vscode",
            name: "VS Code",
            short_name: "VS Code",
            location: "Add inside `servers` in `.vscode/mcp.json`.",
            snippet: format!(
                "\"wisp\": {{\n  \"type\": \"stdio\",\n  \"command\": {quoted},\n  \"args\": [\"--mcp\"]\n}}"
            ),
        },
        ClientSnippet {
            id: "generic",
            name: "Other clients",
            short_name: "Other",
            location: "Add inside `mcpServers` (Claude Desktop, Windsurf, ...).",
            snippet: format!(
                "\"wisp\": {{\n  \"command\": {quoted},\n  \"args\": [\"--mcp\"]\n}}"
            ),
        },
    ]
}

/// A JSON/TOML basic string literal for `value`.
///
/// Windows paths are full of backslashes, which have to double in both formats.
fn json_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string())
}

#[cfg(test)]
mod tests {
    use super::all;
    use std::path::Path;

    fn snippets() -> Vec<super::ClientSnippet> {
        all(Path::new(r"C:\Users\me\.wisp\wisp.exe"))
    }

    fn find(id: &str) -> super::ClientSnippet {
        snippets()
            .into_iter()
            .find(|client| client.id == id)
            .unwrap_or_else(|| panic!("no snippet for {id}"))
    }

    #[test]
    fn every_client_launches_the_bridge() {
        for client in snippets() {
            assert!(
                client
                    .snippet
                    .contains(r#""C:\\Users\\me\\.wisp\\wisp.exe""#),
                "{} does not contain the quoted exe path:\n{}",
                client.name,
                client.snippet
            );
            assert!(
                client.snippet.contains("--mcp"),
                "{} does not pass --mcp",
                client.name
            );
        }
    }

    #[test]
    fn the_json_entries_are_valid_inside_their_parent_object() {
        for client in snippets()
            .into_iter()
            .filter(|client| client.id != "claude-code" && client.id != "codex")
        {
            // The snippet is one `"wisp": { ... }` entry, so wrap it in the
            // parent object before parsing.
            let wrapped = format!("{{{}}}", client.snippet);
            let parsed: serde_json::Value = serde_json::from_str(&wrapped)
                .unwrap_or_else(|error| panic!("{} is not JSON: {error}", client.name));
            assert!(
                parsed.get("wisp").is_some_and(serde_json::Value::is_object),
                "{} does not define a wisp object",
                client.name
            );
        }
    }

    #[test]
    fn the_client_shapes_use_the_keys_those_clients_expect() {
        let opencode = find("opencode");
        assert!(opencode.snippet.contains("\"type\": \"local\""));
        assert!(opencode.location.contains("`mcp`"));

        let cursor = find("cursor");
        assert!(cursor.snippet.contains("\"command\""));
        assert!(cursor.location.contains("`mcpServers`"));

        let vscode = find("vscode");
        assert!(vscode.snippet.contains("\"type\": \"stdio\""));
        assert!(vscode.location.contains("`servers`"));

        assert!(find("generic").location.contains("`mcpServers`"));
        assert!(find("codex").snippet.contains("[mcp_servers.wisp]"));
        assert!(find("claude-code").snippet.starts_with("claude mcp add"));
    }

    #[test]
    fn tab_labels_are_short_and_unique() {
        let mut seen = std::collections::HashSet::new();
        for client in snippets() {
            assert!(!client.short_name.is_empty());
            assert!(
                client.short_name.len() <= 10,
                "{} is too long for a tab",
                client.short_name
            );
            assert!(
                seen.insert(client.short_name),
                "duplicate tab label {}",
                client.short_name
            );
        }
    }
}
