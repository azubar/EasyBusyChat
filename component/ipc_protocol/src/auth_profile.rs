use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Bootstrap identity shared by the DLL and UI. Never contains the raw token.
/// An opaque token is part of the identity because Init has no stable user ID.
#[derive(Clone, Eq, PartialEq)]
pub struct AuthProfile(String);

impl AuthProfile {
    pub fn for_context(api: &str, company: &str, token: &str) -> Self {
        let mut digest = Sha256::new();
        digest.update(b"EasyBusyChat auth profile v1\0");
        // Length-prefix each field to prevent ambiguous concatenation.
        for field in [api, company, token] {
            digest.update((field.len() as u64).to_le_bytes());
            digest.update(field.as_bytes());
        }
        Self(format!("{:x}", digest.finalize()))
    }

    pub fn parse(key: &str) -> Result<Self, &'static str> {
        if key.len() != 64
            || !key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("Invalid authentication profile key");
        }
        Ok(Self(key.to_owned()))
    }

    pub fn key(&self) -> &str {
        &self.0
    }

    pub fn data_directory(&self, local_data: &Path) -> Result<PathBuf, &'static str> {
        if !local_data.is_absolute() {
            return Err("Local application data directory must be absolute");
        }
        Ok(local_data
            .join("com.easybusychat.ui")
            .join("auth-profiles-v1")
            .join(&self.0))
    }

    pub fn accepts(&self, api: &str, company: &str, token: &str) -> bool {
        *self == Self::for_context(api, company, token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeated_context_reuses_profile_but_each_auth_dimension_is_isolated() {
        let first = AuthProfile::for_context("https://api.example", "company", "synthetic-a");
        assert!(first.accepts("https://api.example", "company", "synthetic-a"));
        assert!(!first.accepts("https://api.example", "company", "synthetic-b"));
        assert!(!first.accepts("https://api.example", "other", "synthetic-a"));
        assert!(!first.accepts("https://other.example", "company", "synthetic-a"));
        assert!(AuthProfile::parse(first.key()).unwrap() == first);
    }

    #[test]
    fn profile_key_is_unambiguous_and_does_not_expose_credentials() {
        let first = AuthProfile::for_context("ab", "c", "synthetic-token");
        let second = AuthProfile::for_context("a", "bc", "synthetic-token");
        assert!(first != second);
        assert!(!first.key().contains("synthetic-token"));
    }

    #[test]
    fn rejects_missing_malformed_or_path_like_bootstrap_keys() {
        for invalid in [
            "",
            "../shared",
            "C:\\shared",
            &"a".repeat(63),
            &"a".repeat(65),
            &"G".repeat(64),
        ] {
            assert!(AuthProfile::parse(invalid).is_err());
        }
        let profile = AuthProfile::for_context("api", "company", "synthetic");
        assert!(profile.data_directory(Path::new("relative")).is_err());
        let root = std::env::temp_dir();
        let dir = profile.data_directory(&root).unwrap();
        assert!(dir.starts_with(root.join("com.easybusychat.ui/auth-profiles-v1")));
        assert_eq!(dir.file_name().unwrap(), profile.key());
    }
}
