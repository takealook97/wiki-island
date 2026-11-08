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
        #[cfg(windows)]
        {
            wi_sys::credential::read(&format!("wiki-island.notion/{}", self.account))
                .map_err(|_| Error::Token)?
                .filter(|token| !token.is_empty())
                .ok_or(Error::Token)
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        Err(Error::Token)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn credential_source_reads_a_unique_vault_target_and_cleans_up() {
        let unique = tempfile::tempdir().unwrap();
        let account = format!(
            "test-{}-{}",
            std::process::id(),
            unique.path().file_name().unwrap().to_string_lossy()
        );
        let target = format!("wiki-island.notion/{account}");
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = wi_sys::credential::delete(&self.0);
            }
        }
        let _cleanup = Cleanup(target.clone());
        let source = KeychainTokenSource { account };
        assert!(source.token().is_err());
        wi_sys::credential::write(&target, "test_token_value_1234").unwrap();
        assert_eq!(source.token().unwrap(), "test_token_value_1234");
        assert!(wi_sys::credential::delete(&target).unwrap());
        assert!(source.token().is_err());
    }
}
