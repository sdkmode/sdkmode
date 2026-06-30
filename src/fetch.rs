use deno_error::JsErrorBox;
use deno_fetch::{ReqBody, RequestBuilder};
use http;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;

struct RequestInterceptor {
    sdks: Vec<Arc<Mutex<dyn crate::sdk::Sdk>>>,
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
            let path = request.uri().path();

            // Git smart-HTTP lives on the web host (e.g. `github.com`) rather than
            // the API host (`api.github.com`), and authenticates with Basic, not
            // Bearer. We broker it by pairing the web host with whichever SDK owns
            // the `api.<host>` API host, but only on the well-known git endpoints —
            // a bare web fetch to the host never receives credentials.
            let is_git_smart_http = path.ends_with("/info/refs")
                || path.ends_with("/git-upload-pack")
                || path.ends_with("/git-receive-pack");
            let git_api_host = host.as_deref().map(|h| format!("api.{h}"));

            // Find a registered SDK matching this request: directly on its API
            // host, or (for git requests) on the paired `api.<host>` host.
            let mut matched: Option<(Arc<Mutex<dyn crate::sdk::Sdk>>, bool)> = None;
            for candidate in &self.sdks {
                let api_host = candidate.lock().await.url().host_str().map(str::to_string);
                if api_host.as_deref() == host.as_deref() {
                    matched = Some((candidate.clone(), false));
                    break;
                }
                if is_git_smart_http && api_host.as_deref() == git_api_host.as_deref() {
                    matched = Some((candidate.clone(), true));
                    break;
                }
            }

            let Some((sdk, is_git)) = matched else {
                // Not a registered SDK: this is general web access. We inject no
                // credentials, but block requests to the host's own loopback,
                // private, and link-local ranges (SSRF / cloud metadata).
                return match host.as_deref() {
                    Some(host) if !is_blocked_host(host) => Ok(()),
                    other => Err(JsErrorBox::generic(format!(
                        "network blocked: {}",
                        other.unwrap_or("<no host>")
                    ))),
                };
            };

            let mut sdk = sdk.lock().await;

            if !sdk.is_authed() {
                sdk.auth().await?;
            }

            // For git smart-HTTP, present the SDK's token as HTTP Basic (token as
            // the password) instead of the API's Bearer scheme.
            let auth = match (is_git, sdk.auth_header()) {
                (true, Some(bearer)) => {
                    use base64::Engine;
                    let token = bearer.strip_prefix("Bearer ").unwrap_or(&bearer).trim();
                    let creds = base64::engine::general_purpose::STANDARD
                        .encode(format!("x-access-token:{token}"));
                    Some(format!("Basic {creds}"))
                }
                (false, header) => header,
                (true, None) => None,
            };

            let headers = request.headers_mut();

            headers.remove(http::header::AUTHORIZATION);
            headers.remove(http::header::COOKIE);

            if let Some(auth_header) = auth {
                headers.insert(
                    http::header::AUTHORIZATION,
                    auth_header
                        .parse::<http::HeaderValue>()
                        .or_else(|e| Err(JsErrorBox::generic(e.to_string())))?,
                );
            }

            if !is_git {
                if let Some(cookies) = sdk.cookies() {
                    if !cookies.is_empty() {
                        let cookie_str = cookies
                            .iter()
                            .map(|(k, v)| format!("{k}={v}"))
                            .collect::<Vec<_>>()
                            .join("; ");
                        headers.insert(
                            http::header::COOKIE,
                            cookie_str
                                .parse::<http::HeaderValue>()
                                .or_else(|e| Err(JsErrorBox::generic(e.to_string())))?,
                        );
                    }
                }
            }

            Ok(())
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

    if host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("metadata.google.internal")
    {
        return true;
    }

    match host.parse::<IpAddr>() {
        Ok(ip) => is_blocked_ip(ip),
        Err(_) => false,
    }
}

fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 0
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                // fe80::/10 link-local
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // fc00::/7 unique local
                || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

pub(crate) fn fetch_options() -> deno_fetch::Options {
    deno_fetch::Options {
        request_builder_hook: Some(Arc::new(RequestInterceptor::new())),
        ..Default::default()
    }
}
