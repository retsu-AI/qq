//! Network egress policy for `fetch`: which hosts a run may reach at all, and
//! which addresses a name may resolve to. Names are judged before a request
//! exists (managed denies, private suffixes); addresses are judged after
//! resolution and before connecting, and the connection is pinned to the
//! judged addresses so a second lookup cannot rebind to a private one.

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
};

use ipnet::{IpNet, Ipv4Net, Ipv6Net};

/// The workspace's managed host denies, translated from configuration and
/// carried on the compiled plan. Exact lowercase names or `*.suffix`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NetworkPolicy {
    pub deny_hosts: Arc<[String]>,
    /// Test-only: admit loopback and private addresses so fixture servers on
    /// `127.0.0.1` can stand in for the public internet. Never set from
    /// configuration.
    #[doc(hidden)]
    pub allow_private_for_tests: bool,
}

/// Why a host or address may not be reached. Rendered to the model verbatim
/// so it stops trying rather than retrying with variations.
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
    #[error("host {host} did not resolve to any address")]
    Unresolved { host: String },
}

/// Whether a host grant (exact or `*.suffix`) covers a request host. A
/// wildcard covers any subdomain and never the apex.
#[must_use]
pub fn host_grant_matches(grant: &str, host: &str) -> bool {
    match grant.strip_prefix("*.") {
        Some(suffix) => host
            .strip_suffix(suffix)
            .is_some_and(|head| head.len() > 1 && head.ends_with('.')),
        None => grant.eq_ignore_ascii_case(host),
    }
}

/// Name suffixes that never leave the local network.
const PRIVATE_SUFFIXES: &[&str] = &[".local", ".internal", ".localhost", ".home.arpa", ".lan"];

/// Cloud instance-metadata names; their addresses are in the private ranges
/// too, but the name check refuses before any lookup.
const METADATA_HOSTS: &[&str] = &[
    "metadata.google.internal",
    "metadata",
    "instance-data",
    "instance-data.ec2.internal",
];

/// Judges the host name before resolution: scheme, presence, managed deny,
/// private suffixes, metadata names, and IP literals in private ranges.
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

/// Judges one resolved address. Refused ranges: unspecified, loopback,
/// RFC 1918, shared address space (100.64/10), link-local (incl. the
/// 169.254.169.254 metadata address), ULA, IPv6 link-local, multicast,
/// documentation and benchmark ranges, and IPv4-mapped IPv6 forms of any of
/// those.
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

fn is_loopback_or_private_for_tests(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private(),
        IpAddr::V6(v6) => v6.is_loopback(),
    }
}

fn is_private_v4(address: Ipv4Addr) -> bool {
    const REFUSED: &[Ipv4Net] = &[
        // 0.0.0.0/8 "this" network
        Ipv4Net::new_assert(Ipv4Addr::new(0, 0, 0, 0), 8),
        // 10/8
        Ipv4Net::new_assert(Ipv4Addr::new(10, 0, 0, 0), 8),
        // 100.64/10 shared address space
        Ipv4Net::new_assert(Ipv4Addr::new(100, 64, 0, 0), 10),
        // 127/8 loopback
        Ipv4Net::new_assert(Ipv4Addr::new(127, 0, 0, 0), 8),
        // 169.254/16 link-local (metadata lives here)
        Ipv4Net::new_assert(Ipv4Addr::new(169, 254, 0, 0), 16),
        // 172.16/12
        Ipv4Net::new_assert(Ipv4Addr::new(172, 16, 0, 0), 12),
        // 192.0.0/24 IETF protocol assignments
        Ipv4Net::new_assert(Ipv4Addr::new(192, 0, 0, 0), 24),
        // 192.0.2/24 TEST-NET-1
        Ipv4Net::new_assert(Ipv4Addr::new(192, 0, 2, 0), 24),
        // 192.168/16
        Ipv4Net::new_assert(Ipv4Addr::new(192, 168, 0, 0), 16),
        // 198.18/15 benchmarking
        Ipv4Net::new_assert(Ipv4Addr::new(198, 18, 0, 0), 15),
        // 198.51.100/24 TEST-NET-2
        Ipv4Net::new_assert(Ipv4Addr::new(198, 51, 100, 0), 24),
        // 203.0.113/24 TEST-NET-3
        Ipv4Net::new_assert(Ipv4Addr::new(203, 0, 113, 0), 24),
        // 224/4 multicast, 240/4 reserved + broadcast
        Ipv4Net::new_assert(Ipv4Addr::new(224, 0, 0, 0), 4),
        Ipv4Net::new_assert(Ipv4Addr::new(240, 0, 0, 0), 4),
    ];
    REFUSED.iter().any(|net| net.contains(&address))
}

fn is_private_v6(address: Ipv6Addr) -> bool {
    const REFUSED: &[Ipv6Net] = &[
        // ::/128 unspecified and ::1/128 loopback
        Ipv6Net::new_assert(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 127),
        // 100::/64 discard
        Ipv6Net::new_assert(Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
        // 2001:db8::/32 documentation
        Ipv6Net::new_assert(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
        // fc00::/7 unique local
        Ipv6Net::new_assert(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
        // fe80::/10 link-local
        Ipv6Net::new_assert(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
        // ff00::/8 multicast
        Ipv6Net::new_assert(Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
    ];
    REFUSED.iter().any(|net| net.contains(&address))
        // NAT64 (64:ff9b::/96) carries an IPv4 in the low 32 bits: judge
        // that address, so a public one passes and a private one does not.
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

/// Keeps `IpNet` referenced so the dependency is exercised by one type in
/// both families (the constant tables above are typed per family).
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
        // The test escape admits loopback and RFC 1918 only, never metadata.
        assert!(check_address("h", "169.254.169.254".parse().unwrap(), &test_policy).is_err());
    }
}
