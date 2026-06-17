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

impl super::Sdk for GitHub {
    fn url(&self) -> Url {
        Url::parse("https://api.github.com").unwrap()
    }

    fn auth<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>> {
        Box::pin(async move {
            self.token = String::from_utf8(
                Command::new("gh")
                    .arg("auth")
                    .arg("token")
                    .output()
                    .await
                    .or_else(|e| Err(JsErrorBox::from_err(e)))?
                    .stdout
                    .to_vec(),
            )
            .ok();
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
}
