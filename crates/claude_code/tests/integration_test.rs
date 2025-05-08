use language_model::LanguageModel;
use claude_code::{Model, ClaudeCodeModel};

#[test]
fn test_claude_code_model_creation() {
    // Test the ClaudeCodeModel creation with default settings
    let model = ClaudeCodeModel::with_default_runner(Model::ClaudeCode);
    
    // Check if the model ID matches the expected value
    assert_eq!(model.id().0, "claude-3-7-sonnet-20250219");
    assert_eq!(model.provider_id().0, "claude_code");
    assert!(model.supports_tools());
}

// This test is commented out because it requires the actual Claude CLI to be installed
// Uncomment it when you have the Claude CLI or the stub script in your PATH
/*
#[tokio::test]
async fn test_claude_code_completion() {
    let mut cx = TestAppContext::new();
    
    // Initialize language model registry
    LanguageModelRegistry::init(&mut cx);
    
    // Register the Claude Code provider
    let provider = Arc::new(ClaudeCodeProvider::new(&mut cx));
    LanguageModelRegistry::global(&mut cx).update(&mut cx, |registry, cx| {
        registry.register_provider(provider.clone(), cx);
    });
    
    // Get the default model
    let model = provider.default_model(&mut cx).unwrap();
    
    // Create a simple request
    let request = LanguageModelRequest {
        messages: vec![
            language_model::Message {
                role: Role::User,
                content: vec![MessageContent::Text("Hello, Claude Code! What's your name?".into())],
            }
        ],
        tools: vec![],
        show_thinking: false,
        temperature: Some(0.7),
        stop: vec![],
    };
    
    // Get the async app context
    let async_cx = cx.to_async();
    
    // Stream completion
    let completion_stream = model.stream_completion(request, &async_cx).await.unwrap();
    
    // Collect all events from the stream
    let mut collected_text = String::new();
    let mut has_stop_event = false;
    
    while let Some(event) = completion_stream.next().await {
        match event {
            Ok(LanguageModelCompletionEvent::Text(text)) => {
                collected_text.push_str(&text);
            }
            Ok(LanguageModelCompletionEvent::Stop(_)) => {
                has_stop_event = true;
            }
            _ => {}
        }
    }
    
    // We should have received some text and a stop event
    assert!(!collected_text.is_empty());
    assert!(has_stop_event);
}
*/