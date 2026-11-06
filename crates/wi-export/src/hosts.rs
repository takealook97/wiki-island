//! Installed host discovery. Detection never creates application directories.
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostId {
    Claude,
    Codex,
    Agy,
    Cursor,
    Windsurf,
    ClaudeDesktop,
    Vscode,
    Opencode,
    Zed,
    Gemini,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json(&'static str),
    Toml,
}
#[derive(Debug)]
pub struct HostSpec {
    pub id: HostId,
    pub name: &'static str,
    pub detect_path: &'static str,
    pub config_path: &'static str,
    pub format: Format,
}
pub const HOSTS: &[HostSpec] = &[
    HostSpec {
        id: HostId::Claude,
        name: "Claude Code",
        detect_path: ".claude",
        config_path: ".claude.json",
        format: Format::Json("mcpServers"),
    },
    HostSpec {
        id: HostId::Codex,
        name: "Codex",
        detect_path: ".codex",
        config_path: ".codex/config.toml",
        format: Format::Toml,
    },
    HostSpec {
        id: HostId::Agy,
        name: "Antigravity",
        detect_path: ".gemini",
        config_path: ".gemini/config/mcp_config.json",
        format: Format::Json("mcpServers"),
    },
    HostSpec {
        id: HostId::Cursor,
        name: "Cursor",
        detect_path: ".cursor",
        config_path: ".cursor/mcp.json",
        format: Format::Json("mcpServers"),
    },
    HostSpec {
        id: HostId::Windsurf,
        name: "Windsurf",
        detect_path: ".codeium/windsurf",
        config_path: ".codeium/windsurf/mcp_config.json",
        format: Format::Json("mcpServers"),
    },
    HostSpec {
        id: HostId::ClaudeDesktop,
        name: "Claude Desktop",
        detect_path: "Library/Application Support/Claude",
        config_path: "Library/Application Support/Claude/claude_desktop_config.json",
        format: Format::Json("mcpServers"),
    },
    HostSpec {
        id: HostId::Vscode,
        name: "VS Code",
        detect_path: "Library/Application Support/Code/User",
        config_path: "Library/Application Support/Code/User/mcp.json",
        format: Format::Json("servers"),
    },
    HostSpec {
        id: HostId::Opencode,
        name: "OpenCode",
        detect_path: ".config/opencode",
        config_path: ".config/opencode/opencode.json",
        format: Format::Json("mcp"),
    },
    HostSpec {
        id: HostId::Zed,
        name: "Zed",
        detect_path: ".config/zed",
        config_path: ".config/zed/settings.json",
        format: Format::Json("context_servers"),
    },
    HostSpec {
        id: HostId::Gemini,
        name: "Gemini CLI",
        detect_path: ".gemini",
        config_path: ".gemini/settings.json",
        format: Format::Json("mcpServers"),
    },
];
impl HostId {
    pub fn spec(self) -> &'static HostSpec {
        // The exhaustive match keeps new variants from silently lacking a specification.
        &HOSTS[match self {
            Self::Claude => 0,
            Self::Codex => 1,
            Self::Agy => 2,
            Self::Cursor => 3,
            Self::Windsurf => 4,
            Self::ClaudeDesktop => 5,
            Self::Vscode => 6,
            Self::Opencode => 7,
            Self::Zed => 8,
            Self::Gemini => 9,
        }]
    }
    pub fn config_path(self, home: &Path, codex_home: Option<&Path>) -> PathBuf {
        if self == Self::Codex
            && let Some(root) = codex_home
        {
            return root.join("config.toml");
        }
        let path = home.join(self.spec().config_path);
        if self == Self::Opencode && !path.exists() && path.with_extension("jsonc").is_file() {
            return path.with_extension("jsonc");
        }
        path
    }
}
impl From<crate::Host> for HostId {
    fn from(host: crate::Host) -> Self {
        match host {
            crate::Host::Claude => Self::Claude,
            crate::Host::Codex => Self::Codex,
            crate::Host::Gemini => Self::Gemini,
        }
    }
}
pub fn detect(home: &Path) -> Vec<HostId> {
    HOSTS
        .iter()
        .filter(|host| {
            home.join(host.detect_path).is_dir() || home.join(host.config_path).is_file()
        })
        .map(|host| host.id)
        .collect()
}
