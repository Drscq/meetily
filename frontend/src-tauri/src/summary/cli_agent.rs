//! Local CLI agent summarizer.
//!
//! Lets the summary pipeline drive a locally installed, subscription-authenticated
//! coding agent CLI (Claude Code, OpenAI Codex, Gemini CLI, or any custom command)
//! instead of calling a cloud HTTP API with an API key. The CLI is spawned as a child
//! process, the prompt is delivered on stdin (or via `{prompt}` / `{prompt_file}`
//! placeholders), and its text output becomes the model completion.
//!
//! Nothing about authentication is handled here: the user logs in with the CLI itself
//! (`claude login`, `codex login`, `gemini`) and the CLI reuses that session.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tauri::{AppHandle, Manager, Runtime};
use tauri_plugin_store::StoreExt;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// Tauri store file that holds the CLI agent configuration.
const STORE_FILE: &str = "cli_agent_config.json";
const STORE_KEY: &str = "config";

/// Provider id used in the `settings.provider` column and by the frontend.
pub const PROVIDER_ID: &str = "cli-agent";

const DEFAULT_TIMEOUT_SECS: u64 = 600;
const MIN_TIMEOUT_SECS: u64 = 15;
const MAX_TIMEOUT_SECS: u64 = 3600;
const TEST_TIMEOUT_SECS: u64 = 180;

/// How the agent is invoked. Presets only pre-fill the command line and decide which flag
/// carries the model name; the command stays fully editable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CliAgentPreset {
    /// Anthropic Claude Code CLI (`claude`).
    ClaudeCode,
    /// OpenAI Codex CLI (`codex`).
    Codex,
    /// Google Gemini CLI (`gemini`).
    Gemini,
    /// Anything else, e.g. a wrapper script around another agent.
    Custom,
}

impl Default for CliAgentPreset {
    fn default() -> Self {
        Self::ClaudeCode
    }
}

impl CliAgentPreset {
    pub fn id(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Custom => "custom",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code CLI",
            Self::Codex => "Codex CLI",
            Self::Gemini => "Gemini CLI",
            Self::Custom => "Custom command",
        }
    }

    /// Command line used when the preset is selected. Placeholders:
    /// `{output_file}` – a temp file the CLI should write its final answer to;
    /// `{prompt_file}` – a temp file containing the full prompt;
    /// `{prompt}` – the full prompt as one argument;
    /// `{model}` – the configured model name.
    /// Without `{prompt}` / `{prompt_file}` the prompt is piped to stdin.
    pub fn default_command(self) -> &'static str {
        match self {
            // Non-interactive print mode, plain text on stdout, prompt on stdin.
            Self::ClaudeCode => "claude -p --output-format text",
            // Non-interactive exec, read-only sandbox, final message written to a file so
            // progress output on stdout/stderr never pollutes the summary.
            Self::Codex => "codex exec --skip-git-repo-check -s read-only -o {output_file}",
            // Piped stdin puts Gemini CLI into non-interactive mode; text on stdout.
            Self::Gemini => "gemini",
            Self::Custom => "",
        }
    }

    /// Flag used to pass the model when the command has no `{model}` placeholder.
    fn model_flag(self) -> Option<&'static str> {
        match self {
            Self::ClaudeCode => Some("--model"),
            Self::Codex => Some("-m"),
            Self::Gemini => Some("-m"),
            Self::Custom => None,
        }
    }

    pub fn notes(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Requires the Claude Code CLI to be installed and logged in (`claude` then `/login`, or `claude login`). Uses your Claude subscription; no API key needed.",
            Self::Codex => "Requires the OpenAI Codex CLI to be installed and logged in (`codex login`). Uses your ChatGPT subscription; no API key needed.",
            Self::Gemini => "Requires the Gemini CLI to be installed and logged in (run `gemini` once and sign in with Google). Uses your Google account quota.",
            Self::Custom => "Any executable that reads a prompt from stdin (or via {prompt} / {prompt_file}) and prints the answer to stdout (or writes it to {output_file}). Use this for other agent CLIs such as Antigravity if they offer a non-interactive mode.",
        }
    }

    pub fn all() -> [CliAgentPreset; 4] {
        [Self::ClaudeCode, Self::Codex, Self::Gemini, Self::Custom]
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::all().into_iter().find(|p| p.id() == id)
    }
}

/// Persisted configuration for the CLI agent provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CliAgentConfig {
    #[serde(default)]
    pub preset: CliAgentPreset,
    /// Full command line (see `CliAgentPreset::default_command` for placeholders).
    #[serde(default)]
    pub command: String,
    /// Optional model override passed to the CLI.
    #[serde(default)]
    pub model: Option<String>,
    /// Working directory for the child process. Defaults to a scratch folder inside the
    /// app data dir so project-level agent instructions (CLAUDE.md, AGENTS.md, ...) are
    /// never picked up.
    #[serde(default)]
    pub working_dir: Option<String>,
    /// Per-request timeout in seconds.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

impl Default for CliAgentConfig {
    fn default() -> Self {
        Self {
            preset: CliAgentPreset::ClaudeCode,
            command: CliAgentPreset::ClaudeCode.default_command().to_string(),
            model: None,
            working_dir: None,
            timeout_secs: Some(DEFAULT_TIMEOUT_SECS),
        }
    }
}

impl CliAgentConfig {
    /// Validate the configuration, normalising whitespace. Returns a user-facing error.
    pub fn validated(mut self) -> Result<Self, String> {
        self.command = self.command.trim().to_string();
        if self.command.is_empty() {
            return Err("CLI command is required".to_string());
        }
        let args = split_command(&self.command)?;
        if args.is_empty() {
            return Err("CLI command is required".to_string());
        }
        self.model = self.model.map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
        self.working_dir = self
            .working_dir
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty());
        if let Some(dir) = &self.working_dir {
            let path = expand_home(Path::new(dir));
            if !path.is_dir() {
                return Err(format!("Working directory does not exist: {}", path.display()));
            }
        }
        if let Some(timeout) = self.timeout_secs {
            if !(MIN_TIMEOUT_SECS..=MAX_TIMEOUT_SECS).contains(&timeout) {
                return Err(format!(
                    "Timeout must be between {} and {} seconds",
                    MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS
                ));
            }
        }
        Ok(self)
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS))
    }

    /// Human readable description for logs and the UI model column.
    pub fn display_name(&self) -> String {
        let exe = split_command(&self.command)
            .ok()
            .and_then(|args| args.first().cloned())
            .unwrap_or_else(|| self.preset.label().to_string());
        match &self.model {
            Some(model) => format!("{} ({})", exe, model),
            None => exe,
        }
    }
}

/// Description of a preset for the settings UI.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliAgentPresetInfo {
    pub id: &'static str,
    pub label: &'static str,
    pub default_command: &'static str,
    pub notes: &'static str,
}

/// Result of a connectivity test.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CliAgentTestResult {
    pub status: &'static str,
    pub message: String,
    pub resolved_executable: String,
    pub output: String,
    pub duration_ms: u128,
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

pub async fn load_config<R: Runtime>(app: &AppHandle<R>) -> Result<Option<CliAgentConfig>, String> {
    let store = app
        .store(STORE_FILE)
        .map_err(|e| format!("Failed to open CLI agent config store: {}", e))?;
    match store.get(STORE_KEY) {
        Some(value) => serde_json::from_value::<CliAgentConfig>(value.clone())
            .map(Some)
            .map_err(|e| format!("Stored CLI agent config is invalid: {}", e)),
        None => Ok(None),
    }
}

pub async fn save_config<R: Runtime>(app: &AppHandle<R>, config: &CliAgentConfig) -> Result<(), String> {
    let store = app
        .store(STORE_FILE)
        .map_err(|e| format!("Failed to open CLI agent config store: {}", e))?;
    let value = serde_json::to_value(config).map_err(|e| format!("Failed to serialise config: {}", e))?;
    store.set(STORE_KEY, value);
    store
        .save()
        .map_err(|e| format!("Failed to persist CLI agent config: {}", e))
}

/// Scratch working directory used when the user did not pick one.
pub fn default_working_dir<R: Runtime>(app: &AppHandle<R>) -> Option<PathBuf> {
    let dir = app.path().app_data_dir().ok()?.join("cli-agent-workspace");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        warn!("Failed to create CLI agent workspace {:?}: {}", dir, e);
        return None;
    }
    Some(dir)
}

// ---------------------------------------------------------------------------
// Command preparation
// ---------------------------------------------------------------------------

/// Split a command line into arguments using POSIX shell quoting rules.
pub fn split_command(command: &str) -> Result<Vec<String>, String> {
    shlex::split(command.trim()).ok_or_else(|| {
        "CLI command has unbalanced quotes or a trailing backslash".to_string()
    })
}

/// Combine the system and user prompts into one message, since CLIs take a single prompt.
pub fn combine_prompts(system_prompt: &str, user_prompt: &str) -> String {
    let system = system_prompt.trim();
    let user = user_prompt.trim();
    if system.is_empty() {
        return user.to_string();
    }
    format!(
        "{system}\n\nRespond with the requested content only. Do not run tools, do not inspect or modify files, and do not add commentary before or after the answer.\n\n---\n\n{user}"
    )
}

#[derive(Debug, Default)]
struct PreparedInvocation {
    args: Vec<String>,
    prompt_on_stdin: bool,
    output_file: Option<PathBuf>,
    prompt_file: Option<PathBuf>,
}

/// Expand placeholders and decide how the prompt is delivered.
fn prepare_invocation(
    config: &CliAgentConfig,
    prompt: &str,
    scratch_dir: &Path,
) -> Result<PreparedInvocation, String> {
    let mut args = split_command(&config.command)?;
    if args.is_empty() {
        return Err("CLI command is required".to_string());
    }

    let has_model_placeholder = args.iter().any(|a| a.contains("{model}"));
    let uses_prompt_arg = args.iter().any(|a| a.contains("{prompt}"));
    let uses_prompt_file = args.iter().any(|a| a.contains("{prompt_file}"));
    let uses_output_file = args.iter().any(|a| a.contains("{output_file}"));

    let unique = format!("{}-{}", std::process::id(), chrono::Utc::now().timestamp_micros());
    let output_file = uses_output_file.then(|| scratch_dir.join(format!("meetily-cli-output-{unique}.md")));
    let prompt_file = uses_prompt_file.then(|| scratch_dir.join(format!("meetily-cli-prompt-{unique}.md")));

    if let Some(path) = &prompt_file {
        std::fs::write(path, prompt).map_err(|e| format!("Failed to write prompt file: {}", e))?;
    }

    let model = config.model.clone().unwrap_or_default();
    for arg in &mut args {
        if arg.contains("{model}") {
            *arg = arg.replace("{model}", &model);
        }
        if let Some(path) = &output_file {
            *arg = arg.replace("{output_file}", &path.to_string_lossy());
        }
        if let Some(path) = &prompt_file {
            *arg = arg.replace("{prompt_file}", &path.to_string_lossy());
        }
        if arg.contains("{prompt}") {
            *arg = arg.replace("{prompt}", prompt);
        }
    }

    // Drop arguments that became empty because `{model}` was unset, together with the
    // flag in front of them (`--model {model}` → nothing).
    if model.is_empty() && has_model_placeholder {
        let mut pruned: Vec<String> = Vec::with_capacity(args.len());
        for arg in args.iter() {
            if arg.is_empty() {
                if pruned.len() > 1 && pruned.last().is_some_and(|prev| prev.starts_with('-')) {
                    pruned.pop();
                }
                continue;
            }
            pruned.push(arg.clone());
        }
        args = pruned;
    }

    if !model.is_empty() && !has_model_placeholder {
        if let Some(flag) = config.preset.model_flag() {
            args.push(flag.to_string());
            args.push(model);
        }
    }

    Ok(PreparedInvocation {
        args,
        prompt_on_stdin: !uses_prompt_arg && !uses_prompt_file,
        output_file,
        prompt_file,
    })
}

// ---------------------------------------------------------------------------
// Executable resolution (GUI apps do not inherit the shell PATH)
// ---------------------------------------------------------------------------

fn expand_home(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    } else if s == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    path.to_path_buf()
}

/// Directories where node/npm/bun/homebrew-installed CLIs commonly live, in addition to
/// whatever PATH the app process inherited.
fn candidate_bin_dirs() -> Vec<PathBuf> {
    let mut dirs_out: Vec<PathBuf> = Vec::new();
    let home = dirs::home_dir();

    if let Some(home) = &home {
        for rel in [
            ".local/bin",
            ".npm-global/bin",
            ".npm/bin",
            ".bun/bin",
            ".volta/bin",
            ".cargo/bin",
            ".claude/local",
            ".claude/local/bin",
            ".codex/bin",
            ".yarn/bin",
            ".config/yarn/global/node_modules/.bin",
            ".local/share/pnpm",
            "Library/pnpm",
            ".antigravity/bin",
            "bin",
        ] {
            dirs_out.push(home.join(rel));
        }
        // nvm / fnm keep one bin dir per installed Node version.
        for versions_root in [home.join(".nvm/versions/node"), home.join(".fnm/node-versions")] {
            if let Ok(entries) = std::fs::read_dir(&versions_root) {
                let mut versions: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
                versions.sort();
                versions.reverse(); // newest first
                for v in versions {
                    dirs_out.push(v.join("bin"));
                    dirs_out.push(v.join("installation").join("bin"));
                }
            }
        }
    }

    for abs in [
        "/opt/homebrew/bin",
        "/opt/homebrew/opt/node/bin",
        "/usr/local/bin",
        "/usr/local/opt/node/bin",
        "/usr/bin",
        "/bin",
        "/snap/bin",
    ] {
        dirs_out.push(PathBuf::from(abs));
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            dirs_out.push(PathBuf::from(&appdata).join("npm"));
        }
        if let Ok(local) = std::env::var("LOCALAPPDATA") {
            dirs_out.push(PathBuf::from(&local).join("Programs").join("nodejs"));
            dirs_out.push(PathBuf::from(&local).join("pnpm"));
        }
        if let Ok(pf) = std::env::var("ProgramFiles") {
            dirs_out.push(PathBuf::from(&pf).join("nodejs"));
        }
        if let Some(home) = &home {
            dirs_out.push(home.join(".bun").join("bin"));
            dirs_out.push(home.join("scoop").join("shims"));
        }
    }

    dirs_out.into_iter().filter(|d| d.is_dir()).collect()
}

/// PATH value for the child process: inherited PATH plus the candidate dirs. Node-based
/// CLIs (`#!/usr/bin/env node`) need `node` itself to be discoverable, so this is passed
/// to the child even when the executable was resolved by absolute path.
fn augmented_path() -> String {
    let sep = if cfg!(target_os = "windows") { ";" } else { ":" };
    let mut seen = BTreeSet::new();
    let mut parts: Vec<String> = Vec::new();

    let existing = std::env::var("PATH").unwrap_or_default();
    for p in existing.split(sep).filter(|p| !p.is_empty()) {
        if seen.insert(p.to_string()) {
            parts.push(p.to_string());
        }
    }
    for dir in candidate_bin_dirs() {
        let s = dir.to_string_lossy().to_string();
        if seen.insert(s.clone()) {
            parts.push(s);
        }
    }
    parts.join(sep)
}

/// Ask the user's login shell where a command lives. Catches PATH entries added by
/// `.zshrc`/`.bashrc` that no heuristic list would know about.
#[cfg(not(target_os = "windows"))]
async fn resolve_via_login_shell(name: &str) -> Option<PathBuf> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let script = format!("command -v {}", shlex::try_quote(name).ok()?);
    let output = tokio::time::timeout(
        Duration::from_secs(8),
        tokio::process::Command::new(&shell)
            .arg("-lc")
            .arg(&script)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output(),
    )
    .await
    .ok()?
    .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().rev().find(|l| !l.trim().is_empty())?.trim();
    let path = PathBuf::from(line);
    path.is_file().then_some(path)
}

#[cfg(target_os = "windows")]
async fn resolve_via_login_shell(_name: &str) -> Option<PathBuf> {
    None
}

/// Resolve the executable named by the first argument to an absolute path.
pub async fn resolve_executable(name: &str, cwd: &Path) -> Result<PathBuf, String> {
    let as_path = expand_home(Path::new(name));
    if as_path.components().count() > 1 || as_path.is_absolute() {
        if as_path.is_file() {
            return Ok(as_path);
        }
        return Err(format!("CLI executable not found at {}", as_path.display()));
    }

    let path_env = augmented_path();
    if let Ok(found) = which::which_in(name, Some(&path_env), cwd) {
        return Ok(found);
    }
    if let Some(found) = resolve_via_login_shell(name).await {
        return Ok(found);
    }

    Err(format!(
        "Could not find `{name}` on PATH. Make sure the CLI is installed and works in a terminal, \
         or put its absolute path in the command (e.g. /opt/homebrew/bin/{name}). \
         Searched: {}",
        path_env
    ))
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct RunOutcome {
    text: String,
    executable: PathBuf,
}

fn tail(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let skip = trimmed.chars().count() - max_chars;
    format!("…{}", trimmed.chars().skip(skip).collect::<String>())
}

async fn run_once(
    config: &CliAgentConfig,
    prompt: &str,
    working_dir: &Path,
    timeout: Duration,
    cancellation_token: Option<&CancellationToken>,
) -> Result<RunOutcome, String> {
    if cancellation_token.is_some_and(CancellationToken::is_cancelled) {
        return Err("Summary generation was cancelled".to_string());
    }

    let scratch_dir = std::env::temp_dir();
    let prepared = prepare_invocation(config, prompt, &scratch_dir)?;
    let (program, rest) = prepared
        .args
        .split_first()
        .ok_or_else(|| "CLI command is required".to_string())?;
    let executable = resolve_executable(program, working_dir).await?;

    info!(
        "Running CLI agent {:?} ({} args, prompt {} chars, stdin={}, cwd={})",
        executable,
        rest.len(),
        prompt.len(),
        prepared.prompt_on_stdin,
        working_dir.display()
    );

    let mut command = tokio::process::Command::new(&executable);
    command
        .args(rest)
        .current_dir(working_dir)
        .env("PATH", augmented_path())
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("CI", "1")
        // Claude Code refuses to start when it believes it is nested inside another
        // Claude Code session; make sure stray env never trips that check.
        .env_remove("CLAUDECODE")
        .env_remove("CLAUDE_CODE_ENTRYPOINT")
        .stdin(if prepared.prompt_on_stdin { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to start {}: {}", executable.display(), e))?;

    if prepared.prompt_on_stdin {
        if let Some(mut stdin) = child.stdin.take() {
            let payload = prompt.to_string();
            // Write in a task so a CLI that never reads stdin cannot deadlock us.
            tokio::spawn(async move {
                if let Err(e) = stdin.write_all(payload.as_bytes()).await {
                    warn!("Failed to write prompt to CLI agent stdin: {}", e);
                }
                let _ = stdin.shutdown().await;
            });
        }
    }

    let wait = child.wait_with_output();
    let output = if let Some(token) = cancellation_token {
        tokio::select! {
            biased;
            _ = token.cancelled() => {
                cleanup_temp_files(&prepared);
                return Err("Summary generation was cancelled".to_string());
            }
            result = tokio::time::timeout(timeout, wait) => result,
        }
    } else {
        tokio::time::timeout(timeout, wait).await
    };

    let output = match output {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            cleanup_temp_files(&prepared);
            return Err(format!("CLI agent failed to run: {}", e));
        }
        Err(_) => {
            cleanup_temp_files(&prepared);
            return Err(format!(
                "CLI agent timed out after {} seconds. Increase the timeout in Settings or use a faster model.",
                timeout.as_secs()
            ));
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();

    let file_text = prepared
        .output_file
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    cleanup_temp_files(&prepared);

    if !output.status.success() {
        let detail = if !stderr.trim().is_empty() { tail(&stderr, 1200) } else { tail(&stdout, 1200) };
        return Err(format!(
            "CLI agent exited with {}{}",
            output.status,
            if detail.is_empty() { String::new() } else { format!(": {}", detail) }
        ));
    }

    let text = match file_text {
        Some(text) => text,
        None => stdout.trim().to_string(),
    };

    if text.is_empty() {
        let detail = tail(&stderr, 1200);
        return Err(format!(
            "CLI agent returned no output{}",
            if detail.is_empty() { String::new() } else { format!(". stderr: {}", detail) }
        ));
    }

    if !stderr.trim().is_empty() {
        info!("CLI agent stderr (ignored): {}", tail(&stderr, 400));
    }

    Ok(RunOutcome { text, executable })
}

fn cleanup_temp_files(prepared: &PreparedInvocation) {
    for path in [&prepared.output_file, &prepared.prompt_file].into_iter().flatten() {
        let _ = std::fs::remove_file(path);
    }
}

/// Generate a completion through the configured CLI. This is what the summary pipeline
/// calls for every LLM request when the provider is `cli-agent`.
pub async fn generate_with_cli(
    config: &CliAgentConfig,
    system_prompt: &str,
    user_prompt: &str,
    app_data_dir: Option<&PathBuf>,
    cancellation_token: Option<&CancellationToken>,
) -> Result<String, String> {
    let prompt = combine_prompts(system_prompt, user_prompt);
    let working_dir = resolve_working_dir(config, app_data_dir);
    let outcome = run_once(config, &prompt, &working_dir, config.timeout(), cancellation_token).await?;
    Ok(outcome.text)
}

fn resolve_working_dir(config: &CliAgentConfig, app_data_dir: Option<&PathBuf>) -> PathBuf {
    if let Some(dir) = &config.working_dir {
        let path = expand_home(Path::new(dir));
        if path.is_dir() {
            return path;
        }
        warn!("Configured CLI agent working dir {:?} is missing; falling back", path);
    }
    if let Some(app_dir) = app_data_dir {
        let dir = app_dir.join("cli-agent-workspace");
        if std::fs::create_dir_all(&dir).is_ok() {
            return dir;
        }
    }
    std::env::temp_dir()
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn api_get_cli_agent_presets() -> Result<Vec<CliAgentPresetInfo>, String> {
    Ok(CliAgentPreset::all()
        .into_iter()
        .map(|p| CliAgentPresetInfo {
            id: p.id(),
            label: p.label(),
            default_command: p.default_command(),
            notes: p.notes(),
        })
        .collect())
}

#[tauri::command]
pub async fn api_get_cli_agent_config<R: Runtime>(
    app: AppHandle<R>,
) -> Result<Option<CliAgentConfig>, String> {
    load_config(&app).await
}

#[tauri::command]
pub async fn api_save_cli_agent_config<R: Runtime>(
    app: AppHandle<R>,
    config: CliAgentConfig,
) -> Result<CliAgentConfig, String> {
    let config = config.validated()?;
    save_config(&app, &config).await?;
    info!("Saved CLI agent config: preset={}, command={:?}, model={:?}", config.preset.id(), config.command, config.model);
    Ok(config)
}

/// Run the CLI with a tiny prompt to confirm it is installed, authenticated and answers.
#[tauri::command]
pub async fn api_test_cli_agent<R: Runtime>(
    app: AppHandle<R>,
    config: CliAgentConfig,
) -> Result<CliAgentTestResult, String> {
    let config = config.validated()?;
    let app_data_dir = app.path().app_data_dir().ok();
    let working_dir = resolve_working_dir(&config, app_data_dir.as_ref());
    let prompt = combine_prompts(
        "You are a connectivity check.",
        "Reply with exactly the two words: MEETILY OK",
    );
    let started = std::time::Instant::now();
    let outcome = run_once(
        &config,
        &prompt,
        &working_dir,
        Duration::from_secs(TEST_TIMEOUT_SECS.min(config.timeout().as_secs().max(MIN_TIMEOUT_SECS))),
        None,
    )
    .await?;
    let duration_ms = started.elapsed().as_millis();
    let ok = outcome.text.to_ascii_uppercase().contains("MEETILY OK");
    Ok(CliAgentTestResult {
        status: if ok { "success" } else { "warning" },
        message: if ok {
            format!("{} answered in {:.1}s", outcome.executable.display(), duration_ms as f64 / 1000.0)
        } else {
            format!(
                "{} ran, but the reply did not contain the expected text. Check the output below.",
                outcome.executable.display()
            )
        },
        resolved_executable: outcome.executable.to_string_lossy().to_string(),
        output: tail(&outcome.text, 600),
        duration_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(preset: CliAgentPreset, command: &str, model: Option<&str>) -> CliAgentConfig {
        CliAgentConfig {
            preset,
            command: command.to_string(),
            model: model.map(str::to_string),
            working_dir: None,
            timeout_secs: Some(60),
        }
    }

    #[test]
    fn claude_default_pipes_prompt_on_stdin_and_appends_model() {
        let cfg = config(CliAgentPreset::ClaudeCode, CliAgentPreset::ClaudeCode.default_command(), Some("sonnet"));
        let prepared = prepare_invocation(&cfg, "hello", &std::env::temp_dir()).unwrap();
        assert_eq!(prepared.args, vec!["claude", "-p", "--output-format", "text", "--model", "sonnet"]);
        assert!(prepared.prompt_on_stdin);
        assert!(prepared.output_file.is_none());
        assert!(prepared.prompt_file.is_none());
    }

    #[test]
    fn claude_without_model_adds_no_flag() {
        let cfg = config(CliAgentPreset::ClaudeCode, CliAgentPreset::ClaudeCode.default_command(), None);
        let prepared = prepare_invocation(&cfg, "hello", &std::env::temp_dir()).unwrap();
        assert_eq!(prepared.args, vec!["claude", "-p", "--output-format", "text"]);
    }

    #[test]
    fn codex_default_uses_output_file_placeholder() {
        let cfg = config(CliAgentPreset::Codex, CliAgentPreset::Codex.default_command(), Some("o4-mini"));
        let prepared = prepare_invocation(&cfg, "hello", &std::env::temp_dir()).unwrap();
        let output_file = prepared.output_file.clone().expect("output file");
        assert_eq!(
            prepared.args,
            vec![
                "codex", "exec", "--skip-git-repo-check", "-s", "read-only", "-o",
                output_file.to_string_lossy().as_ref(), "-m", "o4-mini"
            ]
        );
        assert!(prepared.prompt_on_stdin);
    }

    #[test]
    fn prompt_placeholder_disables_stdin_and_keeps_prompt_as_single_arg() {
        let cfg = config(CliAgentPreset::Custom, "gemini -p {prompt}", None);
        let prepared = prepare_invocation(&cfg, "two words", &std::env::temp_dir()).unwrap();
        assert_eq!(prepared.args, vec!["gemini", "-p", "two words"]);
        assert!(!prepared.prompt_on_stdin);
    }

    #[test]
    fn prompt_file_placeholder_writes_prompt_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config(CliAgentPreset::Custom, "my-agent --file {prompt_file}", None);
        let prepared = prepare_invocation(&cfg, "saved prompt", dir.path()).unwrap();
        let path = prepared.prompt_file.clone().expect("prompt file");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "saved prompt");
        assert_eq!(prepared.args[2], path.to_string_lossy());
        assert!(!prepared.prompt_on_stdin);
        cleanup_temp_files(&prepared);
        assert!(!path.exists());
    }

    #[test]
    fn model_placeholder_is_substituted_or_pruned() {
        let with_model = config(CliAgentPreset::Custom, "agent --model {model} run", Some("x1"));
        let prepared = prepare_invocation(&with_model, "p", &std::env::temp_dir()).unwrap();
        assert_eq!(prepared.args, vec!["agent", "--model", "x1", "run"]);

        let without_model = config(CliAgentPreset::Custom, "agent --model {model} run", None);
        let prepared = prepare_invocation(&without_model, "p", &std::env::temp_dir()).unwrap();
        assert_eq!(prepared.args, vec!["agent", "run"]);
    }

    #[test]
    fn quoted_arguments_survive_splitting() {
        let args = split_command(r#"claude -p --append-system-prompt "be brief" --output-format text"#).unwrap();
        assert_eq!(args[3], "be brief");
        assert!(split_command(r#"claude "unbalanced"#).is_err());
    }

    #[test]
    fn validation_rejects_empty_command_and_bad_timeout() {
        assert!(config(CliAgentPreset::Custom, "   ", None).validated().is_err());
        let mut cfg = config(CliAgentPreset::ClaudeCode, "claude -p", None);
        cfg.timeout_secs = Some(1);
        assert!(cfg.validated().is_err());
        let ok = config(CliAgentPreset::ClaudeCode, "  claude -p  ", Some("  ")).validated().unwrap();
        assert_eq!(ok.command, "claude -p");
        assert_eq!(ok.model, None);
    }

    #[test]
    fn combined_prompt_keeps_both_parts() {
        let combined = combine_prompts("SYSTEM", "USER");
        assert!(combined.starts_with("SYSTEM"));
        assert!(combined.ends_with("USER"));
        assert_eq!(combine_prompts("  ", "only user"), "only user");
    }

    #[test]
    fn preset_round_trips_through_serde_ids() {
        for preset in CliAgentPreset::all() {
            let json = serde_json::to_string(&preset).unwrap();
            assert_eq!(json, format!("\"{}\"", preset.id()));
            assert_eq!(CliAgentPreset::from_id(preset.id()), Some(preset));
        }
        let legacy: CliAgentConfig = serde_json::from_str(r#"{"command":"claude -p"}"#).unwrap();
        assert_eq!(legacy.preset, CliAgentPreset::ClaudeCode);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runs_a_real_process_and_captures_stdout() {
        let cfg = config(CliAgentPreset::Custom, "cat", None);
        let out = run_once(&cfg, "echo back", &std::env::temp_dir(), Duration::from_secs(10), None)
            .await
            .unwrap();
        assert_eq!(out.text, "echo back");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn nonzero_exit_is_reported_with_stderr() {
        let cfg = config(CliAgentPreset::Custom, "sh -c 'echo boom >&2; exit 3'", None);
        let err = run_once(&cfg, "p", &std::env::temp_dir(), Duration::from_secs(10), None)
            .await
            .unwrap_err();
        assert!(err.contains("boom"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_file_wins_over_stdout() {
        let cfg = config(CliAgentPreset::Custom, "sh -c 'cat >/dev/null; echo noise; echo FINAL > \"$0\"' {output_file}", None);
        let out = run_once(&cfg, "p", &std::env::temp_dir(), Duration::from_secs(10), None)
            .await
            .unwrap();
        assert_eq!(out.text, "FINAL");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_slow_process() {
        let cfg = config(CliAgentPreset::Custom, "sleep 30", None);
        let err = run_once(&cfg, "p", &std::env::temp_dir(), Duration::from_millis(300), None)
            .await
            .unwrap_err();
        assert!(err.contains("timed out"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancellation_stops_waiting() {
        let cfg = config(CliAgentPreset::Custom, "sleep 30", None);
        let token = CancellationToken::new();
        let token_clone = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            token_clone.cancel();
        });
        let err = run_once(&cfg, "p", &std::env::temp_dir(), Duration::from_secs(30), Some(&token))
            .await
            .unwrap_err();
        assert_eq!(err, "Summary generation was cancelled");
    }

    #[tokio::test]
    async fn missing_executable_gives_actionable_error() {
        let err = resolve_executable("definitely-not-a-real-cli-xyz", &std::env::temp_dir())
            .await
            .unwrap_err();
        assert!(err.contains("Could not find"), "{err}");
    }
}
