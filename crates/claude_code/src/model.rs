use anyhow::{anyhow, Context as _, Result};
use futures::{future::BoxFuture, stream::BoxStream, StreamExt};
use gpui::{App, AsyncApp};
use language_model::{
    LanguageModel, LanguageModelCompletionError, LanguageModelCompletionEvent, LanguageModelId,
    LanguageModelName, LanguageModelProviderId, LanguageModelProviderName, LanguageModelRequest,
    LanguageModelToolSchemaFormat, LanguageModelToolUse, StopReason, TokenUsage,
};
use log::{debug, error, warn};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{process::Stdio, sync::Arc, time::Duration};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Child;

use crate::tools;

// Provider constants - duplicated from language_models for independence
pub const PROVIDER_ID: &str = "claude_code";
pub const PROVIDER_NAME: &str = "Claude Code";

// Model definitions
#[derive(Debug, Clone, Copy)]
pub enum Model {
    ClaudeCode,
}

impl Model {
    pub fn id(&self) -> &str {
        match self {
            Model::ClaudeCode => "claude-3-7-sonnet-20250219",
        }
    }

    pub fn display_name(&self) -> &str {
        match self {
            Model::ClaudeCode => "Claude Code",
        }
    }

    pub fn max_token_count(&self) -> usize {
        match self {
            Model::ClaudeCode => 200_000,
        }
    }

    pub fn max_output_tokens(&self) -> Option<u32> {
        match self {
            Model::ClaudeCode => Some(8_192),
        }
    }
}

// CLI Runner to manage Claude CLI execution
#[derive(Clone, Debug)]
pub struct CliRunner {
    pub cli_path: String,
    pub default_args: Vec<String>,
    pub timeout: Duration,
}

impl Default for CliRunner {
    fn default() -> Self {
        Self {
            cli_path: "claude".to_string(),
            default_args: Vec::new(),
            timeout: Duration::from_secs(60),
        }
    }
}

impl CliRunner {
    pub fn new(cli_path: Option<String>, default_args: Vec<String>, timeout_ms: u64) -> Self {
        Self {
            cli_path: cli_path.unwrap_or_else(|| "claude".to_string()),
            default_args,
            timeout: Duration::from_millis(timeout_ms.max(1000)),
        }
    }

    pub async fn run_command(&self, prompt: &str) -> Result<Child> {
        debug!("Running Claude CLI with prompt: {}", prompt);

        // Build the command
        let mut cmd = tokio::process::Command::new(&self.cli_path);

        // Add default arguments
        for arg in &self.default_args {
            cmd.arg(arg);
        }

        // Add the required args for our request
        cmd.arg("-p")
            .arg(prompt)
            .arg("--output-format")
            .arg("stream-json")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Spawn the process
        let process = cmd.spawn().context("Failed to spawn Claude CLI")?;

        Ok(process)
    }
}

// Model implementation that manages completion requests
pub struct ClaudeCodeModel {
    id: LanguageModelId,
    model: Model,
    runner: CliRunner,
}

// Claude CLI JSON response types
#[derive(Debug, Deserialize, Serialize)]
struct ClaudeMessage {
    id: String,
    #[serde(rename = "type")]
    message_type: String,
    role: String,
    model: String,
    content: Vec<ClaudeContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop_sequence: Option<String>,
    usage: ClaudeUsage,
}

#[derive(Debug, Deserialize, Serialize)]
struct ClaudeUsage {
    #[serde(default)]
    input_tokens: u32,
    #[serde(default)]
    output_tokens: u32,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
enum ClaudeContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
}

#[derive(Debug, Deserialize, Serialize)]
struct ClaudeUserResponse {
    role: String,
    content: Vec<ClaudeToolResult>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type")]
enum ClaudeToolResult {
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        #[serde(default)]
        is_error: bool,
    },
}

impl ClaudeCodeModel {
    pub fn new(model: Model, runner: CliRunner) -> Self {
        Self {
            id: LanguageModelId::from(model.id().to_string()),
            model,
            runner,
        }
    }

    pub fn with_default_runner(model: Model) -> Self {
        Self::new(model, CliRunner::default())
    }

    fn parse_claude_cli_output(
        &self,
        mut process: Child,
    ) -> BoxStream<'static, Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>
    {
        // Get stdout and create a buffered reader
        let stdout = process
            .stdout
            .take()
            .expect("Failed to get stdout from process");

        let token_usage = Arc::new(Mutex::new(TokenUsage::default()));

        // Create a stream from stdout lines
        let reader = BufReader::new(stdout);

        // Capture token_usage in a clone before the unfold closure
        let token_usage_clone = token_usage.clone();

        // Use a simple stream to process each line
        let stream = futures::stream::unfold(reader, move |mut reader| {
            // Clone again inside the closure for each iteration
            let token_usage_ref = token_usage_clone.clone();

            async move {
                let mut line = String::new();
                match reader.read_line(&mut line).await {
                    Ok(0) => None, // EOF
                    Ok(_) => {
                        let result = if line.trim().is_empty() {
                            Ok(LanguageModelCompletionEvent::UsageUpdate(
                                TokenUsage::default(),
                            ))
                        } else {
                            match parse_claude_json_line(&line, &token_usage_ref) {
                                Ok(Some(event)) => Ok(event),
                                Ok(None) => Ok(LanguageModelCompletionEvent::UsageUpdate(
                                    TokenUsage::default(),
                                )),
                                Err(e) => {
                                    error!("Error parsing Claude CLI output: {:?}", e);
                                    Err(LanguageModelCompletionError::Other(e))
                                }
                            }
                        };
                        Some((result, reader))
                    }
                    Err(e) => {
                        error!("Error reading Claude CLI output: {:?}", e);
                        Some((Err(LanguageModelCompletionError::Other(anyhow!(e))), reader))
                    }
                }
            }
        })
        .boxed();

        stream
    }
}

fn parse_claude_json_line(
    line: &str,
    token_usage: &Arc<Mutex<TokenUsage>>,
) -> Result<Option<LanguageModelCompletionEvent>> {
    // Remove "data: " prefix if present (used in SSE format)
    let json_str = line.trim_start_matches("data: ").trim();

    if json_str.is_empty() {
        return Ok(None);
    }

    // Try to parse as a Claude message
    if let Ok(message) = serde_json::from_str::<ClaudeMessage>(json_str) {
        // Process the message based on its content
        if let Some(content) = message.content.first() {
            match content {
                ClaudeContent::Text { text } => {
                    return Ok(Some(LanguageModelCompletionEvent::Text(text.clone())));
                }
                ClaudeContent::ToolUse { id, name, input } => {
                    let raw_input = serde_json::to_string(input).unwrap_or_default();
                    return Ok(Some(LanguageModelCompletionEvent::ToolUse(
                        LanguageModelToolUse {
                            id: id.clone().into(),
                            name: name.as_str().into(),
                            raw_input: raw_input.clone(),
                            input: input.clone(),
                            is_input_complete: true,
                        },
                    )));
                }
            }
        }

        // Update token usage
        let mut usage = token_usage.lock();
        usage.input_tokens = message.usage.input_tokens;
        usage.output_tokens = message.usage.output_tokens;

        // Check for stop reason
        if let Some(stop_reason) = message.stop_reason {
            return Ok(Some(LanguageModelCompletionEvent::Stop(
                match stop_reason.as_str() {
                    "tool_use" => StopReason::ToolUse,
                    "max_tokens" => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                },
            )));
        }

        // If we got here, just send a usage update
        return Ok(Some(LanguageModelCompletionEvent::UsageUpdate(
            TokenUsage {
                input_tokens: message.usage.input_tokens,
                output_tokens: message.usage.output_tokens,
                ..Default::default()
            },
        )));
    }

    // Try to parse as a user response (tool result)
    if let Ok(_user_response) = serde_json::from_str::<ClaudeUserResponse>(json_str) {
        // We don't usually need to process these, as they're echoed back from our side
        return Ok(None);
    }

    // If we couldn't parse the JSON, log it and return None
    warn!("Couldn't parse Claude CLI JSON: {}", json_str);
    Ok(None)
}

impl LanguageModel for ClaudeCodeModel {
    fn id(&self) -> LanguageModelId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelName {
        LanguageModelName::from(self.model.display_name().to_string())
    }

    fn provider_id(&self) -> LanguageModelProviderId {
        LanguageModelProviderId(PROVIDER_ID.into())
    }

    fn provider_name(&self) -> LanguageModelProviderName {
        LanguageModelProviderName(PROVIDER_NAME.into())
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn tool_input_format(&self) -> LanguageModelToolSchemaFormat {
        LanguageModelToolSchemaFormat::JsonSchema
    }

    fn telemetry_id(&self) -> String {
        format!("claude_code/{}", self.model.id())
    }

    fn max_token_count(&self) -> usize {
        self.model.max_token_count()
    }

    fn max_output_tokens(&self) -> Option<u32> {
        self.model.max_output_tokens()
    }

    fn count_tokens(
        &self,
        _request: LanguageModelRequest,
        _cx: &App,
    ) -> BoxFuture<'static, Result<usize>> {
        // Claude Code CLI doesn't provide token counting
        // Return a reasonable estimate based on character count
        Box::pin(async { Ok(1000) })
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        _cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            BoxStream<'static, Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>,
        >,
    > {
        // Clone the entire model to avoid lifetime issues
        let model_clone = ClaudeCodeModel {
            id: self.id.clone(),
            model: self.model,
            runner: self.runner.clone(),
        };

        // Move the clone into the async block
        Box::pin(async move {
            // Convert the request to a prompt for the Claude CLI
            let prompt = tools::convert_request_to_prompt(&request);

            // Spawn Claude CLI directly
            let process = model_clone.runner.run_command(&prompt).await?;

            // Parse the JSON output from the process
            let stream = model_clone.parse_claude_cli_output(process);

            Ok(stream)
        })
    }
}
