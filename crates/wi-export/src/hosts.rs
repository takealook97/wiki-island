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
    /// Resolve both discovery and configuration from the same platform roots.
    pub fn paths(self, home: &Path, _app_data: Option<&Path>) -> (PathBuf, PathBuf) {
        #[cfg(windows)]
        {
            let app_data = _app_data
                .map(Path::to_path_buf)
                .or_else(|| {
                    // An empty or relative APPDATA is unset, never a path
                    // under the working directory.
                    std::env::var_os("APPDATA")
                        .map(PathBuf::from)
                        .filter(|path| path.is_absolute())
                })
                .unwrap_or_else(|| home.join("AppData/Roaming"));
            let relative = match self {
                Self::ClaudeDesktop => Some(("Claude", "Claude/claude_desktop_config.json")),
                Self::Vscode => Some(("Code/User", "Code/User/mcp.json")),
                Self::Zed => Some(("Zed", "Zed/settings.json")),
                _ => None,
            };
            if let Some((detect, config)) = relative {
                return (app_data.join(detect), app_data.join(config));
            }
        }
        (
            home.join(self.spec().detect_path),
            home.join(self.spec().config_path),
        )
    }

    pub fn config_path(self, home: &Path, codex_home: Option<&Path>) -> PathBuf {
        if self == Self::Codex
            && let Some(root) = codex_home
        {
            return root.join("config.toml");
        }
        let path = self.paths(home, None).1;
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
            let (directory, config) = host.id.paths(home, None);
            directory.is_dir() || config.is_file()
        })
        .map(|host| host.id)
        .collect()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn app_paths_use_roaming_data_and_other_hosts_use_the_given_profile() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("profile");
        let roaming = temp.path().join("roaming");
        for (host, directory, config) in [
            (
                HostId::ClaudeDesktop,
                "Claude",
                "Claude/claude_desktop_config.json",
            ),
            (HostId::Vscode, "Code/User", "Code/User/mcp.json"),
            (HostId::Zed, "Zed", "Zed/settings.json"),
        ] {
            assert_eq!(
                host.paths(&home, Some(&roaming)),
                (roaming.join(directory), roaming.join(config))
            );
        }
        for host in HOSTS
            .iter()
            .filter(|h| !matches!(h.id, HostId::ClaudeDesktop | HostId::Vscode | HostId::Zed))
        {
            assert_eq!(
                host.id.paths(&home, Some(&roaming)),
                (home.join(host.detect_path), home.join(host.config_path))
            );
        }
        assert_eq!(
            HostId::Codex.config_path(&home, Some(&roaming)),
            roaming.join("config.toml")
        );
    }
}
