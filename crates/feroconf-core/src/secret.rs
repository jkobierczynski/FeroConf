//! Secret values (SNMP communities, SNMPv3 passphrases).
//!
//! In the inventory a secret can be written three ways:
//!
//! ```toml
//! write_community = "private"                       # inline (fine for a lab, avoid in production)
//! write_community = { env = "FEROCONF_RW_COMMUNITY" } # read from an environment variable
//! write_community = { file = "/etc/feroconf/rw" }      # read from a file (trailing newline trimmed)
//! ```

use std::fmt;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Secret {
    Inline(String),
    Env { env: String },
    File { file: PathBuf },
}

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("environment variable {0} is not set")]
    MissingEnv(String),
    #[error("cannot read secret file {path}: {source}")]
    File {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

impl Secret {
    /// Resolve the secret to its value.
    pub fn resolve(&self) -> Result<String, SecretError> {
        match self {
            Secret::Inline(v) => Ok(v.clone()),
            Secret::Env { env } => {
                std::env::var(env).map_err(|_| SecretError::MissingEnv(env.clone()))
            }
            Secret::File { file } => std::fs::read_to_string(file)
                .map(|s| s.trim_end_matches(['\r', '\n']).to_string())
                .map_err(|source| SecretError::File {
                    path: file.clone(),
                    source,
                }),
        }
    }

    /// True when the secret is stored in the inventory file itself.
    pub fn is_inline(&self) -> bool {
        matches!(self, Secret::Inline(_))
    }
}

/// Never print secret values, even in debug output.
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Secret::Inline(_) => f.write_str("Secret::Inline(<redacted>)"),
            Secret::Env { env } => write!(f, "Secret::Env({env})"),
            Secret::File { file } => write!(f, "Secret::File({})", file.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_redacts_inline() {
        let s = Secret::Inline("hunter2".into());
        assert!(!format!("{s:?}").contains("hunter2"));
    }

    #[test]
    fn file_secret_trims_newline() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("c");
        std::fs::write(&p, "s3cret\n").unwrap();
        assert_eq!(Secret::File { file: p }.resolve().unwrap(), "s3cret");
    }

    #[test]
    fn missing_env_is_an_error() {
        let s = Secret::Env {
            env: "FEROCONF_TEST_SURELY_UNSET_VAR".into(),
        };
        assert!(matches!(s.resolve(), Err(SecretError::MissingEnv(_))));
    }
}
