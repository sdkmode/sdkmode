//! The GitHub SDK: an authenticated Octokit client in the sandbox, with
//! credentials brokered host-side. Owns everything GitHub-shaped: the
//! `@octokit/rest` pin, the lazy `globalThis.octokit` shim, the prompt docs,
//! and the [`Auth`] facet that injects a token into API requests — including
//! git smart-HTTP requests on the web host, which authenticate with Basic
//! rather than the API's Bearer scheme.
//!
//! The token has two sources, chosen host-side so the guest is identical
//! either way:
//!   - **User mode (default):** `gh auth token` — acts as the logged-in user.
//!     Zero setup; ideal for local dev.
//!   - **App mode:** a GitHub App, when `SDKMODE_GITHUB_APP_ID`,
//!     `SDKMODE_GITHUB_APP_KEY` (a PEM path), and
//!     `SDKMODE_GITHUB_APP_INSTALLATION_ID` are all set. The facet mints a
//!     short-lived RS256 JWT from the private key, exchanges it for a 1-hour
//!     installation token, and caches that. Acts as `app-slug[bot]` — a
//!     scoped, revocable, non-personal identity, which is what a public bot
//!     should use. The private key never enters the sandbox.

use deno_core::op2;
use deno_error::JsErrorBox;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::process::Command;

use super::{Auth, Docs, Sdk};

/// Env vars that switch the GitHub token from `gh` (user) to a GitHub App.
const APP_ID_ENV: &str = "SDKMODE_GITHUB_APP_ID";
const APP_KEY_ENV: &str = "SDKMODE_GITHUB_APP_KEY";
const APP_INSTALLATION_ENV: &str = "SDKMODE_GITHUB_APP_INSTALLATION_ID";

/// Refresh margin under the installation token's 1-hour validity: cache for
/// 55 minutes so a token is never used within 5 minutes of expiry.
const APP_TOKEN_TTL: Duration = Duration::from_secs(55 * 60);

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

    // Host-resolved identity: works in both auth modes, where
    // `octokit.rest.users.getAuthenticated()` does not (an App installation
    // token gets 403 on `GET /user`). Returns the user login in `gh` mode, or
    // `sdkmode[bot]` in App mode. Lazy — no network until called.
    const actorOp = globalThis.__sdkmode_githubActorOp;
    globalThis.githubActor = () => actorOp();
})(globalThis);
"#;

/// Captures the actor op before bootstrap removes `Deno.core`, so the shim can
/// hold a reference to it (mirrors the browser SDK's endpoint op).
const PRE_BOOTSTRAP: &str =
    "globalThis.__sdkmode_githubActorOp = Deno.core.ops.op_sdkmode_github_actor;";

const SEED_DOC: &str = r#"import { Octokit } from "@octokit/rest";
const octokit = new Octokit();

// octokit is authenticated — as the logged-in user, or as this tool's bot
// ("sdkmode[bot]") when running as a GitHub App. For "you" / "your" / "my",
// get your identity with githubActor() — it works in both modes; never guess
// from an email:
//   const me = await githubActor(); // e.g. "he1d1" or "sdkmode[bot]""#;

pub(crate) struct GitHub {
    auth: Arc<GitHubAuth>,
}

impl GitHub {
    pub(crate) fn new() -> Self {
        Self {
            auth: Arc::new(GitHubAuth {
                app: resolve_app_config(
                    env_nonempty(APP_ID_ENV),
                    env_nonempty(APP_KEY_ENV),
                    env_nonempty(APP_INSTALLATION_ENV),
                ),
                token: tokio::sync::Mutex::new(None),
            }),
        }
    }
}

/// An environment variable's value if set and non-blank.
fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
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
                "`octokit` (an authenticated @octokit/rest client), `githubActor()` (an \
                 async fn returning your GitHub login — the user in `gh` mode, or \
                 `sdkmode[bot]` as a GitHub App), and `prompt` (the user's latest message) \
                 are always provided. Do not redeclare or re-import them. Use \
                 `githubActor()` for identity, not a guess.",
            ),
            import_blurb: "`@octokit/rest` (GitHub API)",
            mcp_blurb: "`@octokit/rest` (authenticated GitHub API)",
        }
    }

    fn pre_bootstrap_script(&self) -> Option<&'static str> {
        Some(PRE_BOOTSTRAP)
    }

    fn shim(&self) -> Option<&'static str> {
        Some(OCTOKIT_SHIM)
    }

    fn extension(&self) -> Option<deno_core::Extension> {
        Some(sdkmode_github::init())
    }

    fn auth(&self) -> Option<Arc<dyn Auth>> {
        Some(self.auth.clone())
    }
}

/// A GitHub App's host-side configuration (see the module docs). The private
/// key is referenced by path and read only when a JWT is minted, so it can
/// appear after startup and never lives in memory longer than a signing call.
struct AppConfig {
    app_id: String,
    key_path: String,
    installation_id: String,
}

/// A cached token and when it stops being usable. `expires_at` is `None` for
/// the `gh` user token (stable for the process) and `Some` for an App
/// installation token (refreshed before its hour is up).
struct CachedToken {
    value: String,
    expires_at: Option<Instant>,
}

/// Auth state lives behind its own mutex so the rest of the SDK (imports,
/// docs, shim) is lock-free; only requests GitHub actually claims touch it.
/// `app` is resolved once at construction: `None` = user mode; `Some(Ok)` =
/// App mode; `Some(Err)` = App partially configured (surface at first use).
struct GitHubAuth {
    app: Option<Result<AppConfig, String>>,
    token: tokio::sync::Mutex<Option<CachedToken>>,
}

/// Decide the token source from the three App env vars: all set → App mode;
/// none set → user mode (`gh`); some set → a configuration error, so a
/// half-configured App fails loudly instead of silently acting as the wrong
/// (user) identity.
fn resolve_app_config(
    app_id: Option<String>,
    key_path: Option<String>,
    installation_id: Option<String>,
) -> Option<Result<AppConfig, String>> {
    match (app_id, key_path, installation_id) {
        (None, None, None) => None,
        (Some(app_id), Some(key_path), Some(installation_id)) => Some(Ok(AppConfig {
            app_id,
            key_path,
            installation_id,
        })),
        _ => Some(Err(format!(
            "incomplete GitHub App config: set all of {APP_ID_ENV}, {APP_KEY_ENV} \
             (a PEM key path), and {APP_INSTALLATION_ENV} — or none of them to use \
             `gh auth token`"
        ))),
    }
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
    /// The cached token, resolving a fresh one when the cache is empty or an
    /// App installation token has expired. The value is a user token (`gh`) or
    /// an App installation token depending on configuration; both present to
    /// GitHub the same way (see [`Auth::apply`]).
    async fn token(&self) -> Result<String, JsErrorBox> {
        let mut guard = self.token.lock().await;
        if let Some(cached) = guard.as_ref()
            && cached.expires_at.is_none_or(|expiry| Instant::now() < expiry)
        {
            return Ok(cached.value.clone());
        }

        let (value, expires_at) = match &self.app {
            None => (self.gh_user_token().await?, None),
            Some(Ok(config)) => app_installation_token(config)
                .await
                .map(|token| (token, Some(Instant::now() + APP_TOKEN_TTL)))
                .map_err(JsErrorBox::generic)?,
            Some(Err(error)) => return Err(JsErrorBox::generic(error.clone())),
        };

        *guard = Some(CachedToken {
            value: value.clone(),
            expires_at,
        });
        Ok(value)
    }

    /// User mode: the token from `gh auth token`.
    async fn gh_user_token(&self) -> Result<String, JsErrorBox> {
        let output = Command::new("gh")
            .arg("auth")
            .arg("token")
            .output()
            .await
            .map_err(JsErrorBox::from_err)?;

        token_from_output(output.status.success(), &output.stdout, &output.stderr)
            .map_err(JsErrorBox::generic)
    }
}

/// App mode: mint an RS256 JWT from the app's private key and exchange it for
/// a scoped installation access token. The exchange is a plain authenticated
/// POST, done via `curl` to match the existing shell-out-to-a-known-CLI
/// pattern (`gh`) rather than pulling in an HTTP client.
async fn app_installation_token(config: &AppConfig) -> Result<String, String> {
    let jwt = mint_app_jwt(&config.app_id, &config.key_path)?;
    let url = format!(
        "https://api.github.com/app/installations/{}/access_tokens",
        config.installation_id
    );

    let output = Command::new("curl")
        .args(["-sS", "-X", "POST"])
        .args(["-H", &format!("Authorization: Bearer {jwt}")])
        .args(["-H", "Accept: application/vnd.github+json"])
        .args(["-H", "X-GitHub-Api-Version: 2022-11-28"])
        // Append the HTTP status on its own line so failures are legible.
        .args(["-w", "\n%{http_code}"])
        .arg(&url)
        .output()
        .await
        .map_err(|error| format!("failed to run curl for the GitHub App token exchange: {error}"))?;

    if !output.status.success() {
        return Err(format!(
            "curl failed during the GitHub App token exchange: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    installation_token_from_response(&String::from_utf8_lossy(&output.stdout))
}

/// Sign a GitHub App JWT (RS256) valid for ~9 minutes. `iat` is backdated 60s
/// so GitHub does not reject it for clock skew; `iss` is the app id.
fn mint_app_jwt(app_id: &str, key_path: &str) -> Result<String, String> {
    let pem = std::fs::read(key_path)
        .map_err(|error| format!("cannot read GitHub App key at {key_path}: {error}"))?;
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(&pem).map_err(|error| {
        format!("GitHub App key at {key_path} is not a valid RSA PEM: {error}")
    })?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_secs();
    let claims = app_jwt_claims(app_id, now);
    let header = jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256);

    jsonwebtoken::encode(&header, &claims, &key)
        .map_err(|error| format!("failed to sign the GitHub App JWT: {error}"))
}

/// The JWT claims: issued 60s ago (skew tolerance), expiring in 9 minutes
/// (under GitHub's 10-minute ceiling), issued by the app id.
fn app_jwt_claims(app_id: &str, now_secs: u64) -> serde_json::Value {
    serde_json::json!({
        "iat": now_secs.saturating_sub(60),
        "exp": now_secs + 9 * 60,
        "iss": app_id,
    })
}

/// Parse the installation-token exchange response — `<json>\n<http_code>` from
/// curl's `-w`. A 201 (or 200) yields the token; anything else surfaces the
/// status and body so a misconfigured app or installation is diagnosable.
fn installation_token_from_response(raw: &str) -> Result<String, String> {
    let (body, code) = raw.trim_end().rsplit_once('\n').unwrap_or((raw, ""));
    let (body, code) = (body.trim(), code.trim());

    if code != "201" && code != "200" {
        return Err(format!(
            "GitHub App token exchange failed (HTTP {code}): {body}"
        ));
    }

    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("unparseable token response: {error}"))?;
    value
        .get("token")
        .and_then(|token| token.as_str())
        .map(str::to_string)
        .ok_or_else(|| format!("no token in the exchange response: {body}"))
}

/// The resolved GitHub actor login, cached for the process. Independent of the
/// per-request auth cache: identity is derived from the same process-global
/// config (env vars) and never changes within a run.
static ACTOR: OnceLock<tokio::sync::Mutex<Option<String>>> = OnceLock::new();

/// The GitHub login the guest acts as: the `gh` user in user mode, or
/// `slug[bot]` in App mode. Resolved once (lazily) and cached.
async fn resolve_actor() -> Result<String, String> {
    let cell = ACTOR.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut guard = cell.lock().await;
    if let Some(actor) = guard.as_ref() {
        return Ok(actor.clone());
    }

    let actor = match resolve_app_config(
        env_nonempty(APP_ID_ENV),
        env_nonempty(APP_KEY_ENV),
        env_nonempty(APP_INSTALLATION_ENV),
    ) {
        None => gh_user_login().await?,
        Some(Ok(config)) => app_bot_login(&config).await?,
        Some(Err(error)) => return Err(error),
    };

    *guard = Some(actor.clone());
    Ok(actor)
}

/// User mode: the login from `gh api user`.
async fn gh_user_login() -> Result<String, String> {
    let output = Command::new("gh")
        .args(["api", "user", "--jq", ".login"])
        .output()
        .await
        .map_err(|error| format!("failed to run gh: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "gh api user failed: {} (is the GitHub CLI authenticated? run `gh auth login`)",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let login = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if login.is_empty() {
        return Err("gh api user returned an empty login".to_string());
    }
    Ok(login)
}

/// App mode: the bot login `slug[bot]`, from `GET /app` authenticated with a
/// freshly minted app JWT.
async fn app_bot_login(config: &AppConfig) -> Result<String, String> {
    let jwt = mint_app_jwt(&config.app_id, &config.key_path)?;
    let output = Command::new("curl")
        .args(["-sS"])
        .args(["-H", &format!("Authorization: Bearer {jwt}")])
        .args(["-H", "Accept: application/vnd.github+json"])
        .args(["-H", "X-GitHub-Api-Version: 2022-11-28"])
        .args(["-w", "\n%{http_code}"])
        .arg("https://api.github.com/app")
        .output()
        .await
        .map_err(|error| format!("failed to run curl for GET /app: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "curl failed during GET /app: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    app_bot_login_from_response(&String::from_utf8_lossy(&output.stdout))
}

/// Parse `slug[bot]` out of a `GET /app` response (`<json>\n<http_code>`).
fn app_bot_login_from_response(raw: &str) -> Result<String, String> {
    let (body, code) = raw.trim_end().rsplit_once('\n').unwrap_or((raw, ""));
    let (body, code) = (body.trim(), code.trim());
    if code != "200" {
        return Err(format!("GET /app failed (HTTP {code}): {body}"));
    }
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|error| format!("unparseable /app response: {error}"))?;
    value
        .get("slug")
        .and_then(|slug| slug.as_str())
        .map(|slug| format!("{slug}[bot]"))
        .ok_or_else(|| format!("no slug in the /app response: {body}"))
}

/// Resolve the GitHub login the guest acts as. Exposed to guest JS as
/// `githubActor()`; lazy, so it does no I/O until the guest asks who it is.
#[op2]
#[string]
pub async fn op_sdkmode_github_actor() -> Result<String, JsErrorBox> {
    resolve_actor().await.map_err(JsErrorBox::generic)
}

deno_core::extension!(sdkmode_github, ops = [op_sdkmode_github_actor]);

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
    use super::{
        GitHub, app_bot_login_from_response, app_jwt_claims, installation_token_from_response,
        resolve_app_config, token_from_output,
    };
    use crate::sdk::Sdk;

    #[test]
    fn app_bot_login_is_slug_suffixed_and_errors_are_surfaced() {
        let login = app_bot_login_from_response("{\"slug\":\"sdkmode\",\"id\":42}\n200").unwrap();
        assert_eq!(login, "sdkmode[bot]");

        let err = app_bot_login_from_response("{\"message\":\"Bad credentials\"}\n401").unwrap_err();
        assert!(err.contains("HTTP 401"), "unexpected: {err}");
    }

    #[test]
    fn app_config_needs_all_three_vars_or_none() {
        let some = |s: &str| Some(s.to_string());
        // None set: user mode.
        assert!(resolve_app_config(None, None, None).is_none());
        // All set: App mode.
        let ok = resolve_app_config(some("123"), some("/k.pem"), some("456"));
        assert!(matches!(ok, Some(Ok(_))));
        // Partial: a configuration error, not a silent fallback to `gh`.
        let partial = resolve_app_config(some("123"), None, some("456"));
        assert!(matches!(partial, Some(Err(_))));
    }

    #[test]
    fn jwt_claims_backdate_iat_and_bound_exp() {
        let claims = app_jwt_claims("42", 1_000_000);
        assert_eq!(claims["iat"], 999_940); // now - 60
        assert_eq!(claims["exp"], 1_000_540); // now + 9 min
        assert_eq!(claims["iss"], "42");
    }

    #[test]
    fn installation_response_parses_success_and_reports_failure() {
        let ok = installation_token_from_response(
            "{\"token\":\"ghs_abc\",\"expires_at\":\"2026-01-01T00:00:00Z\"}\n201",
        )
        .unwrap();
        assert_eq!(ok, "ghs_abc");

        let err = installation_token_from_response("{\"message\":\"Bad credentials\"}\n401")
            .unwrap_err();
        assert!(err.contains("HTTP 401"), "unexpected: {err}");
        assert!(err.contains("Bad credentials"), "body not surfaced: {err}");
    }

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
