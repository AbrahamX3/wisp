//! Copy-ready client configuration for the MCP server.
//!
//! Every client launches the same thing — `wisp.exe --mcp` — but each has its
//! own config file and key names. Generating them here keeps the Settings page
//! and the documentation from drifting apart, and makes the escaping testable.

use std::path::Path;

/// One client's configuration block, ready to paste.
pub struct ClientSnippet {
    /// Stable key, also used for element ids.
    pub id: &'static str,
    pub name: &'static str,
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
            location: "Run once in a terminal. Add `-s project` to keep it in this repository.",
            snippet: format!(
                "claude mcp add --transport stdio wisp -- {quoted} --mcp"
            ),
        },
        ClientSnippet {
            id: "opencode",
            name: "OpenCode",
            location: "Merge into `opencode.json`, or `~/.config/opencode/opencode.json`.",
            snippet: format!(
                "{{\n  \"$schema\": \"https://opencode.ai/config.json\",\n  \"mcp\": {{\n    \"wisp\": {{\n      \"type\": \"local\",\n      \"command\": [{quoted}, \"--mcp\"],\n      \"enabled\": true\n    }}\n  }}\n}}"
            ),
        },
        ClientSnippet {
            id: "codex",
            name: "Codex CLI",
            location: "Merge into `~/.codex/config.toml`.",
            snippet: format!(
                "[mcp_servers.wisp]\ncommand = {quoted}\nargs = [\"--mcp\"]"
            ),
        },
        ClientSnippet {
            id: "cursor",
            name: "Cursor",
            location: "Merge into `.cursor/mcp.json`, or `~/.cursor/mcp.json`.",
            snippet: format!(
                "{{\n  \"mcpServers\": {{\n    \"wisp\": {{\n      \"command\": {quoted},\n      \"args\": [\"--mcp\"]\n    }}\n  }}\n}}"
            ),
        },
        ClientSnippet {
            id: "vscode",
            name: "VS Code",
            location: "Merge into `.vscode/mcp.json` in the workspace.",
            snippet: format!(
                "{{\n  \"servers\": {{\n    \"wisp\": {{\n      \"type\": \"stdio\",\n      \"command\": {quoted},\n      \"args\": [\"--mcp\"]\n    }}\n  }}\n}}"
            ),
        },
        ClientSnippet {
            id: "generic",
            name: "Other clients",
            location: "The common `mcpServers` shape, used by Claude Desktop, Windsurf and others.",
            snippet: format!(
                "{{\n  \"mcpServers\": {{\n    \"wisp\": {{\n      \"command\": {quoted},\n      \"args\": [\"--mcp\"]\n    }}\n  }}\n}}"
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
    fn the_json_snippets_are_valid_json() {
        for client in snippets()
            .into_iter()
            .filter(|client| client.id != "claude-code" && client.id != "codex")
        {
            let parsed: serde_json::Value = serde_json::from_str(&client.snippet)
                .unwrap_or_else(|error| panic!("{} is not JSON: {error}", client.name));
            assert!(parsed.is_object());
        }
    }

    #[test]
    fn the_client_shapes_use_the_keys_those_clients_expect() {
        let find = |id: &str| {
            snippets()
                .into_iter()
                .find(|client| client.id == id)
                .unwrap()
                .snippet
        };

        assert!(find("opencode").contains("\"mcp\""));
        assert!(find("cursor").contains("\"mcpServers\""));
        assert!(find("vscode").contains("\"servers\""));
        assert!(find("vscode").contains("\"type\": \"stdio\""));
        assert!(find("generic").contains("\"mcpServers\""));
        assert!(find("codex").contains("[mcp_servers.wisp]"));
        assert!(find("claude-code").starts_with("claude mcp add"));
    }
}
