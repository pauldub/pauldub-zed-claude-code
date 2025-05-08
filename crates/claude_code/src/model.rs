use anyhow::{anyhow, Context as _, Result};
use futures::Stream;
use futures::{future::BoxFuture, stream::BoxStream, FutureExt, StreamExt};
use gpui::{App, AsyncApp};
use language_model::{
    LanguageModel, LanguageModelCompletionError, LanguageModelCompletionEvent, LanguageModelId,
    LanguageModelName, LanguageModelProviderId, LanguageModelProviderName, LanguageModelRequest,
    LanguageModelToolSchemaFormat, LanguageModelToolUse, StopReason, TokenUsage,
};
use log::{debug, error, warn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use smol::io::{AsyncBufReadExt, BufReader};
use smol::process::Child;
use std::{process::Stdio, time::Duration};
use util::command;

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

    pub fn run_command(&self, prompt: &str) -> Result<Child> {
        debug!("Running Claude CLI with prompt: {}", prompt);

        // Build the command
        let mut cmd = command::new_smol_command(&self.cli_path);

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

/// Event mapper for Claude CLI responses
pub struct ClaudeCliEventMapper {
    token_usage: TokenUsage,
    json_buffer: String,
}

impl ClaudeCliEventMapper {
    pub fn new() -> Self {
        Self {
            token_usage: TokenUsage::default(),
            json_buffer: String::new(),
        }
    }

    pub fn map_stream(
        mut self,
        reader: BufReader<impl smol::io::AsyncRead + Unpin>,
    ) -> impl Stream<Item = Result<LanguageModelCompletionEvent, LanguageModelCompletionError>>
    {
        // Create a lines stream from the reader
        let lines_stream = async_stream::stream! {
            let mut line = String::new();
            let mut buf_reader = reader;

            loop {
                line.clear();
                match buf_reader.read_line(&mut line).await {
                    Ok(0) => break, // EOF
                    Ok(_) => yield Ok(line.clone()),
                    Err(e) => {
                        yield Err(anyhow::anyhow!(e).into());
                        break;
                    }
                }
            }
        };

        // Map each line to events
        lines_stream.flat_map(move |line_result| {
            futures::stream::iter(match line_result {
                Ok(line) => self.map_line(line),
                Err(e) => vec![Err(LanguageModelCompletionError::Other(e))],
            })
        })
    }

    fn map_line(
        &mut self,
        line: String,
    ) -> Vec<Result<LanguageModelCompletionEvent, LanguageModelCompletionError>> {
        // Skip empty lines
        if line.trim().is_empty() {
            return Vec::new();
        }

        // Accumulate JSON and try to parse it
        match self.accumulate_and_parse_json(&line) {
            Ok(Some(events)) => events.into_iter().map(Ok).collect(),
            Ok(None) => Vec::new(),
            Err(e) => {
                error!("Error parsing Claude CLI output: {:?}", e);
                vec![Err(LanguageModelCompletionError::Other(e))]
            }
        }
    }

    /// Accumulate lines until we have a complete JSON object, then parse it
    fn accumulate_and_parse_json(
        &mut self,
        line: &str,
    ) -> Result<Option<Vec<LanguageModelCompletionEvent>>> {
        // Remove "data: " prefix if present (used in SSE format)
        let json_str = line.trim_start_matches("data: ").trim();

        if json_str.is_empty() {
            return Ok(None);
        }

        // Special case: if the line itself is a complete JSON object, try to parse it directly
        if (json_str.starts_with("{") && json_str.ends_with("}"))
            || (json_str.starts_with("[") && json_str.ends_with("]"))
        {
            if let Some(events) = self.parse_claude_json_object(json_str)? {
                return Ok(Some(events));
            }
        }

        // Add a safety limit to prevent buffer from growing too large (1MB limit)
        const MAX_BUFFER_SIZE: usize = 1 * 1024 * 1024;

        // If the buffer is getting too large, log an error and reset it
        if self.json_buffer.len() + json_str.len() > MAX_BUFFER_SIZE {
            warn!(
                "JSON buffer exceeded max size ({}KB), resetting",
                MAX_BUFFER_SIZE / 1024
            );
            self.json_buffer.clear();
        }

        // Otherwise, accumulate the JSON
        self.json_buffer.push_str(json_str);
        self.json_buffer.push('\n');

        // Check if we have a complete JSON object by counting braces
        let mut open_braces = 0;
        let mut inside_string = false;
        let mut escape_next = false;

        for c in self.json_buffer.chars() {
            if escape_next {
                escape_next = false;
                continue;
            }

            match c {
                '\\' if inside_string => escape_next = true,
                '"' => inside_string = !inside_string,
                '{' if !inside_string => open_braces += 1,
                '}' if !inside_string => open_braces -= 1,
                _ => {}
            }
        }

        // If we have a complete JSON object, try to parse it
        if open_braces == 0 && !self.json_buffer.is_empty() {
            // Try to parse the JSON
            if let Some(events) = self.parse_claude_json_object(&self.json_buffer)? {
                debug!(
                    "Successfully parsed complete JSON object ({} chars, {} events)",
                    self.json_buffer.len(),
                    events.len()
                );
                // Clear the buffer
                self.json_buffer.clear();
                return Ok(Some(events));
            }
        }

        // Not a complete JSON object yet
        Ok(None)
    }

    fn parse_claude_json_object(
        &self,
        json_str: &str,
    ) -> Result<Option<Vec<LanguageModelCompletionEvent>>> {
        let mut events = Vec::new();

        // Try to parse as a Claude message
        if let Ok(message) = serde_json::from_str::<ClaudeMessage>(json_str) {
            debug!("Successfully parsed Claude message with id: {}", message.id);

            // Create token usage instead of updating internal state
            let token_usage = TokenUsage {
                input_tokens: message.usage.input_tokens,
                output_tokens: message.usage.output_tokens,
                ..Default::default()
            };

            // Add usage update event with the created token usage
            events.push(LanguageModelCompletionEvent::UsageUpdate(token_usage));

            // Process message content
            if let Some(content) = message.content.first() {
                match content {
                    ClaudeContent::Text { text } => {
                        events.push(LanguageModelCompletionEvent::Text(text.clone()));
                    }
                    ClaudeContent::ToolUse { id, name, input } => {
                        let raw_input = serde_json::to_string(input).unwrap_or_default();
                        events.push(LanguageModelCompletionEvent::ToolUse(
                            LanguageModelToolUse {
                                id: id.clone().into(),
                                name: name.as_str().into(),
                                raw_input: raw_input.clone(),
                                input: input.clone(),
                                is_input_complete: true,
                            },
                        ));
                    }
                }
            }

            // Check for stop reason
            if let Some(stop_reason) = message.stop_reason {
                events.push(LanguageModelCompletionEvent::Stop(
                    match stop_reason.as_str() {
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        _ => StopReason::EndTurn,
                    },
                ));
            }

            return Ok(Some(events));
        }

        // Try to parse as a user response (tool result)
        if let Ok(_user_response) = serde_json::from_str::<ClaudeUserResponse>(json_str) {
            // We don't usually need to process these, as they're echoed back from our side
            debug!("Parsed user response (tool result)");
            return Ok(None);
        }

        // If it looks like a JSON fragment, it's likely part of a larger JSON object
        if (json_str.trim().starts_with("{") || json_str.trim().starts_with("["))
            && !(json_str.trim().ends_with("}") || json_str.trim().ends_with("]"))
        {
            debug!("Received partial JSON fragment, continuing to accumulate");
        } else {
            // Log a helpful message about the parsing failure
            let preview = if json_str.len() > 100 {
                format!("{}... (truncated)", &json_str[..100])
            } else {
                json_str.to_string()
            };
            debug!(
                "Couldn't parse Claude CLI JSON object ({} chars): {}",
                json_str.len(),
                preview
            );
        }

        Ok(None)
    }
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
        // Clone the runner to avoid lifetime issues
        let runner = self.runner.clone();

        // Convert the request to a prompt for the Claude CLI
        let prompt = tools::convert_request_to_prompt(&request);

        async move {
            // Run the Claude CLI command
            let mut process = runner
                .run_command(&prompt)
                .context("Failed to run Claude CLI")?;

            // Get the stdout from the process
            let stdout = process
                .stdout
                .take()
                .ok_or_else(|| anyhow!("Failed to get stdout from Claude CLI process"))?;

            // Create a buffered reader from stdout - explicitly wrap to get AsyncBufReadExt
            let reader = BufReader::new(stdout);

            // Create an event mapper and stream the results
            let stream = ClaudeCliEventMapper::new().map_stream(reader);

            Ok(stream.boxed())
        }
        .boxed()
    }
}
