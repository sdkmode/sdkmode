//! Host-side Discord gateway: the `sdkmode discord` front-end.
//!
//! The gateway is a WebSocket the guest can never own — it authenticates by
//! putting the raw bot token in the IDENTIFY payload, and the sandbox never
//! sees the token (that is the whole brokering premise). So the host holds the
//! socket: it connects, IDENTIFYs, heartbeats, and receives `MESSAGE_CREATE`
//! dispatches. What to *do* with a message is the guest's business.
//!
//! Two layers:
//!   - **Connection** ([`gateway_loop`]): connect → HELLO → heartbeat →
//!     IDENTIFY → READY → forward each message event onto a channel,
//!     reconnecting on drops. If the bot lacks the privileged Message Content
//!     intent (IDENTIFY closes with 4014) it says so and reconnects without it
//!     — where `content` arrives empty, the same redaction seen over REST.
//!   - **Turns** ([`run`]): a single agent — [`Session`] + [`Transcript`] +
//!     [`Llm`] — drains that channel one event at a time. Each event is first
//!     passed to the guest's `onDiscordEvent(event)` policy (a plain host-eval,
//!     no model call): it returns a prompt to escalate into a full agent turn,
//!     or nothing to ignore. The default policy ignores bots (so the agent
//!     never answers itself) and escalates every human message; the agent can
//!     redefine it at any time, since it is just a global function. On
//!     escalation the agent takes a turn and replies with `discord.send(...)`.

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;

/// The env var whose presence enables the Discord source (see
/// [`token`]). The REPL runs the gateway alongside the terminal whenever it
/// is set — Discord is not a separate mode.
pub(crate) const TOKEN_ENV: &str = "SDKMODE_DISCORD_TOKEN";

const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// Gateway intents we request. `GUILD_MESSAGES` (server channel messages) and
/// `DIRECT_MESSAGES` (DMs to the bot) are unprivileged; `MESSAGE_CONTENT` (the
/// `content` field is populated for guild messages) is privileged and must be
/// enabled in the developer portal. DM content is never gated by
/// `MESSAGE_CONTENT`, so DMs carry text regardless.
const GUILD_MESSAGES: u64 = 1 << 9;
const DIRECT_MESSAGES: u64 = 1 << 12;
const MESSAGE_CONTENT: u64 = 1 << 15;

/// The default `onDiscordEvent` policy, installed once at startup by the REPL
/// when Discord is enabled. It runs with the message event as its argument and
/// returns a prompt string to escalate, or null to ignore. Ignoring bot
/// authors is load-bearing: without it the agent's own `discord.send` replies
/// would trigger fresh turns, looping forever. The agent may replace this
/// global whenever it likes.
pub(crate) const DEFAULT_POLICY: &str = r#"
globalThis.onDiscordEvent ??= (event) => {
    // Never react to bots — including this bot — so replies don't loop.
    if (event.author && event.author.bot) return null;
    const channelId = event.channel_id;
    const author = (event.author && event.author.username) || "someone";
    const content = event.content || "";
    // Escalate every human message into an agent turn. The prompt tells you
    // how to reply; `lastDiscordEvent` holds the full event for context.
    return `A Discord message arrived in channel ${channelId} from ${author}: `
        + `"${content}". You are a Discord bot. If a reply is warranted, send it `
        + `with discord.send("${channelId}", yourText). The full event is in the `
        + `lastDiscordEvent variable. You can also change your own event policy by `
        + `reassigning globalThis.onDiscordEvent(event) => promptStringOrNull.`;
};
"#;

/// The bot token from the environment, if Discord is enabled. `None` means the
/// REPL runs terminal-only.
pub(crate) fn token() -> Option<String> {
    std::env::var(TOKEN_ENV)
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

/// Stream `MESSAGE_CREATE` data objects onto `events_tx`, forever, reconnecting
/// as needed. The REPL runs this as a background source feeding its inbox; the
/// agent (which owns the runtime) evaluates the `onDiscordEvent` policy and
/// takes turns. Requests the Message Content intent, dropping it (with a
/// warning) if the bot is not approved for it.
pub(crate) async fn run_into(token: String, events_tx: mpsc::UnboundedSender<serde_json::Value>) {
    let mut want_content = true;
    loop {
        match connect_once(&token, want_content, &events_tx).await {
            Ended::DisallowedIntent if want_content => {
                eprintln!(
                    "\n⚠  The bot lacks the Message Content intent, so it cannot read message \
                     text.\n   Enable it: Discord developer portal → your app → Bot → \
                     Privileged Gateway Intents → Message Content Intent.\n   Reconnecting \
                     without it — message `content` will be empty until you enable it.\n"
                );
                want_content = false;
            }
            Ended::DisallowedIntent | Ended::Reconnect => {
                eprintln!("gateway disconnected; reconnecting in 3s…");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
}

/// Why a single gateway connection ended.
enum Ended {
    /// The socket closed or errored; reconnect.
    Reconnect,
    /// IDENTIFY was refused for requesting the Message Content intent (close
    /// code 4014); reconnect without it.
    DisallowedIntent,
}

/// One connection lifecycle: HELLO → heartbeat + IDENTIFY → dispatch loop,
/// forwarding message events onto `events_tx` until the socket closes.
async fn connect_once(
    token: &str,
    want_content: bool,
    events_tx: &mpsc::UnboundedSender<serde_json::Value>,
) -> Ended {
    let (stream, _) = match tokio_tungstenite::connect_async(GATEWAY_URL).await {
        Ok(ok) => ok,
        Err(error) => {
            eprintln!("gateway connect error: {error}");
            return Ended::Reconnect;
        }
    };
    let (mut sink, mut source) = stream.split();

    // Outgoing frames flow through a channel so the heartbeat timer and the
    // dispatch loop can both send without sharing the sink.
    let (out, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(message) = out_rx.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });

    // The last sequence number seen, echoed in heartbeats (-1 → JSON null).
    let last_seq = Arc::new(AtomicI64::new(-1));
    let mut heartbeat: Option<tokio::task::JoinHandle<()>> = None;

    let ended = loop {
        let Some(frame) = source.next().await else {
            break Ended::Reconnect;
        };
        let message = match frame {
            Ok(message) => message,
            Err(_) => break Ended::Reconnect,
        };

        match message {
            Message::Text(text) => {
                match handle_payload(&text, &last_seq, &out, token, want_content, events_tx) {
                    PayloadAction::None => {}
                    PayloadAction::StartHeartbeat(interval_ms) => {
                        if heartbeat.is_none() {
                            heartbeat =
                                Some(spawn_heartbeat(interval_ms, last_seq.clone(), out.clone()));
                        }
                    }
                    PayloadAction::Reconnect => break Ended::Reconnect,
                }
            }
            Message::Close(frame) => break close_reason(frame),
            Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {}
        }
    };

    if let Some(handle) = heartbeat {
        handle.abort();
    }
    drop(out);
    writer.abort();
    ended
}

/// What the dispatch loop should do after a text payload.
enum PayloadAction {
    None,
    StartHeartbeat(u64),
    Reconnect,
}

/// Handle one gateway text payload: update the sequence, react to control ops
/// (HELLO, heartbeat request, reconnect/invalid-session), send IDENTIFY after
/// HELLO, and forward message dispatches onto `events_tx`.
fn handle_payload(
    text: &str,
    last_seq: &Arc<AtomicI64>,
    out: &mpsc::UnboundedSender<Message>,
    token: &str,
    want_content: bool,
    events_tx: &mpsc::UnboundedSender<serde_json::Value>,
) -> PayloadAction {
    let Ok(payload) = serde_json::from_str::<serde_json::Value>(text) else {
        return PayloadAction::None;
    };

    if let Some(seq) = payload.get("s").and_then(serde_json::Value::as_i64) {
        last_seq.store(seq, Ordering::SeqCst);
    }

    match payload.get("op").and_then(serde_json::Value::as_u64) {
        // HELLO: start heartbeating, then IDENTIFY.
        Some(10) => {
            let interval = payload
                .get("d")
                .and_then(|d| d.get("heartbeat_interval"))
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(41_250);
            let _ = out.send(identify(token, want_content));
            PayloadAction::StartHeartbeat(interval)
        }
        // Heartbeat request: reply immediately.
        Some(1) => {
            let _ = out.send(heartbeat_frame(last_seq));
            PayloadAction::None
        }
        // Heartbeat ACK.
        Some(11) => PayloadAction::None,
        // Reconnect / Invalid Session: drop and re-handshake.
        Some(7) | Some(9) => PayloadAction::Reconnect,
        // Dispatch.
        Some(0) => {
            on_dispatch(&payload, events_tx);
            PayloadAction::None
        }
        _ => PayloadAction::None,
    }
}

/// React to a dispatch (`op` 0): log READY, forward each `MESSAGE_CREATE` data
/// object onto the events channel for the agent to consider.
fn on_dispatch(
    payload: &serde_json::Value,
    events_tx: &mpsc::UnboundedSender<serde_json::Value>,
) {
    let Some(kind) = payload.get("t").and_then(serde_json::Value::as_str) else {
        return;
    };
    match kind {
        "READY" => {
            let user = payload
                .get("d")
                .and_then(|d| d.get("user"))
                .and_then(|u| u.get("username"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("?");
            eprintln!("connected as {user}; listening for messages. Ctrl-C to exit.\n");
        }
        "MESSAGE_CREATE" => {
            if let Some(data) = payload.get("d") {
                let _ = events_tx.send(data.clone());
            }
        }
        _ => {}
    }
}

/// The IDENTIFY payload (op 2): token, intents, and minimal properties.
fn identify(token: &str, want_content: bool) -> Message {
    let intents =
        GUILD_MESSAGES | DIRECT_MESSAGES | if want_content { MESSAGE_CONTENT } else { 0 };
    let payload = serde_json::json!({
        "op": 2,
        "d": {
            "token": token,
            "intents": intents,
            "properties": {
                "os": std::env::consts::OS,
                "browser": "sdkmode",
                "device": "sdkmode",
            },
        },
    });
    Message::text(payload.to_string())
}

/// A heartbeat frame (op 1) echoing the last sequence number (null if none).
fn heartbeat_frame(last_seq: &AtomicI64) -> Message {
    let seq = last_seq.load(Ordering::SeqCst);
    let d = if seq < 0 {
        serde_json::Value::Null
    } else {
        serde_json::Value::from(seq)
    };
    Message::text(serde_json::json!({ "op": 1, "d": d }).to_string())
}

/// Spawn the heartbeat loop: send op 1 every `interval_ms` until aborted.
fn spawn_heartbeat(
    interval_ms: u64,
    last_seq: Arc<AtomicI64>,
    out: mpsc::UnboundedSender<Message>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms.max(1000)));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if out.send(heartbeat_frame(&last_seq)).is_err() {
                break;
            }
        }
    })
}

/// Map a close frame to an `Ended`: 4014 is the disallowed-intent case, which
/// we handle specially; everything else is a plain reconnect.
fn close_reason(frame: Option<CloseFrame>) -> Ended {
    let code: u16 = frame.map(|f| f.code.into()).unwrap_or(0);
    if code == 4014 {
        Ended::DisallowedIntent
    } else {
        if code != 0 {
            eprintln!("gateway closed with code {code}");
        }
        Ended::Reconnect
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_POLICY, DIRECT_MESSAGES, GUILD_MESSAGES, MESSAGE_CONTENT, identify};
    use tokio_tungstenite::tungstenite::Message;

    fn identify_json(want_content: bool) -> serde_json::Value {
        let Message::Text(text) = identify("tok", want_content) else {
            panic!("identify must be a text frame");
        };
        serde_json::from_str(text.as_str()).unwrap()
    }

    #[test]
    fn identify_requests_dms_and_content_intent_only_when_wanted() {
        let with = identify_json(true);
        assert_eq!(with["op"], 2);
        assert_eq!(with["d"]["token"], "tok");
        assert_eq!(
            with["d"]["intents"],
            GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT
        );

        // DMs are always requested; only the privileged content intent drops.
        let without = identify_json(false);
        assert_eq!(without["d"]["intents"], GUILD_MESSAGES | DIRECT_MESSAGES);
    }

    /// The default policy must ignore bot authors (else the bot's own replies
    /// would loop) and escalate human messages into a prompt that names the
    /// channel to reply on. Exercised through a real session, exactly as
    /// `process_event` evaluates it.
    #[tokio::test]
    async fn default_policy_ignores_bots_and_escalates_humans() {
        let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut session = crate::sandbox::Session::new().await.expect("session");
        session.run_host_script(DEFAULT_POLICY).expect("install policy");

        let decide = |session: &mut crate::sandbox::Session, event: &str| {
            session.read_to_string(format!(
                "JSON.stringify(globalThis.onDiscordEvent({event}) ?? null)"
            ))
        };

        let bot = decide(
            &mut session,
            r#"{ "author": { "username": "sdkmode", "bot": true }, "channel_id": "1", "content": "hi" }"#,
        );
        assert_eq!(bot, "null", "bot messages must be ignored");

        let human = decide(
            &mut session,
            r#"{ "author": { "username": "he1d1" }, "channel_id": "42", "content": "hey bot" }"#,
        );
        let prompt: String = serde_json::from_str(&human).expect("policy returns a JSON string");
        assert!(prompt.contains("channel 42"), "prompt names the channel: {prompt}");
        assert!(prompt.contains("he1d1"), "prompt names the author: {prompt}");
        assert!(prompt.contains("discord.send"), "prompt tells you how to reply: {prompt}");
    }
}
