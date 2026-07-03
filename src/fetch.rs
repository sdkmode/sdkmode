use deno_error::JsErrorBox;
use deno_fetch::{ReqBody, RequestBuilder};
use std::net::{IpAddr, Ipv4Addr};
use std::pin::Pin;
use std::sync::Arc;

struct RequestInterceptor {
    sdks: Vec<Arc<dyn crate::sdk::Sdk>>,
}

impl RequestInterceptor {
    fn new() -> Self {
        Self {
            sdks: crate::sdk::registry(),
        }
    }
}

impl RequestBuilder for RequestInterceptor {
    fn hook<'a>(
        &'a self,
        request: &'a mut http::Request<ReqBody>,
    ) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>> {
        Box::pin(async move {
            let host = request.uri().host().map(str::to_string);
            let path = request.uri().path().to_string();

            // Ask each SDK's auth facet whether it brokers this request.
            // Claiming is pure host/path matching, so no auth state is touched
            // for the (common) unclaimed case.
            let claimed = host.as_deref().and_then(|host| {
                self.sdks
                    .iter()
                    .filter_map(|sdk| sdk.auth())
                    .find(|auth| auth.claims(host, &path))
            });

            let Some(auth) = claimed else {
                // Not claimed by any SDK: this is general web access. We inject
                // no credentials, but block requests to the host's own loopback,
                // private, and link-local ranges (SSRF / cloud metadata).
                return match host.as_deref() {
                    Some(host) if !is_blocked_host(host) => Ok(()),
                    other => Err(JsErrorBox::generic(format!(
                        "network blocked: {}",
                        other.unwrap_or("<no host>")
                    ))),
                };
            };

            // Brokered request: guest-supplied credentials never pass through —
            // strip them centrally, then let the SDK inject its own.
            let headers = request.headers_mut();
            headers.remove(http::header::AUTHORIZATION);
            headers.remove(http::header::COOKIE);

            auth.apply(request).await
        })
    }
}

/// Whether a host should be refused for general web access. Covers the obvious
/// SSRF targets: `localhost`, the cloud metadata hostname, and literal IPs in
/// loopback / private / link-local / unspecified ranges.
///
/// Note: this does not resolve hostnames, so a domain that resolves to a private
/// address (DNS rebinding) is not caught here — that would need a connect-time
/// check in the HTTP connector.
fn is_blocked_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    // Normalize an FQDN trailing dot: `localhost.` is still localhost.
    let host = host.strip_suffix('.').unwrap_or(host);

    // `localhost` and any name under it (`foo.localhost`) resolve to loopback
    // per RFC 6761, so match subdomains too.
    if host.eq_ignore_ascii_case("localhost")
        || ends_with_ignore_case(host, ".localhost")
        || host.eq_ignore_ascii_case("metadata.google.internal")
    {
        return true;
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_blocked_ip(ip);
    }

    // Best-effort for non-dotted IPv4 literals. A host like `2130706433` is a
    // valid way to write `127.0.0.1` (curl/browsers accept it) but does not
    // parse as an `IpAddr`, so without this it would slip past the blocklist.
    // We only handle the plain big-endian decimal `u32` form: it's the common
    // one and unambiguous. Octal/hex/dotted-partial forms are deliberately left
    // out — they're rarely used and easy to get subtly wrong.
    if !host.is_empty()
        && host.bytes().all(|b| b.is_ascii_digit())
        && let Ok(n) = host.parse::<u32>()
    {
        return is_blocked_v4(Ipv4Addr::from(n));
    }

    false
}

fn ends_with_ignore_case(host: &str, suffix: &str) -> bool {
    // Compare as bytes: a string slice at `len - suffix.len()` would panic if
    // that byte offset fell inside a multi-byte character of a non-ASCII host.
    host.len() >= suffix.len()
        && host.as_bytes()[host.len() - suffix.len()..].eq_ignore_ascii_case(suffix.as_bytes())
}

/// The v4 blocking rules, split out so they can also be applied to IPv4-mapped
/// IPv6 addresses and to integer IP literals.
fn is_blocked_v4(v4: Ipv4Addr) -> bool {
    let octets = v4.octets();
    v4.is_loopback()
        || v4.is_private()
        || v4.is_link_local()
        || v4.is_unspecified()
        || v4.is_broadcast()
        // 224.0.0.0/4 multicast
        || v4.is_multicast()
        || octets[0] == 0
        // 100.64.0.0/10 shared address space / CGNAT (RFC 6598); some cloud
        // metadata services live here (e.g. Alibaba's 100.100.100.200).
        || (octets[0] == 100 && (octets[1] & 0xc0) == 0x40)
        // 240.0.0.0/4 reserved (also re-covers 255.255.255.255 broadcast)
        || octets[0] >= 240
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => {
            // IPv4-mapped (`::ffff:a.b.c.d`) and IPv4-compatible
            // (`::a.b.c.d`) addresses reach v4 destinations but don't trip the
            // v6 range checks below — `is_loopback` only matches `::1`. Re-run
            // the v4 rules on the embedded address so e.g.
            // `::ffff:127.0.0.1` and `::ffff:169.254.169.254` stay blocked.
            if let Some(v4) = v6.to_ipv4_mapped().or_else(|| v6.to_ipv4())
                && is_blocked_v4(v4)
            {
                return true;
            }

            // NAT64 well-known prefix 64:ff9b::/96 (RFC 6052) also embeds a
            // reachable IPv4 address in the last 32 bits; judge it by the
            // embedded address as well.
            let seg = v6.segments();
            if seg[..6] == [0x64, 0xff9b, 0, 0, 0, 0] {
                let v4 = Ipv4Addr::new(
                    (seg[6] >> 8) as u8,
                    seg[6] as u8,
                    (seg[7] >> 8) as u8,
                    seg[7] as u8,
                );
                if is_blocked_v4(v4) {
                    return true;
                }
            }

            v6.is_loopback()
                || v6.is_unspecified()
                // fe80::/10 link-local
                || (seg[0] & 0xffc0) == 0xfe80
                // fc00::/7 unique local
                || (seg[0] & 0xfe00) == 0xfc00
                // ff00::/8 multicast
                || (seg[0] & 0xff00) == 0xff00
        }
    }
}

pub(crate) fn fetch_options() -> deno_fetch::Options {
    deno_fetch::Options {
        request_builder_hook: Some(Arc::new(RequestInterceptor::new())),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::is_blocked_host;

    // Existing behavior we must not regress.
    #[test]
    fn blocks_named_ssrf_targets() {
        assert!(is_blocked_host("localhost"));
        assert!(is_blocked_host("LocalHost"));
        assert!(is_blocked_host("metadata.google.internal"));
    }

    #[test]
    fn blocks_plain_v4_ranges() {
        assert!(is_blocked_host("127.0.0.1"));
        assert!(is_blocked_host("169.254.169.254"));
        assert!(is_blocked_host("10.0.0.1"));
    }

    #[test]
    fn allows_public_v4() {
        assert!(!is_blocked_host("8.8.8.8"));
    }

    // Bug 1: IPv4-mapped IPv6 addresses must re-run the v4 rules. Hosts arrive
    // bracketed from a URI, so exercise both the bracketed and bare forms.
    #[test]
    fn blocks_ipv4_mapped_loopback() {
        assert!(is_blocked_host("::ffff:127.0.0.1"));
        assert!(is_blocked_host("[::ffff:127.0.0.1]"));
    }

    #[test]
    fn blocks_ipv4_mapped_link_local_metadata() {
        assert!(is_blocked_host("::ffff:169.254.169.254"));
        assert!(is_blocked_host("[::ffff:169.254.169.254]"));
    }

    #[test]
    fn allows_public_v6() {
        // A genuine public v6 address has no embedded v4 form and hits none of
        // the v6 range checks, so it stays allowed.
        assert!(!is_blocked_host("2606:4700:4700::1111"));
        assert!(!is_blocked_host("[2606:4700:4700::1111]"));
    }

    // Bug 2: decimal integer IP literals.
    #[test]
    fn blocks_decimal_literal_loopback() {
        // 2130706433 == 0x7f000001 == 127.0.0.1
        assert!(is_blocked_host("2130706433"));
    }

    #[test]
    fn allows_decimal_literal_public() {
        // 134744072 == 0x08080808 == 8.8.8.8
        assert!(!is_blocked_host("134744072"));
    }

    // Bug 3: FQDN trailing dots and `.localhost` subdomains still name loopback.
    #[test]
    fn blocks_trailing_dot_and_localhost_subdomains() {
        assert!(is_blocked_host("localhost."));
        assert!(is_blocked_host("metadata.google.internal."));
        assert!(is_blocked_host("foo.localhost"));
        assert!(is_blocked_host("Foo.LocalHost."));
        // Not a subdomain of localhost — must stay allowed.
        assert!(!is_blocked_host("notlocalhost"));
        assert!(!is_blocked_host("example.com"));
    }

    // Bug 4: additional reserved v4 ranges.
    #[test]
    fn blocks_extra_v4_ranges() {
        // 100.64.0.0/10 shared address space / CGNAT (cloud metadata hosts).
        assert!(is_blocked_host("100.100.100.200"));
        assert!(is_blocked_host("100.64.0.1"));
        assert!(!is_blocked_host("100.63.255.255"));
        assert!(!is_blocked_host("100.128.0.1"));
        // Multicast and reserved.
        assert!(is_blocked_host("224.0.0.1"));
        assert!(is_blocked_host("240.0.0.1"));
    }

    // Bug 5: additional v6 forms that reach blocked targets.
    #[test]
    fn blocks_extra_v6_forms() {
        // NAT64 well-known prefix embedding a blocked v4 address.
        assert!(is_blocked_host("64:ff9b::127.0.0.1"));
        assert!(is_blocked_host("[64:ff9b::a9fe:a9fe]")); // 169.254.169.254
        assert!(!is_blocked_host("64:ff9b::8.8.8.8"));
        // IPv4-compatible (deprecated ::/96) embedding.
        assert!(is_blocked_host("::127.0.0.1"));
        // v6 multicast.
        assert!(is_blocked_host("ff02::1"));
    }
}
