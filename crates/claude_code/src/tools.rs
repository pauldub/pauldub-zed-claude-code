use anyhow::Result;
use language_model::{LanguageModelRequest, MessageContent, Role};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Claude Code tool schemas
#[derive(Debug, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value, // JSON Schema
}

/// A map of Zed tool names to Claude Code tool names
pub struct ToolMap {
    map: HashMap<String, String>,
}

impl ToolMap {
    pub fn new() -> Self {
        let mut map = HashMap::new();
        map.insert("LS".to_string(), "LS".to_string());
        map.insert("Grep".to_string(), "Grep".to_string());
        map.insert("Glob".to_string(), "Glob".to_string());
        map.insert("Read".to_string(), "Read".to_string());
        map.insert("Edit".to_string(), "Edit".to_string());
        map.insert("MultiEdit".to_string(), "MultiEdit".to_string());
        map.insert("Write".to_string(), "Write".to_string());
        map.insert("Bash".to_string(), "Bash".to_string());
        map.insert("Batch".to_string(), "Batch".to_string());
        map.insert("Task".to_string(), "Task".to_string());
        map.insert("NotebookRead".to_string(), "NotebookRead".to_string());
        map.insert("NotebookEdit".to_string(), "NotebookEdit".to_string());
        map.insert("WebFetch".to_string(), "WebFetch".to_string());
        map.insert("TodoRead".to_string(), "TodoRead".to_string());
        map.insert("TodoWrite".to_string(), "TodoWrite".to_string());
        Self { map }
    }

    pub fn get_claude_tool_name(&self, zed_tool_name: &str) -> Option<&String> {
        self.map.get(zed_tool_name)
    }
}

impl Default for ToolMap {
    fn default() -> Self {
        Self::new()
    }
}

/// Construct a Claude-compatible tool schema from a Zed tool
pub fn create_claude_tool_schema(tool_name: &str, schema: &Value) -> Result<Tool> {
    let description = schema
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("No description provided")
        .to_string();
    
    // Create the Claude tool schema
    Ok(Tool {
        name: tool_name.to_string(),
        description,
        input_schema: schema.clone(),
    })
}

/// Convert a Zed LanguageModelRequest to a prompt for Claude Code
pub fn convert_request_to_prompt(request: &LanguageModelRequest) -> String {
    let mut prompt = String::new();
    
    // Add system message if present
    if let Some(system_message) = request.messages.iter().find(|m| m.role == Role::System) {
        prompt.push_str("System: ");
        for content in &system_message.content {
            if let MessageContent::Text(text) = content {
                prompt.push_str(text);
                prompt.push_str("\n\n");
            }
        }
    }
    
    // Add remaining messages in order
    for message in request.messages.iter().filter(|m| m.role != Role::System) {
        match message.role {
            Role::User => prompt.push_str("User: "),
            Role::Assistant => prompt.push_str("Assistant: "),
            Role::System => continue, // Already handled
        }
        
        for content in &message.content {
            match content {
                MessageContent::Text(text) => {
                    prompt.push_str(text);
                    prompt.push('\n');
                }
                MessageContent::Thinking { text, .. } => {
                    prompt.push_str("<thinking>\n");
                    prompt.push_str(text);
                    prompt.push_str("\n</thinking>\n");
                }
                // Skip other message content types for now
                _ => {}
            }
        }
        
        prompt.push('\n');
    }
    
    prompt
}