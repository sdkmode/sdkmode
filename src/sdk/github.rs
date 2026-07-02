use deno_error::JsErrorBox;
use std::collections::HashMap;
use std::pin::Pin;
use tokio::process::Command;
use url::Url;

pub(crate) struct GitHub {
    token: Option<String>,
}

impl GitHub {
    pub(crate) fn new() -> Self {
        Self { token: None }
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

impl super::Sdk for GitHub {
    fn url(&self) -> Url {
        Url::parse("https://api.github.com").unwrap()
    }

    fn auth<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>> {
        Box::pin(async move {
            let output = Command::new("gh")
                .arg("auth")
                .arg("token")
                .output()
                .await
                .map_err(JsErrorBox::from_err)?;

            let token = token_from_output(output.status.success(), &output.stdout, &output.stderr)
                .map_err(JsErrorBox::generic)?;

            self.token = Some(token);
            Ok(())
        })
    }

    fn is_authed(&self) -> bool {
        self.token.is_some()
    }

    fn auth_header(&self) -> Option<String> {
        self.token.as_ref().map(|t| format!("Bearer {}", t.trim()))
    }
    fn cookies(&self) -> Option<HashMap<String, String>> {
        Some(HashMap::<String, String>::new())
    }

    fn packages(&self) -> &'static [&'static str] {
        &["@octokit/rest"]
    }
}

#[cfg(test)]
mod tests {
    use super::token_from_output;

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
}
