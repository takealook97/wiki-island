use crate::{Error, Result};

/// Implementations must never include credential bytes in an error or log.
pub trait TokenSource {
    fn token(&self) -> Result<String>;
}

/// Reads an existing generic password. Provisioning is the caller's responsibility.
/// The account is typically the vault id; only its token goes to the HTTP client.
#[derive(Debug, Clone)]
pub struct KeychainTokenSource {
    pub account: String,
}

impl TokenSource for KeychainTokenSource {
    fn token(&self) -> Result<String> {
        #[cfg(target_os = "macos")]
        {
            let output = std::process::Command::new("/usr/bin/security")
                .args([
                    "find-generic-password",
                    "-s",
                    "wiki-island.notion",
                    "-a",
                    &self.account,
                    "-w",
                ])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .map_err(|_| Error::Token)?;
            if !output.status.success() {
                return Err(Error::Token);
            }
            let token = String::from_utf8(output.stdout).map_err(|_| Error::Token)?;
            let token = token.trim_end_matches(['\r', '\n']).to_owned();
            if token.is_empty() {
                return Err(Error::Token);
            }
            Ok(token)
        }
        #[cfg(not(target_os = "macos"))]
        Err(Error::Token)
    }
}
