use anyhow::{Result, anyhow};
use claude_code::{ClaudeCodeModel, Model};
use gpui::{AnyView, App, Context, Entity, Render, Subscription, Task, Window, div, prelude::*};
use language_model::{
    AuthenticateError, LanguageModel, LanguageModelProvider, LanguageModelProviderId,
    LanguageModelProviderName, LanguageModelProviderState,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsStore};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
};
use ui::{Color, Icon, Label, List, h_flex, prelude::*, v_flex};

use crate::{AllLanguageModelSettings, ui::InstructionListItem};

// Provider constants
pub const PROVIDER_ID: &str = "claude_code";
pub const PROVIDER_NAME: &str = "Claude Code";
pub const CLAUDE_CLI_COMMAND: &str = "claude"; // The actual command to run

// Settings for the Claude Code provider
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ClaudeCodeSettings {
    /// Path to the Claude CLI executable, if it's not in the default location or PATH
    pub cli_path: Option<String>,

    /// Additional default arguments to pass to the CLI
    pub default_args: Vec<String>,

    /// Maximum time in milliseconds to wait for a response before timing out
    pub timeout_ms: u64,
}

/// Helper functions for Claude Code settings
impl ClaudeCodeSettings {
    /// Get the path to the Claude CLI executable
    pub fn cli_path(&self) -> PathBuf {
        if let Some(path) = &self.cli_path {
            PathBuf::from(path)
        } else {
            let cli_path = which::which("claude");
            cli_path.unwrap_or(PathBuf::from("claude"))
        }
    }

    /// Get the timeout in milliseconds
    pub fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.timeout_ms.max(1000))
    }

    /// Check if the Claude CLI is available in PATH
    pub fn is_cli_available(&self) -> bool {
        if let Some(path) = &self.cli_path {
            // If user specified a path, check if that file exists
            Path::new(path).exists()
        } else {
            // Otherwise check if "claude" is in PATH
            Command::new(CLAUDE_CLI_COMMAND)
                .arg("--version")
                .output()
                .map(|output| output.status.success())
                .unwrap_or(false)
        }
    }
}

// Provider state that manages authentication and settings
pub struct ClaudeCodeProviderState {
    // CLI is available if we can detect it on the system
    cli_available: bool,
    cli_path: Option<String>,
    cli_version: Option<String>,
    checking_cli: bool,
    _subscription: Subscription,
}

// Provider implementation that handles registration and model creation
#[derive(Clone)]
pub struct ClaudeCodeProvider {
    state: Entity<ClaudeCodeProviderState>,
}

impl ClaudeCodeProviderState {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Initialize with settings
        let settings = AllLanguageModelSettings::get_global(cx);
        let cli_path = settings.claude_code.cli_path.clone();

        let mut state = Self {
            cli_available: false,
            cli_path,
            cli_version: None,
            checking_cli: false,
            _subscription: cx.observe_global::<SettingsStore>(|this: &mut Self, cx| {
                // Get the updated settings
                let settings = AllLanguageModelSettings::get_global(cx);
                this.cli_path = settings.claude_code.cli_path.clone();

                // Notify to update UI
                cx.notify();

                // Check CLI availability with new settings
                this.check_cli_availability(cx).detach_and_log_err(cx);
            }),
        };

        // Initial check for CLI availability
        state.check_cli_availability(cx).detach_and_log_err(cx);

        state
    }

    fn check_cli_availability(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        if self.checking_cli {
            return Task::ready(Ok(()));
        }

        self.checking_cli = true;
        cx.notify();

        let cmd_name = self
            .cli_path
            .clone()
            .unwrap_or_else(|| CLAUDE_CLI_COMMAND.to_string());

        let output = match Command::new(&cmd_name).arg("--version").output() {
            Ok(output) if output.status.success() => {
                // Parse version from output
                let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
                Some(version)
            }
            _ => None,
        };

        self.cli_available = output.is_some();
        self.cli_version = output;
        self.checking_cli = false;

        cx.spawn(async |this, cx| this.update(cx, |_, cx| cx.notify()))
    }

    pub fn is_authenticated(&self) -> bool {
        self.cli_available
    }

    pub fn authenticate(&self, cx: &mut Context<Self>) -> Task<Result<(), AuthenticateError>> {
        if self.is_authenticated() {
            return Task::ready(Ok(()));
        }

        // For Claude CLI, authentication just means checking if the CLI is available
        cx.spawn(async move |this, cx| {
            // Make a mutable update to start checking
            let _ = this.update(cx, |this, cx| -> Result<(), AuthenticateError> {
                if !this.checking_cli {
                    this.checking_cli = true;
                    cx.notify();
                }
                Ok(())
            })?;

            let cmd_name =
                this.read_with(cx, |this, _cx| {
                    this.cli_path.clone()
                }).unwrap_or(None).unwrap_or_else(|| CLAUDE_CLI_COMMAND.to_string());

            // Check CLI availability
            let output = Command::new(&cmd_name).arg("--version").output();

            match output {
                Ok(output) if output.status.success() => {
                    // Parse version from output
                    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();

                    let _ = this.update(cx, |this, cx| -> Result<(), AuthenticateError> {
                        this.cli_available = true;
                        this.cli_version = Some(version);
                        this.checking_cli = false;
                        cx.notify();
                        Ok(())
                    })?;

                    Ok(())
                }
                _ => {
                    let _ = this.update(cx, |this, cx| -> Result<(), AuthenticateError> {
                        this.cli_available = false;
                        this.cli_version = None;
                        this.checking_cli = false;
                        cx.notify();
                        Ok(())
                    })?;

                    Err(AuthenticateError::Other(anyhow!(
                        "Claude CLI not found. Please install the Claude CLI and ensure it's in your PATH."
                    )))
                }
            }
        })
    }
}

impl ClaudeCodeProvider {
    pub fn new(cx: &mut App) -> Self {
        // Create a new entity model directly
        let state = cx.new(|cx| ClaudeCodeProviderState::new(cx));

        Self { state }
    }

    fn create_language_model(&self, model: Model, cx: &App) -> Arc<dyn LanguageModel> {
        let settings = AllLanguageModelSettings::get_global(cx);

        // Create a CLI runner from the settings
        let runner = claude_code::CliRunner::new(
            settings.claude_code.cli_path.clone(),
            settings.claude_code.default_args.clone(),
            settings.claude_code.timeout_ms,
        );

        Arc::new(ClaudeCodeModel::new(model, runner))
    }
}

impl LanguageModelProviderState for ClaudeCodeProvider {
    type ObservableEntity = ClaudeCodeProviderState;

    fn observable_entity(&self) -> Option<Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for ClaudeCodeProvider {
    fn id(&self) -> LanguageModelProviderId {
        LanguageModelProviderId(PROVIDER_ID.into())
    }

    fn name(&self) -> LanguageModelProviderName {
        LanguageModelProviderName(PROVIDER_NAME.into())
    }

    fn default_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        Some(self.create_language_model(Model::ClaudeCode, cx))
    }

    fn default_fast_model(&self, cx: &App) -> Option<Arc<dyn LanguageModel>> {
        Some(self.create_language_model(Model::ClaudeCode, cx))
    }

    fn provided_models(&self, cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        vec![self.create_language_model(Model::ClaudeCode, cx)]
    }

    fn is_authenticated(&self, cx: &App) -> bool {
        self.state.read(cx).is_authenticated()
    }

    fn authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>> {
        self.state.update(cx, |state, cx| state.authenticate(cx))
    }

    fn configuration_view(&self, _window: &mut Window, cx: &mut App) -> AnyView {
        cx.new(|_cx| ConfigurationView::new(self.state.clone()))
            .into()
    }

    fn reset_credentials(&self, cx: &mut App) -> Task<Result<()>> {
        let task = cx.background_executor().spawn(async {
            // Nothing to do for Claude Code CLI
            Ok(())
        });

        Task::from(task)
    }
}

// Configuration view implementation
struct ConfigurationView {
    state: Entity<ClaudeCodeProviderState>,
}

impl ConfigurationView {
    fn new(state: Entity<ClaudeCodeProviderState>) -> Self {
        Self { state }
    }
}

impl Render for ConfigurationView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let checking = state.checking_cli;

        v_flex()
            .gap_4()
            .p_4()
            .size_full()
            .child(
                div()
                    .child("Claude Code CLI Configuration")
            )
            .child(
                Label::new("To use Claude Code, you need to install the Claude CLI on your system.")
            )
            .child(
                List::new()
                    .child(
                        InstructionListItem::new(
                            "Install Claude CLI by following",
                            Some("the official documentation"),
                            Some("https://docs.anthropic.com/en/docs/claude-code/getting-started")
                        )
                    )
                    .child(
                        InstructionListItem::text_only("Provide the path to the Claude CLI below (or leave empty to use the system PATH)")
                    )
            )
            .child(
                h_flex()
                    .mt_2()
                    .gap_4()
                    .child(Label::new("Status:"))
                    .child(
                        if checking {
                            h_flex()
                                .gap_1()
                                .child(Icon::new(IconName::Rerun).color(Color::Muted))
                                .child(Label::new("Checking..."))
                        } else if state.is_authenticated() {
                            h_flex()
                                .gap_1()
                                .child(Icon::new(IconName::Check).color(Color::Success))
                                .child(Label::new("Claude CLI detected"))
                        } else {
                            h_flex()
                                .gap_1()
                                .child(Icon::new(IconName::XCircle).color(Color::Error))
                                .child(Label::new("Claude CLI not found"))
                        }
                    )
            )
            .when(state.cli_version.is_some(), |this| {
                this.child(
                    h_flex()
                        .gap_4()
                        .child(Label::new("Version:"))
                        .child(Label::new(state.cli_version.clone().unwrap_or_default())),
                )
            })
    }
}
