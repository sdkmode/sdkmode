//! The GitHub SDK: an authenticated Octokit client in the sandbox, with
//! credentials brokered host-side. Owns everything GitHub-shaped: the
//! `@octokit/rest` pin, the lazy `globalThis.octokit` shim, the prompt docs,
//! and the [`Auth`] facet that injects a token into API requests — including
//! git smart-HTTP requests on the web host, which authenticate with Basic
//! rather than the API's Bearer scheme.

use deno_error::JsErrorBox;
use std::pin::Pin;
use std::sync::Arc;
use tokio::process::Command;

use super::{Auth, Docs, Sdk};

const API_HOST: &str = "api.github.com";
const WEB_HOST: &str = "github.com";

/// esm.sh version pin for `@octokit/rest`; see [`Sdk::imports`] on bumping.
const IMPORTS: &[(&str, &str)] = &[("@octokit/rest", "https://esm.sh/@octokit/rest@22.0.1")];

/// Exposes `globalThis.octokit`: an authenticated Octokit client that lazily
/// imports `@octokit/rest` (from the pinned esm.sh URL) and constructs itself
/// the first time it is touched, then is reused for the rest of the session.
/// Modeled on the browser shim — a Proxy defers the (async, network) import
/// until an actual access — so building a session needs no network and the
/// REPL can start offline for local-only tasks.
///
/// Octokit is used through nested member chains (e.g.
/// `octokit.rest.users.getAuthenticated()`), so a top-level `get` returns a
/// *nested* proxy that keeps deferring: reads walk a property path, and a call
/// awaits the real Octokit, walks that same path, and invokes the method with
/// the correct `this`. Every leaf is a thenable, so `await octokit.rest.x.y()`
/// resolves in a single expression exactly as the model writes it.
const OCTOKIT_SHIM: &str = r#"
((globalThis) => {
    let loading = null;
    // Import + construct once; a failed attempt is not cached so a later use
    // (after transient network trouble) retries.
    const ready = () =>
        (loading ??= (async () => {
            try {
                const { Octokit } = await import("@octokit/rest");
                return new Octokit();
            } catch (error) {
                loading = null;
                throw error;
            }
        })());

    // A proxy over the property path walked so far (`path`). Reading a property
    // extends the path and returns another such proxy; `then` / calling resolve
    // the real client and follow the path.
    const make = (path) =>
        new Proxy(function () {}, {
            get(_target, prop) {
                // `await octokit...` resolves the value at the current path.
                if (prop === "then") {
                    return (onFulfilled, onRejected) =>
                        ready()
                            .then((root) => path.reduce((obj, key) => obj[key], root))
                            .then(onFulfilled, onRejected);
                }
                if (typeof prop === "symbol") return undefined;
                return make([...path, prop]);
            },
            // Calling forwards to the method at the parent path, bound to its
            // owner so Octokit's internal `this` is correct.
            apply(_target, _thisArg, args) {
                return ready().then((root) => {
                    const parent = path
                        .slice(0, -1)
                        .reduce((obj, key) => obj[key], root);
                    const fn = parent[path[path.length - 1]];
                    return fn.apply(parent, args);
                });
            },
        });

    globalThis.octokit = make([]);
})(globalThis);
"#;

const SEED_DOC: &str = r#"import { Octokit } from "@octokit/rest";
const octokit = new Octokit();

// octokit is authenticated as the current user. For "you" / "your" / "my",
// get the identity from GitHub — never guess from an email. For example:
//   const me = (await octokit.rest.users.getAuthenticated()).data.login; // your real username"#;

pub(crate) struct GitHub {
    auth: Arc<GitHubAuth>,
}

impl GitHub {
    pub(crate) fn new() -> Self {
        Self {
            auth: Arc::new(GitHubAuth {
                token: tokio::sync::Mutex::new(None),
            }),
        }
    }
}

impl Sdk for GitHub {
    fn name(&self) -> &'static str {
        "github"
    }

    fn imports(&self) -> &'static [(&'static str, &'static str)] {
        IMPORTS
    }

    fn docs(&self) -> Docs {
        Docs {
            seed: SEED_DOC,
            // Also documents `prompt`: the two "always provided" globals share
            // one bullet so the prompt reads naturally.
            system_prompt: Some(
                "`octokit` (an authenticated @octokit/rest client) and `prompt` (the user's \
                 latest message) are always provided. Do not redeclare or re-import them.",
            ),
            import_blurb: "`@octokit/rest` (GitHub API)",
            mcp_blurb: "`@octokit/rest` (authenticated GitHub API)",
        }
    }

    fn shim(&self) -> Option<&'static str> {
        Some(OCTOKIT_SHIM)
    }

    fn auth(&self) -> Option<Arc<dyn Auth>> {
        Some(self.auth.clone())
    }
}

/// Auth state lives behind its own mutex so the rest of the SDK (imports,
/// docs, shim) is lock-free; only requests GitHub actually claims touch it.
struct GitHubAuth {
    token: tokio::sync::Mutex<Option<String>>,
}

/// The well-known git smart-HTTP endpoints. These live on the web host
/// (`github.com`) rather than the API host, and are the only web-host paths
/// that receive credentials — a bare web fetch to `github.com` never does.
fn is_git_smart_http(path: &str) -> bool {
    path.ends_with("/info/refs")
        || path.ends_with("/git-upload-pack")
        || path.ends_with("/git-receive-pack")
}

impl GitHubAuth {
    /// The cached token, fetching it from `gh auth token` on first use.
    async fn token(&self) -> Result<String, JsErrorBox> {
        let mut guard = self.token.lock().await;
        if let Some(token) = guard.as_ref() {
            return Ok(token.clone());
        }

        let output = Command::new("gh")
            .arg("auth")
            .arg("token")
            .output()
            .await
            .map_err(JsErrorBox::from_err)?;

        let token = token_from_output(output.status.success(), &output.stdout, &output.stderr)
            .map_err(JsErrorBox::generic)?;

        *guard = Some(token.clone());
        Ok(token)
    }
}

impl Auth for GitHubAuth {
    fn claims(&self, host: &str, path: &str) -> bool {
        host == API_HOST || (host == WEB_HOST && is_git_smart_http(path))
    }

    fn apply<'a>(
        &'a self,
        request: &'a mut http::Request<deno_fetch::ReqBody>,
    ) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>> {
        Box::pin(async move {
            let token = self.token().await?;

            // Git smart-HTTP presents the token as HTTP Basic (token as the
            // password); the API takes Bearer.
            let header = if request.uri().host() == Some(WEB_HOST) {
                use base64::Engine;
                let creds = base64::engine::general_purpose::STANDARD
                    .encode(format!("x-access-token:{}", token.trim()));
                format!("Basic {creds}")
            } else {
                format!("Bearer {}", token.trim())
            };

            request.headers_mut().insert(
                http::header::AUTHORIZATION,
                header
                    .parse::<http::HeaderValue>()
                    .map_err(|e| JsErrorBox::generic(e.to_string()))?,
            );
            Ok(())
        })
    }
}

/// Decide, from the outcome of `gh auth token`, whether we have a usable token.
///
/// A non-zero exit means `gh` failed (most often: not installed or not logged
/// in) — surface the trimmed stderr so the real cause is visible rather than
/// letting an empty token turn into silent 401s later. A zero exit with empty
/// (whitespace-only) stdout is also an error: no token was produced.
fn token_from_output(status_ok: bool, stdout: &[u8], stderr: &[u8]) -> Result<String, String> {
    if !status_ok {
        let stderr = String::from_utf8_lossy(stderr);
        return Err(format!(
            "gh auth token failed: {} (is the GitHub CLI installed and authenticated? run `gh auth login`)",
            stderr.trim()
        ));
    }

    let token = String::from_utf8_lossy(stdout);
    let token = token.trim();
    if token.is_empty() {
        return Err(
            "gh auth token produced no token (is the GitHub CLI authenticated? run `gh auth login`)"
                .to_string(),
        );
    }

    Ok(token.to_string())
}

#[cfg(test)]
mod tests {
    use super::{GitHub, token_from_output};
    use crate::sdk::Sdk;

    #[test]
    fn success_with_token_is_trimmed() {
        let token = token_from_output(true, b"  ghp_abc123\n", b"").unwrap();
        assert_eq!(token, "ghp_abc123");
    }

    #[test]
    fn success_with_empty_stdout_is_error() {
        let err = token_from_output(true, b"   \n", b"").unwrap_err();
        assert!(err.contains("no token"), "unexpected message: {err}");
    }

    #[test]
    fn failure_includes_trimmed_stderr() {
        let err = token_from_output(false, b"", b"  not logged in\n").unwrap_err();
        assert!(
            err.contains("gh auth token failed"),
            "unexpected message: {err}"
        );
        assert!(err.contains("not logged in"), "stderr not surfaced: {err}");
        // stderr should be trimmed of surrounding whitespace.
        assert!(
            !err.contains("  not logged in"),
            "stderr not trimmed: {err}"
        );
    }

    /// The auth facet claims the API host unconditionally, the web host only on
    /// the git smart-HTTP endpoints, and nothing else.
    #[test]
    fn claims_api_and_git_endpoints_only() {
        let auth = GitHub::new().auth().expect("github has an auth facet");
        assert!(auth.claims("api.github.com", "/repos/a/b/issues"));
        assert!(auth.claims("github.com", "/a/b.git/info/refs"));
        assert!(auth.claims("github.com", "/a/b.git/git-upload-pack"));
        assert!(auth.claims("github.com", "/a/b.git/git-receive-pack"));
        // A bare web fetch never gets credentials.
        assert!(!auth.claims("github.com", "/a/b"));
        // Unrelated hosts are never claimed.
        assert!(!auth.claims("example.com", "/info/refs"));
        assert!(!auth.claims("gist.github.com", "/a/b"));
    }
}
