use std::{
    io::Read,
    time::{Duration, Instant, SystemTime, SystemTimeError, UNIX_EPOCH},
};

use reqwest::blocking::{Client, Response};
use serde::de::DeserializeOwned;

use super::{AuthError, validate_credential_name};

#[derive(Clone, Copy, Debug)]
pub(crate) struct OAuthProvider {
    name: &'static str,
    lock_unavailable: fn() -> AuthError,
}

impl PartialEq for OAuthProvider {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for OAuthProvider {}

impl OAuthProvider {
    pub(crate) const OPENAI_CODEX: Self = Self {
        name: "openai-codex",
        lock_unavailable: || super::codex::CodexAuthError::RefreshLockUnavailable.into(),
    };
    pub(crate) const XAI: Self = Self {
        name: "xai",
        lock_unavailable: || super::xai::XaiAuthError::RefreshLockUnavailable.into(),
    };
    #[cfg(test)]
    pub(crate) const FIXTURE: Self = Self {
        name: "fixture-oauth",
        lock_unavailable: || AuthError::CredentialCoordinator { operation: "lock" },
    };

    #[cfg(not(test))]
    pub(crate) const ALL: &[Self] = &[Self::OPENAI_CODEX, Self::XAI];
    #[cfg(test)]
    pub(crate) const ALL: &[Self] = &[Self::OPENAI_CODEX, Self::XAI, Self::FIXTURE];

    pub(crate) const fn name(self) -> &'static str {
        self.name
    }

    pub(crate) fn index(self) -> usize {
        Self::ALL
            .iter()
            .position(|provider| *provider == self)
            .expect("every OAuth provider is a row of OAuthProvider::ALL")
    }

    pub(crate) fn for_credential(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|provider| {
            name.strip_prefix(provider.name)
                .is_some_and(|rest| rest.starts_with('/'))
        })
    }

    pub(crate) fn lock_unavailable(self) -> AuthError {
        (self.lock_unavailable)()
    }
}

pub(crate) fn lock_file_name(provider: &str) -> String {
    format!("{provider}.lock")
}

pub(crate) struct OAuthRequest<'a> {
    pub(crate) url: &'a str,
    pub(crate) body: OAuthBody,
    pub(crate) headers: &'a [(&'static str, &'static str)],
    pub(crate) timeout: Option<Duration>,
    pub(crate) max_body: usize,
}

pub(crate) enum OAuthBody {
    Form(Vec<(&'static str, String)>),
    Json(serde_json::Value),
}

impl OAuthBody {
    #[cfg(test)]
    pub(crate) fn field(&self, name: &str) -> Option<String> {
        match self {
            Self::Form(fields) => fields
                .iter()
                .find(|(field, _)| *field == name)
                .map(|(_, value)| value.clone()),
            Self::Json(value) => value.get(name)?.as_str().map(str::to_owned),
        }
    }
}

pub(crate) struct OAuthResponse {
    pub(crate) status: u16,
    pub(crate) body: Option<Vec<u8>>,
}

impl OAuthResponse {
    pub(crate) const fn is_success(&self) -> bool {
        self.status >= 200 && self.status < 300
    }

    pub(crate) fn json<T: DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_slice(self.body.as_deref()?).ok()
    }

    pub(crate) fn read(
        mut response: Response,
        limit: usize,
        timed_out: impl FnOnce() -> TransportError,
    ) -> Result<Self, TransportError> {
        let status = response.status().as_u16();
        let success = response.status().is_success();
        let mut bytes = Vec::new();
        let body = match (&mut response)
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
        {
            Ok(_) if bytes.len() <= limit => Some(bytes),
            Ok(_) => None,
            Err(error) if success && is_timeout(&error) => return Err(timed_out()),
            Err(_) => None,
        };
        Ok(Self { status, body })
    }
}

fn is_timeout(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::TimedOut
        || error
            .get_ref()
            .and_then(|inner| inner.downcast_ref::<reqwest::Error>())
            .is_some_and(reqwest::Error::is_timeout)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TransportError {
    Failed,
    DeadlineReached,
}

pub(crate) trait OAuthTransport: Send + Sync {
    fn post(&self, request: OAuthRequest<'_>) -> Result<OAuthResponse, TransportError>;
}

pub(crate) struct SystemTransport {
    client: Option<Client>,
    cap: Duration,
}

impl SystemTransport {
    pub(crate) const fn new() -> Self {
        Self {
            client: None,
            cap: REQUEST_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(crate) const fn with_client(client: Client) -> Self {
        Self {
            client: Some(client),
            cap: REQUEST_TIMEOUT,
        }
    }

    #[cfg(test)]
    pub(crate) const fn with_cap(mut self, cap: Duration) -> Self {
        self.cap = cap;
        self
    }
}

impl OAuthTransport for SystemTransport {
    fn post(&self, request: OAuthRequest<'_>) -> Result<OAuthResponse, TransportError> {
        let client = match &self.client {
            Some(client) => client.clone(),
            None => http_client().map_err(|_| TransportError::Failed)?,
        };
        let mut builder = client.post(request.url);
        for (name, value) in request.headers {
            builder = builder.header(*name, *value);
        }
        builder = match request.body {
            OAuthBody::Form(fields) => builder.form(&fields),
            OAuthBody::Json(value) => builder.json(&value),
        };
        builder = builder.timeout(
            request
                .timeout
                .map_or(self.cap, |timeout| timeout.min(self.cap)),
        );
        let started = Instant::now();
        let timed_out = || classify_timeout(request.timeout, self.cap, started.elapsed());
        let response = builder.send().map_err(|error| {
            if error.is_timeout() {
                timed_out()
            } else {
                TransportError::Failed
            }
        })?;
        OAuthResponse::read(response, request.max_body, timed_out)
    }
}

fn classify_timeout(
    deadline: Option<Duration>,
    cap: Duration,
    elapsed: Duration,
) -> TransportError {
    match deadline {
        Some(deadline) if deadline < cap && elapsed >= deadline => TransportError::DeadlineReached,
        _ => TransportError::Failed,
    }
}

pub(crate) enum DeviceStep<T> {
    Pending,
    SlowDown,
    Denied,
    Expired,
    Complete(T),
}

pub(crate) struct DevicePollErrors<E> {
    pub(crate) timed_out: fn() -> E,
    pub(crate) cancelled: fn() -> E,
    pub(crate) denied: fn() -> E,
    pub(crate) expired: fn() -> E,
}

pub(crate) struct DevicePollRuntime<N, C, S> {
    pub(crate) now: N,
    pub(crate) cancelled: C,
    pub(crate) sleep: S,
}

pub(crate) fn poll_device<T, E, N, C, S>(
    deadline: Instant,
    mut interval: Duration,
    max_interval: Duration,
    runtime: &mut DevicePollRuntime<N, C, S>,
    errors: &DevicePollErrors<E>,
    mut poll: impl FnMut(Duration) -> Result<DeviceStep<T>, E>,
) -> Result<T, E>
where
    N: FnMut() -> Instant,
    C: FnMut() -> bool,
    S: FnMut(Duration),
{
    loop {
        if (runtime.cancelled)() {
            return Err((errors.cancelled)());
        }
        let current = (runtime.now)();
        if current >= deadline {
            return Err((errors.timed_out)());
        }
        (runtime.sleep)(interval.min(deadline.saturating_duration_since(current)));
        if (runtime.cancelled)() {
            return Err((errors.cancelled)());
        }
        let current = (runtime.now)();
        if current >= deadline {
            return Err((errors.timed_out)());
        }
        let step = poll(deadline.saturating_duration_since(current))?;
        if (runtime.cancelled)() {
            return Err((errors.cancelled)());
        }
        if (runtime.now)() >= deadline {
            return Err((errors.timed_out)());
        }
        match step {
            DeviceStep::Pending => {}
            DeviceStep::SlowDown => {
                interval = interval
                    .saturating_add(Duration::from_secs(5))
                    .min(max_interval);
            }
            DeviceStep::Denied => return Err((errors.denied)()),
            DeviceStep::Expired => return Err((errors.expired)()),
            DeviceStep::Complete(value) => return Ok(value),
        }
    }
}

pub(super) fn http_client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .use_rustls_tls()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(30))
        .timeout(REQUEST_TIMEOUT)
        .user_agent(concat!("qq/", env!("CARGO_PKG_VERSION")))
        .build()
}

pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

pub(super) fn is_https_url(value: &str) -> bool {
    reqwest::Url::parse(value).is_ok_and(|url| url.scheme() == "https" && url.host().is_some())
}

pub(super) fn unix_time() -> Result<u64, SystemTimeError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
}

pub(super) fn profile_credential_name(
    provider: OAuthProvider,
    profile: &str,
) -> Result<String, AuthError> {
    let name = format!("{}/{profile}", provider.name());
    validate_credential_name(&name)?;
    Ok(name)
}

#[cfg(test)]
pub(crate) struct StalledBodyTransport(pub(crate) Duration);

#[cfg(test)]
impl OAuthTransport for StalledBodyTransport {
    fn post(&self, request: OAuthRequest<'_>) -> Result<OAuthResponse, TransportError> {
        tests::post_to_a_stalled_body(
            &tests::loopback_transport().with_cap(self.0),
            request.timeout,
        )
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::{cell::Cell, collections::VecDeque};

    use super::*;

    #[derive(Debug, PartialEq, Eq)]
    enum Ended {
        TimedOut,
        Cancelled,
        Denied,
        Expired,
    }

    const ERRORS: DevicePollErrors<Ended> = DevicePollErrors {
        timed_out: || Ended::TimedOut,
        cancelled: || Ended::Cancelled,
        denied: || Ended::Denied,
        expired: || Ended::Expired,
    };

    fn run(
        timeout: Duration,
        interval: Duration,
        steps: Vec<DeviceStep<&'static str>>,
        cancel_after_polls: Option<usize>,
    ) -> (Result<&'static str, Ended>, Vec<Duration>, Vec<Duration>) {
        let start = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let polls = Cell::new(0_usize);
        let mut sleeps = Vec::new();
        let mut remaining = Vec::new();
        let mut steps = VecDeque::from(steps);
        let mut runtime = DevicePollRuntime {
            now: || start + elapsed.get(),
            cancelled: || cancel_after_polls.is_some_and(|after| polls.get() >= after),
            sleep: |duration: Duration| {
                sleeps.push(duration);
                elapsed.set(elapsed.get() + duration);
            },
        };
        let result = poll_device(
            start + timeout,
            interval,
            Duration::from_secs(30),
            &mut runtime,
            &ERRORS,
            |left| {
                polls.set(polls.get() + 1);
                remaining.push(left);
                Ok(steps.pop_front().expect("the test supplies every poll"))
            },
        );
        (result, sleeps, remaining)
    }

    #[test]
    fn device_polling_waits_one_interval_per_pending_poll_then_completes() {
        let (result, sleeps, remaining) = run(
            Duration::from_secs(60),
            Duration::from_secs(5),
            vec![
                DeviceStep::Pending,
                DeviceStep::Pending,
                DeviceStep::Complete("token"),
            ],
            None,
        );
        assert_eq!(result, Ok("token"));
        assert_eq!(sleeps, [Duration::from_secs(5); 3]);
        assert_eq!(
            remaining,
            [55, 50, 45].map(Duration::from_secs),
            "each poll is bounded by the time left"
        );
    }

    #[test]
    fn device_polling_slows_down_by_five_seconds_up_to_the_cap() {
        let (result, sleeps, _) = run(
            Duration::from_secs(600),
            Duration::from_secs(20),
            vec![
                DeviceStep::SlowDown,
                DeviceStep::SlowDown,
                DeviceStep::SlowDown,
                DeviceStep::Complete("token"),
            ],
            None,
        );
        assert_eq!(result, Ok("token"));
        assert_eq!(sleeps, [20, 25, 30, 30].map(Duration::from_secs));
    }

    #[test]
    fn device_polling_reports_denial_and_expiry() {
        let (denied, _, _) = run(
            Duration::from_secs(60),
            Duration::from_secs(1),
            vec![DeviceStep::Pending, DeviceStep::Denied],
            None,
        );
        assert_eq!(denied, Err(Ended::Denied));
        let (expired, _, _) = run(
            Duration::from_secs(60),
            Duration::from_secs(1),
            vec![DeviceStep::Expired],
            None,
        );
        assert_eq!(expired, Err(Ended::Expired));
    }

    #[test]
    fn device_polling_stops_at_the_deadline_without_oversleeping() {
        let (result, sleeps, remaining) = run(
            Duration::from_secs(12),
            Duration::from_secs(5),
            vec![DeviceStep::Pending, DeviceStep::Pending],
            None,
        );
        assert_eq!(result, Err(Ended::TimedOut));
        assert_eq!(sleeps, [5, 5, 2].map(Duration::from_secs));
        assert_eq!(remaining.len(), 2);

        let (expired, sleeps, remaining) =
            run(Duration::ZERO, Duration::from_secs(5), Vec::new(), None);
        assert_eq!(expired, Err(Ended::TimedOut));
        assert!(sleeps.is_empty() && remaining.is_empty());
    }

    #[test]
    fn device_polling_stops_when_cancelled_without_polling_again() {
        let (before, sleeps, remaining) = run(
            Duration::from_secs(60),
            Duration::from_secs(5),
            Vec::new(),
            Some(0),
        );
        assert_eq!(before, Err(Ended::Cancelled));
        assert!(sleeps.is_empty() && remaining.is_empty());

        let (during, _, remaining) = run(
            Duration::from_secs(60),
            Duration::from_secs(5),
            vec![DeviceStep::Complete("token")],
            Some(1),
        );
        assert_eq!(during, Err(Ended::Cancelled));
        assert_eq!(remaining.len(), 1);
    }

    pub(crate) fn post_to_a_stalled_body(
        transport: &SystemTransport,
        timeout: Option<Duration>,
    ) -> Result<OAuthResponse, TransportError> {
        post_to_a_stalled_status(transport, 200, timeout)
    }

    pub(crate) fn post_to_a_stalled_status(
        transport: &SystemTransport,
        status: u16,
        timeout: Option<Duration>,
    ) -> Result<OAuthResponse, TransportError> {
        use std::{io::Write as _, net::TcpListener};

        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!("http://{}/token", listener.local_addr().unwrap());
        let (done, finished) = std::sync::mpsc::sync_channel::<()>(1);
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = std::io::Read::read(&mut stream, &mut request);
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 {status} Status\r\nContent-Type: application/json\r\nContent-Length: 64\r\n\r\n{{"
                    )
                    .as_bytes(),
                )
                .unwrap();
            stream.flush().unwrap();
            finished.recv().unwrap();
        });
        let result = transport.post(OAuthRequest {
            url: &url,
            body: OAuthBody::Form(vec![("grant_type", "device_code".to_owned())]),
            headers: &[],
            timeout,
            max_body: 1024 * 1024,
        });
        done.send(()).unwrap();
        server.join().unwrap();
        result
    }

    pub(crate) fn loopback_transport() -> SystemTransport {
        SystemTransport::with_client(
            Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
        )
    }

    #[test]
    fn a_body_that_stalls_past_the_request_deadline_reaches_it() {
        let result =
            post_to_a_stalled_body(&loopback_transport(), Some(Duration::from_millis(200)));
        assert_eq!(result.err(), Some(TransportError::DeadlineReached));
    }

    #[test]
    fn a_long_deadline_still_ends_at_the_transport_cap() {
        let transport = loopback_transport().with_cap(Duration::from_millis(200));
        for timeout in [Some(Duration::from_secs(2 * 60 * 60)), None] {
            let result = post_to_a_stalled_body(&transport, timeout);
            assert_eq!(result.err(), Some(TransportError::Failed), "{timeout:?}");
        }
    }

    #[test]
    fn only_a_timeout_the_deadline_caused_reaches_it() {
        let secs = Duration::from_secs;
        let cap = secs(60);
        assert_eq!(
            classify_timeout(Some(secs(45)), cap, secs(45)),
            TransportError::DeadlineReached
        );
        assert_eq!(
            classify_timeout(Some(secs(45)), cap, secs(30)),
            TransportError::Failed
        );
        assert_eq!(
            classify_timeout(Some(secs(1800)), cap, secs(60)),
            TransportError::Failed
        );
        assert_eq!(
            classify_timeout(None, cap, secs(60)),
            TransportError::Failed
        );
    }

    #[test]
    fn a_rejection_with_a_stalled_body_keeps_its_status() {
        let response =
            post_to_a_stalled_status(&loopback_transport(), 401, Some(Duration::from_millis(200)))
                .unwrap();
        assert_eq!((response.status, response.body), (401, None));
    }
}
