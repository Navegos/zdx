//! Grok Build (xAI Grok subscription OAuth) provider using the Responses API.
//!
//! Authenticates with a `SuperGrok` / X Premium+ subscription via the Grok CLI
//! OAuth flow and sends Responses requests to the Grok CLI chat proxy
//! (`https://cli-chat-proxy.grok.com/v1/responses`) with a short-lived OAuth
//! bearer that is refreshed on demand.
//!
//! `api.x.ai` bills OAuth tokens against API-team credits (HTTP 402
//! `personal-team-blocked:spending-limit`); only the proxy bills the
//! subscription. The proxy rejects requests without Grok CLI client headers, or
//! with a client version below its minimum, with HTTP 426.

use anyhow::{Context, Result};
use reqwest::header::{HeaderMap, HeaderValue};
use zdx_types::ToolDefinition;
use zdx_types::config::ThinkingLevel;

use crate::oauth::grok_build as oauth_grok_build;
use crate::openai::responses::{ResponsesConfig, send_responses_stream};
use crate::shared::merge_system_prompt;
use crate::{ChatMessage, ProviderKind, ProviderStream};

const RESPONSES_PATH: &str = "/responses";
/// Grok CLI version reported to the proxy. Bump it when the proxy answers 426.
const GROK_CLI_VERSION: &str = "1.0.46";

fn reasoning_effort_from_thinking_level(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => "high",
    }
}

/// Resolves the OAuth access token, refreshing (and persisting) if expired.
///
/// # Errors
/// Returns an error if no credentials are stored or the refresh fails.
pub async fn resolve_access_token(account: Option<&str>) -> Result<String> {
    let creds = oauth_grok_build::load_credentials(account)?.ok_or_else(|| {
        anyhow::anyhow!("No Grok Build OAuth credentials found. Run `zdx login --grok-build`.")
    })?;

    let creds = if creds.is_expired() {
        crate::oauth::refresh_locked(
            oauth_grok_build::PROVIDER_KEY,
            account,
            |refresh| async move { oauth_grok_build::refresh_token(&refresh).await },
        )
        .await
        .context("Failed to refresh Grok Build OAuth token")?
    } else {
        creds
    };

    Ok(creds.access)
}

fn build_headers(access_token: &str) -> Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    headers.insert(
        "Authorization",
        crate::shared::header_value("Grok Build access token", &format!("Bearer {access_token}"))?,
    );
    headers.insert("X-XAI-Token-Auth", HeaderValue::from_static("xai-grok-cli"));
    headers.insert(
        "x-grok-client-version",
        HeaderValue::from_static(GROK_CLI_VERSION),
    );
    headers.insert(
        "x-grok-client-identifier",
        HeaderValue::from_static("grok-shell"),
    );
    headers.insert("accept", HeaderValue::from_static("text/event-stream"));
    headers.insert("content-type", HeaderValue::from_static("application/json"));
    headers.insert(
        "user-agent",
        HeaderValue::from_static(crate::shared::USER_AGENT),
    );
    Ok(headers)
}

/// Grok Build client using the xAI Responses API with OAuth auth.
pub struct GrokBuildClient {
    config: ResponsesConfig,
    account: Option<String>,
    http: reqwest::Client,
}

impl GrokBuildClient {
    pub fn new(
        base_url: String,
        model: String,
        max_tokens: Option<u32>,
        prompt_cache_key: Option<String>,
        reasoning_effort: String,
        account: Option<String>,
    ) -> Self {
        Self {
            config: ResponsesConfig {
                base_url,
                path: RESPONSES_PATH.to_string(),
                model,
                max_output_tokens: max_tokens,
                reasoning_effort: Some(reasoning_effort),
                reasoning_summary: None,
                instructions: None,
                text_verbosity: None,
                store: Some(false),
                include: Some(vec!["reasoning.encrypted_content".to_string()]),
                stream_options: None,
                prompt_cache_key,
                parallel_tool_calls: Some(true),
                tool_choice: Some("auto".to_string()),
                truncation: None,
                service_tier: None,
            },
            account,
            http: crate::shared::streaming_http_client(),
        }
    }

    ///
    /// # Errors
    /// Returns an error if credentials cannot be resolved or the request fails.
    pub async fn send_messages_stream(
        &self,
        messages: &[ChatMessage],
        tools: &[ToolDefinition],
        system: Option<&str>,
    ) -> Result<ProviderStream> {
        let access_token = resolve_access_token(self.account.as_deref()).await?;
        let system = merge_system_prompt(system);
        send_responses_stream(
            "grok-build",
            &self.http,
            &self.config,
            build_headers(&access_token)?,
            messages,
            tools,
            system.as_deref(),
        )
        .await
    }
}

/// Constructs the Grok Build client from the given context.
///
/// # Errors
/// Returns an error if the base URL cannot be resolved.
pub fn build(
    ctx: &crate::ProviderBuildContext<'_>,
) -> anyhow::Result<Box<dyn crate::StreamingProvider>> {
    let base_url = ProviderKind::GrokBuild.resolve_base_url(ctx.base_url)?;
    Ok(Box::new(GrokBuildClient::new(
        base_url,
        ctx.model.to_string(),
        ctx.config_max_tokens,
        ctx.cache_key.clone(),
        reasoning_effort_from_thinking_level(ctx.thinking_level).to_string(),
        ctx.account.map(ToString::to_string),
    )))
}

#[cfg(test)]
mod tests {
    use zdx_types::config::ThinkingLevel;

    use super::{
        GROK_CLI_VERSION, GrokBuildClient, RESPONSES_PATH, build_headers,
        reasoning_effort_from_thinking_level,
    };
    use crate::ProviderKind;

    #[test]
    fn grok_build_routes_to_cli_chat_proxy_and_xai_stays_on_api() {
        assert_eq!(
            ProviderKind::GrokBuild.resolve_base_url(None).unwrap(),
            "https://cli-chat-proxy.grok.com/v1"
        );
        assert_eq!(ProviderKind::Xai.meta().base_url, "https://api.x.ai/v1");
    }

    #[test]
    fn grok_build_sends_grok_cli_client_headers() {
        let headers = build_headers("token-123").unwrap();
        assert_eq!(headers["Authorization"], "Bearer token-123");
        assert_eq!(headers["X-XAI-Token-Auth"], "xai-grok-cli");
        assert_eq!(headers["x-grok-client-version"], GROK_CLI_VERSION);
        assert_eq!(headers["x-grok-client-identifier"], "grok-shell");
    }

    #[test]
    fn grok_build_uses_responses_path_and_encrypted_reasoning() {
        let client = GrokBuildClient::new(
            "https://cli-chat-proxy.grok.com/v1".to_string(),
            "grok-4.5".to_string(),
            Some(1024),
            Some("thread-123".to_string()),
            "high".to_string(),
            None,
        );
        assert_eq!(client.config.path, RESPONSES_PATH);
        assert_eq!(client.config.model, "grok-4.5");
        assert_eq!(client.config.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(
            client.config.include.as_ref(),
            Some(&vec!["reasoning.encrypted_content".to_string()])
        );
    }

    #[test]
    fn grok_build_reasoning_effort_clamps_zdx_levels() {
        for level in [ThinkingLevel::Off, ThinkingLevel::Low] {
            assert_eq!(reasoning_effort_from_thinking_level(level), "low");
        }
        assert_eq!(
            reasoning_effort_from_thinking_level(ThinkingLevel::Medium),
            "medium"
        );
        for level in [
            ThinkingLevel::High,
            ThinkingLevel::XHigh,
            ThinkingLevel::Max,
        ] {
            assert_eq!(reasoning_effort_from_thinking_level(level), "high");
        }
    }
}
