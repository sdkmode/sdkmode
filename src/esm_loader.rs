use deno_core::ModuleLoadOptions;
use deno_core::ModuleLoadReferrer;
use deno_core::ModuleLoadResponse;
use deno_core::ModuleLoader;
use deno_core::ModuleSource;
use deno_core::ModuleSourceCode;
use deno_core::ModuleSpecifier;
use deno_core::ModuleType;
use deno_core::ResolutionKind;
use deno_core::error::ModuleLoaderError;
use deno_core::resolve_import;
use deno_error::JsErrorBox;
use http::Request;
use http::header::LOCATION;
use http_body_util::BodyExt;

const OCTOKIT_SPECIFIER: &str = "@octokit/rest";
const OCTOKIT_ESM_SH_URL: &str = "https://esm.sh/@octokit/rest";
const MAX_REDIRECTS: usize = 10;

#[derive(Clone, Debug)]
pub struct EsmLoader {
    client: deno_fetch::Client,
}

impl EsmLoader {
    pub fn new() -> Result<Self, anyhow::Error> {
        let client = deno_fetch::create_http_client("sdkmode/0.1.0", Default::default())?;
        Ok(Self { client })
    }

    fn module_type(specifier: &ModuleSpecifier) -> ModuleType {
        if specifier.path().ends_with(".json") {
            ModuleType::Json
        } else {
            ModuleType::JavaScript
        }
    }

    async fn load_remote(
        &self,
        requested_specifier: ModuleSpecifier,
    ) -> Result<ModuleSource, ModuleLoaderError> {
        let (found_specifier, code) = self.fetch_with_redirects(requested_specifier.clone()).await?;

        Ok(ModuleSource::new_with_redirect(
            Self::module_type(&found_specifier),
            ModuleSourceCode::String(code.into()),
            &requested_specifier,
            &found_specifier,
            None,
        ))
    }

    async fn fetch_with_redirects(
        &self,
        start: ModuleSpecifier,
    ) -> Result<(ModuleSpecifier, String), ModuleLoaderError> {
        let mut current = start;

        for _ in 0..MAX_REDIRECTS {
            let response = self.send_request(&current).await?;

            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(LOCATION)
                    .ok_or_else(|| JsErrorBox::generic("redirect response missing location header"))?
                    .to_str()
                    .map_err(|error| JsErrorBox::generic(error.to_string()))?;

                current = current.join(location).map_err(JsErrorBox::from_err)?;
                continue;
            }

            if !response.status().is_success() {
                return Err(JsErrorBox::generic(format!(
                    "failed to load module {}: {}",
                    current,
                    response.status()
                )));
            }

            let body = response
                .into_body()
                .collect()
                .await
                .map_err(JsErrorBox::from_err)?
                .to_bytes();

            let code = String::from_utf8(body.to_vec()).map_err(|error| {
                JsErrorBox::generic(format!("module {} was not valid utf-8: {}", current, error))
            })?;

            return Ok((current, code));
        }

        Err(JsErrorBox::generic(format!(
            "too many redirects while loading module {}",
            current
        )))
    }

    async fn send_request(
        &self,
        specifier: &ModuleSpecifier,
    ) -> Result<http::Response<deno_fetch::ResBody>, ModuleLoaderError> {
        let uri = specifier
            .as_str()
            .parse::<http::Uri>()
            .map_err(|error| JsErrorBox::generic(error.to_string()))?;
        let request = Request::get(uri)
            .body(deno_fetch::ReqBody::empty())
            .map_err(|error| JsErrorBox::generic(error.to_string()))?;

        self.client.clone().send(request).await.map_err(JsErrorBox::from_err)
    }
}

impl ModuleLoader for EsmLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, ModuleLoaderError> {
        if specifier == OCTOKIT_SPECIFIER {
            return ModuleSpecifier::parse(OCTOKIT_ESM_SH_URL).map_err(JsErrorBox::from_err);
        }

        resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let loader = self.clone();
        let module_specifier = module_specifier.clone();

        ModuleLoadResponse::Async(Box::pin(async move { loader.load_remote(module_specifier).await }))
    }
}
