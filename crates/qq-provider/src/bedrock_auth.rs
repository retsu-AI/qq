use crate::credentials::SecretLiteral;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BedrockAuth {
    DefaultChain,
    Profile(String),
    ApiKey(SecretLiteral),
}
