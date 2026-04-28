use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use codex_api::ApiError;
use codex_api::ReqwestTransport;
use codex_api::decode_models_response;
use codex_api::map_api_error;
use codex_client::HttpTransport;
use codex_client::Request;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::default_client::build_reqwest_client;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::ModelsEndpointClient;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CoreResult;
use codex_protocol::openai_models::ModelInfo;
use http::Method;
use tokio::time::timeout;

const MODELS_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
const MODELS_SYNC_URL: &str = "https://raw.githubusercontent.com/Trirrin/codex/main/model.json";

/// GitHub-hosted OpenAI-compatible model catalog endpoint.
#[derive(Debug)]
pub(crate) struct OpenAiModelsEndpoint {
    provider_info: ModelProviderInfo,
    auth_manager: Option<Arc<AuthManager>>,
    models_url: String,
}

impl OpenAiModelsEndpoint {
    pub(crate) fn new(
        provider_info: ModelProviderInfo,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        Self::with_models_url(provider_info, auth_manager, MODELS_SYNC_URL.to_string())
    }

    #[cfg(test)]
    pub(crate) fn new_with_models_url(
        provider_info: ModelProviderInfo,
        auth_manager: Option<Arc<AuthManager>>,
        models_url: String,
    ) -> Self {
        Self::with_models_url(provider_info, auth_manager, models_url)
    }

    fn with_models_url(
        provider_info: ModelProviderInfo,
        auth_manager: Option<Arc<AuthManager>>,
        models_url: String,
    ) -> Self {
        Self {
            provider_info,
            auth_manager,
            models_url,
        }
    }

    async fn auth(&self) -> Option<CodexAuth> {
        match self.auth_manager.as_ref() {
            Some(auth_manager) => auth_manager.auth().await,
            None => None,
        }
    }

    fn models_url_for_client(&self, client_version: &str) -> String {
        let separator = if self.models_url.contains('?') {
            '&'
        } else {
            '?'
        };
        format!(
            "{}{}client_version={client_version}",
            self.models_url, separator
        )
    }
}

#[async_trait]
impl ModelsEndpointClient for OpenAiModelsEndpoint {
    fn has_command_auth(&self) -> bool {
        self.provider_info.has_command_auth()
    }

    async fn uses_codex_backend(&self) -> bool {
        self.auth()
            .await
            .as_ref()
            .is_some_and(CodexAuth::uses_codex_backend)
    }

    async fn list_models(
        &self,
        client_version: &str,
    ) -> CoreResult<(Vec<ModelInfo>, Option<String>)> {
        let _timer =
            codex_otel::start_global_timer("codex.remote_models.fetch_update.duration_ms", &[]);
        let transport = ReqwestTransport::new(build_reqwest_client());
        let request = Request::new(Method::GET, self.models_url_for_client(client_version));

        let response = timeout(MODELS_REFRESH_TIMEOUT, transport.execute(request))
            .await
            .map_err(|_| CodexErr::Timeout)?
            .map_err(ApiError::from)
            .map_err(map_api_error)?;

        decode_models_response(&response).map_err(map_api_error)
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;
    use codex_protocol::config_types::ModelProviderAuthInfo;
    use codex_protocol::openai_models::ModelsResponse;
    use pretty_assertions::assert_eq;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::matchers::query_param;

    fn provider_info_with_command_auth() -> ModelProviderInfo {
        ModelProviderInfo {
            auth: Some(ModelProviderAuthInfo {
                command: "print-token".to_string(),
                args: Vec::new(),
                timeout_ms: NonZeroU64::new(5_000).expect("timeout should be non-zero"),
                refresh_interval_ms: 300_000,
                cwd: std::env::current_dir()
                    .expect("current dir should be available")
                    .try_into()
                    .expect("current dir should be absolute"),
            }),
            requires_openai_auth: false,
            ..ModelProviderInfo::create_openai_provider(/*base_url*/ None)
        }
    }

    #[tokio::test]
    async fn list_models_fetches_from_github_catalog_url() {
        let server = MockServer::start().await;
        let remote_model = codex_models_manager::model_info::model_info_from_slug("github-model");
        let expected_model = codex_protocol::openai_models::ModelInfo {
            used_fallback_model_metadata: false,
            ..remote_model.clone()
        };

        Mock::given(method("GET"))
            .and(path("/model.json"))
            .and(query_param("client_version", "1.2.3"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("etag", "\"catalog-v1\"")
                    .set_body_json(ModelsResponse {
                        models: vec![remote_model.clone()],
                    }),
            )
            .expect(1)
            .mount(&server)
            .await;

        let endpoint = OpenAiModelsEndpoint::new_with_models_url(
            ModelProviderInfo::create_openai_provider(/*base_url*/ None),
            /*auth_manager*/ None,
            format!("{}/model.json", server.uri()),
        );

        let result = endpoint
            .list_models("1.2.3")
            .await
            .expect("models should load");

        assert_eq!(
            result,
            (vec![expected_model], Some("\"catalog-v1\"".to_string()))
        );
    }

    #[test]
    fn command_auth_provider_reports_command_auth_without_cached_auth() {
        let endpoint = OpenAiModelsEndpoint::new(
            provider_info_with_command_auth(),
            /*auth_manager*/ None,
        );

        assert!(endpoint.has_command_auth());
    }

    #[test]
    fn provider_without_command_auth_reports_no_command_auth() {
        let endpoint = OpenAiModelsEndpoint::new(
            ModelProviderInfo::create_openai_provider(/*base_url*/ None),
            /*auth_manager*/ None,
        );

        assert!(!endpoint.has_command_auth());
    }
}
