#[cfg(any(feature = "tool-fetch", test))]
use ipnet::{IpNet, Ipv4Net, Ipv6Net};
use std::net::IpAddr;
#[cfg(any(feature = "tool-fetch", test))]
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkPolicy {
    pub deny_hosts: Arc<[String]>,
    #[doc(hidden)]
    pub allow_private_for_tests: bool,
}

#[cfg_attr(not(feature = "tool-fetch"), allow(dead_code))]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum HostRefusal {
    #[error("url must be http or https")]
    Scheme,
    #[error("url has no host")]
    NoHost,
    #[error("host {host} is denied by managed policy")]
    ManagedDeny { host: String },
    #[error("host {host} is a private or link-local name")]
    PrivateName { host: String },
    #[error("host {host} is a cloud metadata endpoint")]
    Metadata { host: String },
    #[error("host {host} resolves to a private, loopback, or link-local address ({address})")]
    PrivateAddress { host: String, address: IpAddr },
    #[cfg(feature = "tool-fetch")]
    #[error("host {host} did not resolve to any address")]
    Unresolved { host: String },
}

#[must_use]
pub fn host_grant_matches(grant: &str, host: &str) -> bool {
    match grant.strip_prefix("*.") {
        Some(suffix) => host
            .strip_suffix(suffix)
            .is_some_and(|head| head.len() > 1 && head.ends_with('.')),
        None => grant.eq_ignore_ascii_case(host),
    }
}

#[cfg(any(feature = "tool-fetch", test))]
const PRIVATE_SUFFIXES: &[&str] = &[".local", ".internal", ".localhost", ".home.arpa", ".lan"];

#[cfg(any(feature = "tool-fetch", test))]
const METADATA_HOSTS: &[&str] = &[
    "metadata.google.internal",
    "metadata",
    "instance-data",
    "instance-data.ec2.internal",
];

#[cfg(any(feature = "tool-fetch", test))]
pub(crate) fn check_host_name(
    url: &url::Url,
    policy: &NetworkPolicy,
) -> Result<String, HostRefusal> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err(HostRefusal::Scheme);
    }
    let host = match url.host() {
        Some(url::Host::Domain(name)) => name.trim_end_matches('.').to_ascii_lowercase(),
        Some(url::Host::Ipv4(address)) => {
            check_address(
                url.host_str().unwrap_or_default(),
                IpAddr::V4(address),
                policy,
            )?;
            return Ok(address.to_string());
        }
        Some(url::Host::Ipv6(address)) => {
            check_address(
                url.host_str().unwrap_or_default(),
                IpAddr::V6(address),
                policy,
            )?;
            return Ok(address.to_string());
        }
        None => return Err(HostRefusal::NoHost),
    };
    if host.is_empty() {
        return Err(HostRefusal::NoHost);
    }
    if policy
        .deny_hosts
        .iter()
        .any(|denied| host_grant_matches(denied, &host))
    {
        return Err(HostRefusal::ManagedDeny { host });
    }
    if METADATA_HOSTS.contains(&host.as_str()) {
        return Err(HostRefusal::Metadata { host });
    }
    if !policy.allow_private_for_tests
        && (host == "localhost"
            || PRIVATE_SUFFIXES.iter().any(|suffix| host.ends_with(suffix))
            || !host.contains('.'))
    {
        return Err(HostRefusal::PrivateName { host });
    }
    Ok(host)
}

#[cfg(any(feature = "tool-fetch", test))]
pub(crate) fn check_address(
    host: &str,
    address: IpAddr,
    policy: &NetworkPolicy,
) -> Result<(), HostRefusal> {
    let refused = match address {
        IpAddr::V4(v4) => is_private_v4(v4),
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_private_v4(v4),
            None => is_private_v6(v6),
        },
    };
    if refused && !(policy.allow_private_for_tests && is_loopback_or_private_for_tests(address)) {
        return Err(HostRefusal::PrivateAddress {
            host: host.to_owned(),
            address,
        });
    }
    Ok(())
}

#[cfg(any(feature = "tool-fetch", test))]
fn is_loopback_or_private_for_tests(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

#[cfg(any(feature = "tool-fetch", test))]
fn is_private_v4(address: Ipv4Addr) -> bool {
    const REFUSED: &[Ipv4Net] = &[
        Ipv4Net::new_assert(Ipv4Addr::new(0, 0, 0, 0), 8),
        Ipv4Net::new_assert(Ipv4Addr::new(10, 0, 0, 0), 8),
        Ipv4Net::new_assert(Ipv4Addr::new(100, 64, 0, 0), 10),
        Ipv4Net::new_assert(Ipv4Addr::new(127, 0, 0, 0), 8),
        Ipv4Net::new_assert(Ipv4Addr::new(169, 254, 0, 0), 16),
        Ipv4Net::new_assert(Ipv4Addr::new(172, 16, 0, 0), 12),
        Ipv4Net::new_assert(Ipv4Addr::new(192, 0, 0, 0), 24),
        Ipv4Net::new_assert(Ipv4Addr::new(192, 0, 2, 0), 24),
        Ipv4Net::new_assert(Ipv4Addr::new(192, 168, 0, 0), 16),
        Ipv4Net::new_assert(Ipv4Addr::new(198, 18, 0, 0), 15),
        Ipv4Net::new_assert(Ipv4Addr::new(198, 51, 100, 0), 24),
        Ipv4Net::new_assert(Ipv4Addr::new(203, 0, 113, 0), 24),
        Ipv4Net::new_assert(Ipv4Addr::new(224, 0, 0, 0), 4),
        Ipv4Net::new_assert(Ipv4Addr::new(240, 0, 0, 0), 4),
    ];
    REFUSED.iter().any(|net| net.contains(&address))
}

#[cfg(any(feature = "tool-fetch", test))]
fn is_private_v6(address: Ipv6Addr) -> bool {
    const REFUSED: &[Ipv6Net] = &[
        Ipv6Net::new_assert(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 127),
        Ipv6Net::new_assert(Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
        Ipv6Net::new_assert(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
        Ipv6Net::new_assert(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
        Ipv6Net::new_assert(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
        Ipv6Net::new_assert(Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
    ];
    REFUSED.iter().any(|net| net.contains(&address))
        || (address.segments()[0] == 0x64 && address.segments()[1] == 0xff9b && {
            let [.., a, b] = address.segments();
            is_private_v4(Ipv4Addr::new(
                (a >> 8) as u8,
                (a & 0xff) as u8,
                (b >> 8) as u8,
                (b & 0xff) as u8,
            ))
        })
}

#[cfg(any(feature = "tool-fetch", test))]
#[allow(dead_code)]
const fn _uses_ipnet(_: IpNet) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(deny: &[&str]) -> NetworkPolicy {
        NetworkPolicy {
            deny_hosts: deny.iter().map(|host| (*host).to_owned()).collect(),
            allow_private_for_tests: false,
        }
    }

    fn name(url: &str, policy: &NetworkPolicy) -> Result<String, HostRefusal> {
        check_host_name(&url::Url::parse(url).unwrap(), policy)
    }

    #[test]
    fn host_grants_match_exact_names_and_subdomains_of_wildcards() {
        assert!(host_grant_matches("docs.rs", "docs.rs"));
        assert!(host_grant_matches("docs.rs", "DOCS.RS"));
        assert!(!host_grant_matches("docs.rs", "notdocs.rs"));
        assert!(host_grant_matches("*.github.com", "api.github.com"));
        assert!(host_grant_matches("*.github.com", "a.b.github.com"));
        assert!(!host_grant_matches("*.github.com", "github.com"));
        assert!(!host_grant_matches("*.github.com", "evilgithub.com"));
    }

    #[test]
    fn names_are_refused_for_scheme_managed_deny_private_suffixes_and_metadata() {
        let open = policy(&[]);
        assert_eq!(
            name("https://docs.rs/axum", &open),
            Ok("docs.rs".to_owned())
        );
        assert_eq!(name("https://Docs.RS./x", &open), Ok("docs.rs".to_owned()));
        assert_eq!(name("ftp://docs.rs/", &open), Err(HostRefusal::Scheme));
        assert_eq!(name("file:///etc/passwd", &open), Err(HostRefusal::Scheme));
        for url in [
            "http://localhost/",
            "http://intranet/",
            "http://printer.local/",
            "http://db.internal/",
            "http://router.lan/",
        ] {
            assert!(
                matches!(name(url, &open), Err(HostRefusal::PrivateName { .. })),
                "{url}"
            );
        }
        assert!(matches!(
            name("http://metadata.google.internal/computeMetadata/v1/", &open),
            Err(HostRefusal::Metadata { .. })
        ));
        assert!(matches!(
            name("http://169.254.169.254/latest/meta-data/", &open),
            Err(HostRefusal::PrivateAddress { .. })
        ));
        let denied = policy(&["*.example.com", "tracker.io"]);
        assert!(matches!(
            name("https://api.example.com/", &denied),
            Err(HostRefusal::ManagedDeny { .. })
        ));
        assert!(matches!(
            name("https://tracker.io/", &denied),
            Err(HostRefusal::ManagedDeny { .. })
        ));
        assert_eq!(
            name("https://example.com/", &denied),
            Ok("example.com".to_owned())
        );
    }

    #[test]
    fn addresses_in_private_ranges_are_refused_including_mapped_and_nat64_forms() {
        let open = policy(&[]);
        let refused = [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "127.255.255.254",
            "169.254.169.254",
            "172.16.0.1",
            "172.31.255.255",
            "192.0.0.1",
            "192.0.2.1",
            "192.168.1.1",
            "198.18.0.1",
            "203.0.113.7",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "fc00::1",
            "fd12::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "::ffff:127.0.0.1",
            "::ffff:10.0.0.1",
            "64:ff9b::7f00:1",
        ];
        for text in refused {
            let address: IpAddr = text.parse().unwrap();
            assert!(
                check_address("h", address, &open).is_err(),
                "{text} must be refused"
            );
        }
        for text in [
            "1.1.1.1",
            "8.8.8.8",
            "93.184.216.34",
            "2606:4700::1111",
            "64:ff9b::808:808",
        ] {
            let address: IpAddr = text.parse().unwrap();
            assert_eq!(check_address("h", address, &open), Ok(()), "{text}");
        }
        let test_policy = NetworkPolicy {
            deny_hosts: Arc::from(Vec::new()),
            allow_private_for_tests: true,
        };
        assert_eq!(
            check_address("h", "127.0.0.1".parse().unwrap(), &test_policy),
            Ok(())
        );
        assert!(check_address("h", "169.254.169.254".parse().unwrap(), &test_policy).is_err());
    }
}
