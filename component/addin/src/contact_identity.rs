use serde::{Deserialize, Serialize};

const MAX_PHONE_LENGTH: usize = 30;
const MAX_USERNAME_LENGTH: usize = 150;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IdentityType {
    Phone,
    Username,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NormalizedIdentity {
    #[serde(rename = "type")]
    pub identity_type: IdentityType,
    pub value: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdentityError {
    pub code: &'static str,
    pub message: &'static str,
}

pub fn normalize_identity(input: &str) -> Result<NormalizedIdentity, IdentityError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(IdentityError {
            code: "invalid_identity",
            message: "identity must not be empty",
        });
    }

    if let Some(username) = trimmed.strip_prefix('@') {
        return normalize_username(username);
    }

    let compact = trimmed
        .chars()
        .filter(|character| !matches!(character, ' ' | '\t' | '\r' | '\n' | '(' | ')' | '-' | '.'))
        .collect::<String>();

    let is_phone = compact.strip_prefix('+').is_some_and(|digits| {
        !digits.is_empty() && digits.chars().all(|character| character.is_ascii_digit())
    }) || (!compact.is_empty()
        && compact.chars().all(|character| character.is_ascii_digit()));

    if is_phone {
        if compact.len() > MAX_PHONE_LENGTH {
            return Err(IdentityError {
                code: "invalid_phone",
                message: "normalized phone exceeds 30 characters",
            });
        }
        return Ok(NormalizedIdentity {
            identity_type: IdentityType::Phone,
            value: compact,
        });
    }

    normalize_username(trimmed)
}

fn normalize_username(username: &str) -> Result<NormalizedIdentity, IdentityError> {
    let username = username.trim();
    if username.is_empty()
        || username.len() > MAX_USERNAME_LENGTH
        || username
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(IdentityError {
            code: "invalid_username",
            message: "username must contain 1..150 non-whitespace characters",
        });
    }

    Ok(NormalizedIdentity {
        identity_type: IdentityType::Username,
        value: username.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_and_normalizes_phone() {
        let identity = normalize_identity(" +380 50 123-45-67 ").unwrap();
        assert_eq!(identity.identity_type, IdentityType::Phone);
        assert_eq!(identity.value, "+380501234567");
    }

    #[test]
    fn detects_local_phone_without_adding_country_code() {
        let identity = normalize_identity("0501234567").unwrap();
        assert_eq!(identity.identity_type, IdentityType::Phone);
        assert_eq!(identity.value, "0501234567");
    }

    #[test]
    fn removes_only_leading_username_marker() {
        let identity = normalize_identity(" @ivan_petrenko ").unwrap();
        assert_eq!(identity.identity_type, IdentityType::Username);
        assert_eq!(identity.value, "ivan_petrenko");
    }

    #[test]
    fn detects_username_without_marker() {
        let identity = normalize_identity("ivan_petrenko").unwrap();
        assert_eq!(identity.identity_type, IdentityType::Username);
        assert_eq!(identity.value, "ivan_petrenko");
    }

    #[test]
    fn rejects_empty_identity() {
        assert_eq!(
            normalize_identity("  ").unwrap_err().code,
            "invalid_identity"
        );
    }

    #[test]
    fn rejects_invalid_explicit_username() {
        assert_eq!(
            normalize_identity("@").unwrap_err().code,
            "invalid_username"
        );
        assert_eq!(
            normalize_identity("@ivan petrenko").unwrap_err().code,
            "invalid_username"
        );
    }
}
