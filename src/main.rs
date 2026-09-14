mod approvals;
mod agent;
mod config;
mod execution;
mod fallback;
mod goal;
mod goal_finalizer;
mod goal_runner;
mod goal_verifier;
mod goal_api;
mod goal_backends;
mod mcp;
mod orchestrator_error;
mod planner;
mod replanner;
mod sandbox;
mod scheduler;
mod task;
mod task_store;
mod verifier;
mod writer;

#[cfg(test)]
mod phase0_tests;
#[cfg(test)]
mod phase0_config_tests;
#[cfg(test)]
mod phase0_fallback_tests;
#[cfg(test)]
mod verifier_tests;
#[cfg(test)]
mod goal_finalizer_tests;
#[cfg(test)]
mod goal_verifier_tests;
#[cfg(test)]
mod goal_runner_tests;
#[cfg(test)]
mod agent_tests;
#[cfg(test)]
mod goal_backends_tests;
#[cfg(test)]
mod phase11_goal_run_tests;

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
