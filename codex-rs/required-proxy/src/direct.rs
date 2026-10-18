//! Destinations that may bypass the required proxy, and those that never may.

use std::fmt;
use std::net::IpAddr;

use crate::RequiredProxyError;

/// Registrable domains that always use the upstream proxy and cannot be listed as direct.
pub const PINNED_DOMAINS: &[&str] = &[
    "chatgpt-staging.com",
    "chatgpt.com",
    "oaistatic.com",
    "oaiusercontent.com",
    "openai.com",
    "openai.org",
    // Codex feedback and crash reports.
    "sentry.io",
];

/// Loopback destinations always stay direct: local IPC never needs the upstream proxy.
pub const IMPLICIT_DIRECT: &[&str] = &["localhost", "127.0.0.1", "::1"];

/// Non-public ranges that may be listed as direct.
const DIRECT_RANGES: &[(&str, u8)] = &[
    ("127.0.0.0", 8),
    ("10.0.0.0", 8),
    ("172.16.0.0", 12),
    ("192.168.0.0", 16),
    // Carrier-grade NAT, including Tailscale.
    ("100.64.0.0", 10),
    ("169.254.0.0", 16),
    ("::1", 128),
    // Unique local addresses, including Tailscale.
    ("fc00::", 7),
    ("fe80::", 10),
];

/// Whether `host` belongs to a destination that must always use the upstream proxy.
pub fn is_pinned_host(host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    PINNED_DOMAINS
        .iter()
        .any(|pinned| host == *pinned || host.ends_with(&format!(".{pinned}")))
}

/// Destinations that connect directly. Pinned OpenAI hosts never match.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DirectList {
    entries: Vec<DirectEntry>,
}

impl DirectList {
    /// Validates entries, dropping duplicates.
    pub fn parse(entries: &[String]) -> Result<Self, RequiredProxyError> {
        let mut list = Self::default();
        for entry in entries {
            let (entry, _) = DirectEntry::parse(entry)?;
            if !list.entries.contains(&entry) {
                list.entries.push(entry);
            }
        }
        Ok(list)
    }

    /// Whether `host` (a domain or IP literal, brackets optional) connects directly.
    pub fn matches(&self, host: &str) -> bool {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        !is_pinned_host(host) && self.entries.iter().any(|entry| entry.matches(host))
    }

    /// The entries in `NO_PROXY` spelling.
    pub fn entries(&self) -> Vec<String> {
        self.entries.iter().map(ToString::to_string).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DirectEntry {
    text: String,
    kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Kind {
    /// Matches the domain and its subdomains.
    Domain(String),
    Network {
        address: IpAddr,
        prefix: u8,
    },
}

impl DirectEntry {
    /// Parses a `[proxy] direct` entry and returns it with its `NO_PROXY` spelling.
    pub(crate) fn parse(entry: &str) -> Result<(Self, String), RequiredProxyError> {
        let invalid = |reason| RequiredProxyError::InvalidDirectEntry {
            entry: entry.to_string(),
            reason,
        };
        let normalized = entry.trim().to_ascii_lowercase();
        if normalized.is_empty() {
            return Err(invalid("empty entry"));
        }
        if normalized == "*" {
            return Err(invalid(
                "a wildcard would bypass the proxy for every destination",
            ));
        }
        if let Some((address, prefix)) = normalized.split_once('/') {
            let address: IpAddr = address.parse().map_err(|_| invalid("invalid CIDR"))?;
            let prefix: u8 = prefix.parse().map_err(|_| invalid("invalid CIDR"))?;
            if !is_direct_network(address, prefix) {
                return Err(invalid(
                    "only loopback, private and link-local ranges may bypass the proxy",
                ));
            }
            return Ok(Self::network(normalized, address, prefix));
        }
        let unbracketed = normalized.trim_start_matches('[').trim_end_matches(']');
        if let Ok(address) = unbracketed.parse::<IpAddr>() {
            let prefix = if address.is_ipv4() { 32 } else { 128 };
            if !is_direct_network(address, prefix) {
                return Err(invalid(
                    "only loopback, private and link-local addresses may bypass the proxy",
                ));
            }
            return Ok(Self::network(unbracketed.to_string(), address, prefix));
        }
        let domain = normalized
            .strip_prefix("*.")
            .or_else(|| normalized.strip_prefix('.'))
            .unwrap_or(&normalized);
        let valid_label = |label: &str| {
            !label.is_empty() && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        };
        if !domain.split('.').all(valid_label) {
            return Err(invalid("expected a domain, IP address or CIDR"));
        }
        let overlaps_pinned = PINNED_DOMAINS.iter().any(|pinned| {
            domain == *pinned
                || domain.ends_with(&format!(".{pinned}"))
                || pinned.ends_with(&format!(".{domain}"))
        });
        if overlaps_pinned {
            return Err(invalid("OpenAI destinations always use the proxy"));
        }
        // A leading dot is kept for NO_PROXY readers; both spellings match subdomains.
        let text = if normalized.starts_with('.') || normalized.starts_with("*.") {
            format!(".{domain}")
        } else {
            domain.to_string()
        };
        let entry = Self {
            text: text.clone(),
            kind: Kind::Domain(domain.to_string()),
        };
        Ok((entry, text))
    }

    fn network(text: String, address: IpAddr, prefix: u8) -> (Self, String) {
        let entry = Self {
            text: text.clone(),
            kind: Kind::Network { address, prefix },
        };
        (entry, text)
    }

    pub(crate) fn matches(&self, host: &str) -> bool {
        match &self.kind {
            Kind::Domain(domain) => {
                let host = host.trim_end_matches('.').to_ascii_lowercase();
                host == *domain || host.ends_with(&format!(".{domain}"))
            }
            Kind::Network { address, prefix } => host
                .parse::<IpAddr>()
                .is_ok_and(|host| in_network(host, *address, *prefix)),
        }
    }
}

impl fmt::Display for DirectEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

fn is_direct_network(address: IpAddr, prefix: u8) -> bool {
    DIRECT_RANGES.iter().any(|(network, range_prefix)| {
        network.parse().is_ok_and(|network| {
            prefix >= *range_prefix
                && prefix <= max_prefix(address)
                && in_network(address, network, *range_prefix)
        })
    })
}

fn max_prefix(address: IpAddr) -> u8 {
    if address.is_ipv4() { 32 } else { 128 }
}

fn in_network(address: IpAddr, network: IpAddr, prefix: u8) -> bool {
    match (address, network) {
        (IpAddr::V4(address), IpAddr::V4(network)) => {
            let mask = u32::MAX
                .checked_shl(32 - u32::from(prefix.min(32)))
                .unwrap_or(0);
            u32::from(address) & mask == u32::from(network) & mask
        }
        (IpAddr::V6(address), IpAddr::V6(network)) => {
            let mask = u128::MAX
                .checked_shl(128 - u32::from(prefix.min(128)))
                .unwrap_or(0);
            u128::from(address) & mask == u128::from(network) & mask
        }
        _ => false,
    }
}
