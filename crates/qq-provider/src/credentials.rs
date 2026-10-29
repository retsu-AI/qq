use std::{borrow::Cow, fmt};

use reqwest::header::HeaderValue;
use serde::{Deserialize, Serialize};

use crate::ProviderError;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecretLiteral(String);

impl SecretLiteral {
    #[must_use]
    pub fn new(secret: impl Into<String>) -> Self {
        Self(secret.into())
    }

    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl From<String> for SecretLiteral {
    fn from(secret: String) -> Self {
        Self(secret)
    }
}

impl From<&str> for SecretLiteral {
    fn from(secret: &str) -> Self {
        Self(secret.to_owned())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum StaticAuth {
    None,
    Bearer(SecretLiteral),
    Header(Cow<'static, str>, SecretLiteral),
}

impl StaticAuth {
    pub(crate) fn header(name: impl Into<Cow<'static, str>>, secret: SecretLiteral) -> Self {
        Self::Header(name.into(), secret)
    }

    pub(crate) fn kind(&self) -> &str {
        match self {
            Self::None => "none",
            Self::Bearer(_) => "bearer",
            Self::Header(name, _) => name,
        }
    }
}

pub(crate) fn sensitive_header_value(
    secret: &SecretLiteral,
    label: &str,
) -> Result<HeaderValue, ProviderError> {
    sensitive_value(non_empty_secret(secret, label)?, label)
}

pub(crate) fn sensitive_bearer_value(
    secret: &SecretLiteral,
    label: &str,
) -> Result<HeaderValue, ProviderError> {
    let token = non_empty_secret(secret, label)?;
    sensitive_value(&format!("Bearer {token}"), label)
}

fn non_empty_secret<'a>(secret: &'a SecretLiteral, label: &str) -> Result<&'a str, ProviderError> {
    let secret = secret.expose_secret();
    if secret.trim().is_empty() {
        return Err(ProviderError::Configuration(format!(
            "{label} must not be empty"
        )));
    }
    Ok(secret)
}

fn sensitive_value(raw: &str, label: &str) -> Result<HeaderValue, ProviderError> {
    let mut value = HeaderValue::from_str(raw).map_err(|_| {
        ProviderError::Configuration(format!("{label} is not a valid HTTP header value"))
    })?;
    value.set_sensitive(true);
    Ok(value)
}

impl fmt::Debug for SecretLiteral {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SecretRef {
    Env(String),
    Stored(String),
    Value(SecretLiteral),
}

impl SecretRef {
    #[must_use]
    pub const fn is_literal(&self) -> bool {
        matches!(self, Self::Value(_))
    }
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Env(name) => formatter.debug_tuple("Env").field(name).finish(),
            Self::Stored(name) => formatter.debug_tuple("Stored").field(name).finish(),
            Self::Value(_) => formatter.write_str("Value(<redacted>)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_literals_construct_from_strings_and_redact_debug() {
        let from_str: SecretLiteral = "literal-test-secret".into();
        let from_string: SecretLiteral = String::from("literal-test-secret").into();
        let constructed = SecretLiteral::new("literal-test-secret");

        assert_eq!(from_str, from_string);
        assert_eq!(from_str, constructed);
        assert_eq!(constructed.expose_secret(), "literal-test-secret");
        assert_eq!(format!("{constructed:?}"), "<redacted>");
    }

    #[test]
    fn sensitive_header_values_are_marked_sensitive() {
        let value = sensitive_header_value(
            &SecretLiteral::new("header-test-secret"),
            "x-api-key secret",
        )
        .unwrap();

        assert_eq!(value.to_str().unwrap(), "header-test-secret");
        assert!(value.is_sensitive());
    }

    #[test]
    fn sensitive_bearer_values_prefix_the_scheme_and_stay_sensitive() {
        let value =
            sensitive_bearer_value(&SecretLiteral::new("bearer-test-secret"), "Bearer secret")
                .unwrap();

        assert_eq!(value.to_str().unwrap(), "Bearer bearer-test-secret");
        assert!(value.is_sensitive());
    }

    #[test]
    fn blank_and_invalid_secrets_are_rejected_with_the_labelled_reason() {
        for (secret, label, expected) in [
            ("", "Bearer secret", "Bearer secret must not be empty"),
            (
                "   ",
                "x-api-key secret",
                "x-api-key secret must not be empty",
            ),
            (
                "bad\nvalue",
                "Codex access token",
                "Codex access token is not a valid HTTP header value",
            ),
        ] {
            for build in [sensitive_header_value, sensitive_bearer_value] {
                let error = build(&SecretLiteral::new(secret), label).unwrap_err();
                assert!(
                    matches!(&error, crate::ProviderError::Configuration(message) if message == expected),
                    "unexpected error for {secret:?}: {error:?}"
                );
            }
        }
    }
}
