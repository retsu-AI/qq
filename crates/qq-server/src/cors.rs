use axum::{
    body::Body,
    extract::Request,
    http::{
        HeaderValue, Method, StatusCode,
        header::{
            ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS,
            ACCESS_CONTROL_ALLOW_ORIGIN, ACCESS_CONTROL_MAX_AGE, ACCESS_CONTROL_REQUEST_METHOD,
            HeaderName, ORIGIN, VARY,
        },
    },
    middleware::Next,
    response::{IntoResponse, Response},
};
use thiserror::Error;

pub const MAX_ORIGIN_BYTES: usize = 256;
pub const MAX_ALLOWED_ORIGINS: usize = 32;

const ALLOWED_HEADERS: &str = "authorization, content-type, last-event-id";
const ALLOWED_METHODS: &str = "GET, POST";
const PREFLIGHT_MAX_AGE_SECONDS: &str = "600";
const PRIVATE_NETWORK_REQUEST: HeaderName =
    HeaderName::from_static("access-control-request-private-network");
const PRIVATE_NETWORK_ALLOW: HeaderName =
    HeaderName::from_static("access-control-allow-private-network");

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AllowedOrigins {
    origins: Vec<HeaderValue>,
}

impl AllowedOrigins {
    pub fn new<I, S>(origins: I) -> Result<Self, AllowedOriginError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut normalized = Vec::new();
        for origin in origins {
            let origin = origin.as_ref().trim();
            if normalized.len() >= MAX_ALLOWED_ORIGINS {
                return Err(AllowedOriginError::TooMany);
            }
            let value = normalize_origin(origin)?;
            if !normalized.contains(&value) {
                normalized.push(value);
            }
        }
        Ok(Self {
            origins: normalized,
        })
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.origins.is_empty()
    }

    fn matches(&self, origin: &HeaderValue) -> bool {
        self.origins.iter().any(|allowed| allowed == origin)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AllowedOriginError {
    #[error("origin must be https://host[:port], or http://<loopback>[:port]: {0:?}")]
    Invalid(String),
    #[error("at most {MAX_ALLOWED_ORIGINS} allowed origins are supported")]
    TooMany,
}

fn normalize_origin(candidate: &str) -> Result<HeaderValue, AllowedOriginError> {
    let invalid = || AllowedOriginError::Invalid(candidate.chars().take(64).collect());
    if candidate.is_empty() || candidate.len() > MAX_ORIGIN_BYTES {
        return Err(invalid());
    }
    let (scheme, authority) = if let Some(rest) = candidate.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = candidate.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err(invalid());
    };
    if authority.is_empty()
        || authority.contains(['/', '?', '#', '@', ' ', '\\'])
        || !authority.is_ascii()
        || authority.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    let host = if let Some(rest) = authority.strip_prefix('[') {
        let Some((host, after)) = rest.split_once(']') else {
            return Err(invalid());
        };
        if host.parse::<std::net::Ipv6Addr>().is_err()
            || !(after.is_empty() || after.strip_prefix(':').is_some_and(valid_port))
        {
            return Err(invalid());
        }
        host.to_owned()
    } else {
        match authority.rsplit_once(':') {
            Some((host, port)) if !host.contains(':') => {
                if !valid_port(port) {
                    return Err(invalid());
                }
                host.to_owned()
            }
            Some(_) => return Err(invalid()),
            None => authority.to_owned(),
        }
    };
    if host.is_empty() {
        return Err(invalid());
    }
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if scheme == "http" && !loopback {
        return Err(invalid());
    }
    HeaderValue::from_str(&format!("{scheme}://{}", authority.to_ascii_lowercase()))
        .map_err(|_| invalid())
}

fn valid_port(port: &str) -> bool {
    !port.is_empty() && port != "0" && port.parse::<u16>().is_ok()
}

pub(crate) async fn apply(allowed: AllowedOrigins, request: Request<Body>, next: Next) -> Response {
    if allowed.is_empty() {
        return next.run(request).await;
    }
    let Some(origin) = request.headers().get(ORIGIN).cloned() else {
        return next.run(request).await;
    };
    let is_preflight = request.method() == Method::OPTIONS
        && request
            .headers()
            .contains_key(ACCESS_CONTROL_REQUEST_METHOD);
    if !allowed.matches(&origin) {
        if is_preflight {
            return StatusCode::FORBIDDEN.into_response();
        }
        return next.run(request).await;
    }
    let private_network = request.headers().contains_key(PRIVATE_NETWORK_REQUEST);
    let mut response = if is_preflight {
        let mut response = StatusCode::NO_CONTENT.into_response();
        let headers = response.headers_mut();
        headers.insert(
            ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static(ALLOWED_METHODS),
        );
        headers.insert(
            ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static(ALLOWED_HEADERS),
        );
        headers.insert(
            ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static(PREFLIGHT_MAX_AGE_SECONDS),
        );
        if private_network {
            headers.insert(PRIVATE_NETWORK_ALLOW, HeaderValue::from_static("true"));
        }
        response
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    headers.append(VARY, HeaderValue::from_static("origin"));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_are_validated_and_normalized() {
        let allowed = AllowedOrigins::new([
            " https://App.Example.com ",
            "https://app.example.com",
            "http://localhost:8080",
            "http://127.0.0.1:3000",
            "http://[::1]:3000",
            "https://100.64.0.7:8443",
        ])
        .unwrap();
        assert_eq!(allowed.origins.len(), 5, "duplicates collapse");
        assert!(allowed.matches(&HeaderValue::from_static("https://app.example.com")));
        assert!(!allowed.matches(&HeaderValue::from_static("https://app.example.com:443")));
        assert!(!allowed.matches(&HeaderValue::from_static("https://evil.example.com")));

        for bad in [
            "http://app.example.com",
            "http://100.64.0.7",
            "https://",
            "https://app.example.com/",
            "https://app.example.com/path",
            "https://user@app.example.com",
            "https://app.example.com:0",
            "https://app.example.com:70000",
            "ftp://x",
            "https://[::1",
            "https://ünïcode.example",
            "*",
        ] {
            assert!(
                matches!(
                    AllowedOrigins::new([bad]),
                    Err(AllowedOriginError::Invalid(_))
                ),
                "{bad} should be rejected"
            );
        }
        let many = (0..=MAX_ALLOWED_ORIGINS).map(|index| format!("https://h{index}.example"));
        assert_eq!(AllowedOrigins::new(many), Err(AllowedOriginError::TooMany));
    }
}
