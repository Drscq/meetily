//! `ModelRunner` backed by local agent CLIs, tried in order (Codex first,
//! Claude Code as fallback).

use super::pipeline::ModelRunner;
use crate::summary::cli_agent::{run_prompt, CliAgentConfig};
use async_trait::async_trait;
use std::path::PathBuf;

pub struct CliFallbackRunner {
    configs: Vec<CliAgentConfig>,
    working_dir: PathBuf,
}

impl CliFallbackRunner {
    pub fn new(configs: Vec<CliAgentConfig>, working_dir: PathBuf) -> Self {
        Self { configs, working_dir }
    }
}

#[async_trait]
impl ModelRunner for CliFallbackRunner {
    async fn run(&self, prompt: &str) -> Result<String, String> {
        let mut failures = Vec::new();
        for config in &self.configs {
            match run_prompt(config, prompt, &self.working_dir).await {
                Ok(text) => return Ok(text),
                Err(e) => {
                    log::warn!("smart::cli_runner: {} failed: {}", config.display_name(), e);
                    failures.push(format!("{}: {}", config.display_name(), e));
                }
            }
        }
        if failures.is_empty() {
            return Err("No CLI agent configured for the smart pipeline".to_string());
        }
        Err(format!("All CLI agents failed. {}", failures.join(" | ")))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::summary::cli_agent::CliAgentPreset;

    fn custom(command: &str) -> CliAgentConfig {
        CliAgentConfig {
            preset: CliAgentPreset::Custom,
            command: command.into(),
            model: None,
            working_dir: None,
            timeout_secs: Some(20),
        }
    }

    #[tokio::test]
    async fn falls_back_to_next_cli_when_first_fails() {
        let runner = CliFallbackRunner::new(
            vec![custom("sh -c 'echo codex quota exceeded >&2; exit 2'"), custom("cat")],
            std::env::temp_dir(),
        );
        assert_eq!(runner.run("echo me").await.unwrap(), "echo me");
    }

    #[tokio::test]
    async fn reports_every_failure_when_all_clis_fail() {
        let runner = CliFallbackRunner::new(
            vec![
                custom("sh -c 'echo codex quota exceeded >&2; exit 2'"),
                custom("definitely-not-installed-cli-xyz"),
            ],
            std::env::temp_dir(),
        );
        let err = runner.run("p").await.unwrap_err();
        assert!(err.contains("codex quota exceeded"), "{err}");
        assert!(err.contains("definitely-not-installed-cli-xyz"), "{err}");
    }
}
