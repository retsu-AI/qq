#![forbid(unsafe_code)]

use std::{pin::Pin, sync::Arc};

use futures_core::Stream;

#[cfg(feature = "provider-bedrock")]
mod aws;
mod bedrock_auth;
pub mod compiler;
mod construction;
mod credentials;
mod exchange;
mod http;
mod limits;
mod model;
mod providers;
mod request_auth;
mod sanitize;
mod sse;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_support;

pub use bedrock_auth::BedrockAuth;
pub use compiler::{
    CacheRouting, EndpointSpec, HttpAuth, HttpProtocol, HttpProviderRecipe, ProviderCompiler,
    ProviderRecipe, ResponsesBodyShape,
};
pub use credentials::{SecretLiteral, SecretRef};
pub use http::{AttemptPolicy, TurnRecoveryPolicy};
pub use model::{
    ContentBlock, IncompleteReason, Message, ModelRequest, ProviderError, ProviderErrorKind,
    ProviderEvent, ProviderUsage, Role, ToolChoice, ToolSpec,
};
pub use qq_reasoning::{ReasoningEffort, ReasoningKind};
pub use request_auth::{
    RequestCredential, RequestCredentialError, RequestCredentialFuture, RequestCredentialProvider,
    SharedRequestCredentialProvider,
};

pub const BUILD_IDENTITY: &str = if cfg!(feature = "provider-bedrock") {
    concat!("qq-provider/", env!("CARGO_PKG_VERSION"), "+bedrock")
} else {
    concat!("qq-provider/", env!("CARGO_PKG_VERSION"))
};

pub type ProviderStream =
    Pin<Box<dyn Stream<Item = Result<ProviderEvent, ProviderError>> + Send + 'static>>;

pub trait Provider: Send + Sync {
    fn stream(&self, request: ModelRequest) -> ProviderStream;
}

impl<T> Provider for Arc<T>
where
    T: Provider + ?Sized,
{
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        (**self).stream(request)
    }
}

impl<T> Provider for Box<T>
where
    T: Provider + ?Sized,
{
    fn stream(&self, request: ModelRequest) -> ProviderStream {
        (**self).stream(request)
    }
}
