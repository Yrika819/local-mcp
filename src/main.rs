#![allow(
    unfulfilled_lint_expectations,
    reason = "Frozen item-scoped lint contracts are evaluated in both production and test targets."
)]

mod agent;
mod approvals;
mod atomic_publish_frame;
#[cfg(target_os = "linux")]
mod bubblewrap_support;
mod config;
mod exec_ready;
mod execution;
mod failure_class;
mod fallback;
mod git_command_class;
mod goal;
mod goal_api;
mod goal_backends;
mod goal_finalizer;
mod goal_runner;
mod goal_verifier;
mod managed_worktree;
mod managed_worktree_create;
mod managed_worktree_discovery;
mod managed_worktree_observe;
mod managed_worktree_prepare;
mod mcp;
mod mutation;
mod mutation_recovery;
mod orchestrator_error;
#[cfg(windows)]
mod pipe_security;
mod planner;
mod process_group;
mod readonly_worker;
mod replanner;
mod sandbox;
mod scheduler;
mod secure_fs;
mod task;
mod task_store;
mod verifier;
mod verifier_git_observation;
mod worker_capability;
mod workspace_publish;
mod writer;

#[cfg(test)]
mod agent_tests;
#[cfg(test)]
mod atomic_publish_tests;
#[cfg(test)]
mod goal_backends_tests;
#[cfg(test)]
mod goal_finalizer_tests;
#[cfg(test)]
mod goal_hardening_tests;
#[cfg(test)]
mod goal_runner_tests;
#[cfg(test)]
mod goal_verifier_tests;
#[cfg(test)]
mod managed_worktree_creation_tests;
#[cfg(test)]
mod managed_worktree_discovery_tests;
#[cfg(test)]
mod managed_worktree_tests;
#[cfg(test)]
mod phase0_config_tests;
#[cfg(test)]
mod phase0_fallback_tests;
#[cfg(test)]
mod phase0_tests;
#[cfg(test)]
mod phase11_goal_run_tests;
#[cfg(all(test, unix))]
mod process_group_ownership_tests;
#[cfg(all(test, unix))]
mod process_group_stress_tests;
#[cfg(test)]
mod replanner_compaction_tests;
#[cfg(test)]
mod replanner_replacement_tests;
#[cfg(all(test, unix))]
mod secure_fs_tests;
#[cfg(test)]
mod verifier_tests;
#[cfg(test)]
mod writer_stress_tests;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start a session in the current directory and show its permission UI.
    Start {
        /// Session ID to use instead of generating a UUID.
        session_id: Option<String>,
    },
    /// Run the session-independent MCP server over stdin/stdout.
    Mcp,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command.unwrap_or(Command::Start { session_id: None }) {
        Command::Start { session_id } => approvals::start(session_id.as_deref()).await,
        Command::Mcp => mcp::serve().await,
    }
}
