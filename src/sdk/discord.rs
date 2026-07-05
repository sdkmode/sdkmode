//! The Discord SDK: the discordeno REST client in the sandbox as `discord`,
//! with the bot token brokered host-side. REST-only by design — the gateway
//! (event stream) is a WebSocket, which bypasses the fetch broker and would
//! need the raw token inside the sandbox to IDENTIFY; it belongs to the host
//! (a Discord front-end), not the guest.
//!
//! discordeno is Deno-native, so it loads cleanly over esm.sh where
//! `@discordjs/rest` (which bundles undici) does not. It decodes a bot id
//! from the token at construction, so the shim hands it a shape-valid
//! placeholder; the broker strips that and injects the real `Authorization`,
//! so the guest never sees the token.
//!
//! Policy: the bot may do anything its Discord permissions allow — chaos is
//! accepted — except edit its own account or application. The bot's profile
//! is the demo's marketing surface (it links to the repo and the server), so
//! `PATCH /users/@me` and `PATCH /applications/@me` are rejected in the auth
//! facet, before credentials are ever attached.

use deno_error::JsErrorBox;
use std::pin::Pin;
use std::sync::Arc;

use super::{Auth, Docs, Sdk};

const API_HOST: &str = "discord.com";

/// The host-side source of the bot token. Guest code cannot read env vars
/// (see the env shim in [`crate::sandbox`]), so the token never enters the
/// sandbox.
const TOKEN_ENV: &str = "SDKMODE_DISCORD_TOKEN";

/// Exposes `globalThis.discord`: the discordeno REST manager, imported and
/// constructed lazily on first use (like the octokit shim), so building a
/// session needs no network. A lazy Proxy forwards every access to the real
/// manager, so `await discord.get(path)` / `discord.post(path, { body })`
/// call straight through to discordeno.
const DISCORD_SHIM: &str = r#"
((globalThis) => {
    let manager = null;
    const ready = async () => {
        if (manager) return manager;
        const { createRestManager } = await import("@discordeno/rest");
        // The bot token is injected by the host fetch broker; guest code never
        // sees it. discordeno decodes a bot id from the token at construction,
        // so hand it a shape-valid placeholder (first segment is a base64
        // snowflake). The broker strips this and sets the real Authorization.
        const placeholder =
            btoa("100000000000000000") + ".Gplaceh.olderolderolderolderolderol";
        manager = createRestManager({ token: placeholder });
        return manager;
    };
    globalThis.discord = new Proxy(function () {}, {
        get(_target, prop) {
            if (prop === "then" || typeof prop === "symbol") return undefined;
            // Resolve the manager (once) and forward: call methods, read props.
            return (...args) =>
                ready().then((m) => {
                    const value = m[prop];
                    return typeof value === "function" ? value.apply(m, args) : value;
                });
        },
    });
})(globalThis);
"#;

const SEED_DOC: &str = r#"// discord: the Discord REST API (discordeno), authenticated as this server's
// bot — the token is injected outside the sandbox; you never see it. No import.
//   const me = await discord.get("/users/@me");   // the bot's identity
//   await discord.post(`/channels/${channelId}/messages`, { body: { content: "hi" } });
//   await discord.delete(`/channels/${cid}/messages/${mid}`); // any route/method
// Requests that edit the bot's own account or application are rejected.
let discord;"#;

pub(crate) struct Discord {
    auth: Arc<DiscordAuth>,
}

impl Discord {
    pub(crate) fn new() -> Self {
        Self {
            auth: Arc::new(DiscordAuth),
        }
    }
}

impl Sdk for Discord {
    fn name(&self) -> &'static str {
        "discord"
    }

    fn imports(&self) -> &'static [(&'static str, &'static str)] {
        // discordeno only. `@discordjs/rest` bundles undici and times out over
        // esm.sh; discordeno is Deno-native and loads cleanly.
        &[(
            "@discordeno/rest",
            "https://esm.sh/@discordeno/rest@21.0.0",
        )]
    }

    fn docs(&self) -> Docs {
        Docs {
            seed: SEED_DOC,
            system_prompt: Some(
                "`discord` (the Discord REST API via discordeno, as the server's bot) is \
                 always provided — no import needed. Call `discord.get`/`post`/`put`/\
                 `patch`/`delete(path, { body })` on any REST route, e.g. \
                 `discord.post(`/channels/${id}/messages`, { body: { content: text } })`. \
                 The bot token is injected outside the sandbox; you never see it. Requests \
                 editing the bot's own account or application are rejected.",
            ),
            // discordeno is imported only by the shim, not by the model, so it
            // is left out of the model-facing allowlist sentence (empty blurbs
            // are skipped by `oxford_join`).
            import_blurb: "",
            mcp_blurb: "",
        }
    }

    fn shim(&self) -> Option<&'static str> {
        Some(DISCORD_SHIM)
    }

    fn auth(&self) -> Option<Arc<dyn Auth>> {
        Some(self.auth.clone())
    }
}

/// Stateless facet: the token is read from the environment per request (a
/// getenv, effectively free) so a token exported after startup still works.
struct DiscordAuth;

/// Whether `path` names the bot's own account or application — the two
/// resources the guest may read but never modify. Matches any API version
/// (`/api/users/@me`, `/api/v10/users/@me`, ...); subresources (e.g. guild
/// membership under a different route) are not covered and remain fair game.
fn is_self_account_path(path: &str) -> bool {
    let mut parts = path.split('/').filter(|part| !part.is_empty());
    if parts.next() != Some("api") {
        return false;
    }
    let mut next = parts.next();
    if let Some(segment) = next
        && segment.len() >= 2
        && segment.starts_with('v')
        && segment[1..].bytes().all(|b| b.is_ascii_digit())
    {
        next = parts.next();
    }
    matches!(
        (next, parts.next(), parts.next()),
        (Some("users"), Some("@me"), None) | (Some("applications"), Some("@me"), None)
    )
}

impl Auth for DiscordAuth {
    fn claims(&self, host: &str, path: &str) -> bool {
        host == API_HOST && path.starts_with("/api/")
    }

    fn apply<'a>(
        &'a self,
        request: &'a mut http::Request<deno_fetch::ReqBody>,
    ) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>> {
        Box::pin(async move {
            if request.method() != http::Method::GET
                && request.method() != http::Method::HEAD
                && is_self_account_path(request.uri().path())
            {
                return Err(JsErrorBox::generic(
                    "rejected: the bot may not edit its own account or application",
                ));
            }

            let token = std::env::var(TOKEN_ENV).ok().filter(|t| !t.trim().is_empty());
            let Some(token) = token else {
                return Err(JsErrorBox::generic(format!(
                    "no Discord bot token: set {TOKEN_ENV} (a bot token from the Discord \
                     developer portal) in the host environment"
                )));
            };

            request.headers_mut().insert(
                http::header::AUTHORIZATION,
                format!("Bot {}", token.trim())
                    .parse::<http::HeaderValue>()
                    .map_err(|e| JsErrorBox::generic(e.to_string()))?,
            );
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{Discord, is_self_account_path};
    use crate::sdk::Sdk;

    /// The facet claims the API paths on the API host and nothing else — the
    /// CDN and bare web pages are public and stay unclaimed.
    #[test]
    fn claims_api_paths_only() {
        let auth = Discord::new().auth().expect("discord has an auth facet");
        assert!(auth.claims("discord.com", "/api/v10/users/@me"));
        assert!(auth.claims("discord.com", "/api/v10/channels/1/messages"));
        assert!(!auth.claims("discord.com", "/channels/@me"));
        assert!(!auth.claims("cdn.discordapp.com", "/avatars/x/y.png"));
        assert!(!auth.claims("example.com", "/api/v10/users/@me"));
    }

    /// The self-account matcher covers both protected resources across API
    /// versions, and nothing broader.
    #[test]
    fn self_account_paths_are_recognized_precisely() {
        assert!(is_self_account_path("/api/v10/users/@me"));
        assert!(is_self_account_path("/api/v9/applications/@me"));
        assert!(is_self_account_path("/api/users/@me"));
        assert!(is_self_account_path("/api/v10/users/@me/"));

        // Reads of other users, and @me subresources on other routes, are fine.
        assert!(!is_self_account_path("/api/v10/users/1234"));
        assert!(!is_self_account_path("/api/v10/users/@me/guilds"));
        assert!(!is_self_account_path("/api/v10/guilds/1/members/@me"));
        assert!(!is_self_account_path("/api/v10/channels/1/messages"));
        assert!(!is_self_account_path("/users/@me"));
    }
}
