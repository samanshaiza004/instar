//! `youth` CLI: validate, mount, activate, script, inspect.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;
use youth_runtime::{
    AppFault, AppInspection, RuntimeError, RuntimeErrorCategory, TurnReceipt, YouthAppHandle,
};
use youth_state::{AppId, StateLimits, StateLocation, StateStore, repair_usage, verify_file};
use youth_tree::NodeId;

/// Inspection and test surface for the Youth runtime.
#[derive(Debug, Parser)]
#[command(name = "youth", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Check that a component loads and exports the Youth world. Does not mount.
    Validate { component: PathBuf },
    /// Mount a component and print its initial canonical tree.
    Mount { component: PathBuf },
    /// Mount, deliver one activation, and print the resulting tree.
    Activate {
        component: PathBuf,
        #[arg(long)]
        node: u64,
    },
    /// Replay a deterministic event script and print one canonical snapshot.
    Script { component: PathBuf, events: PathBuf },
    /// Mount and report full application state.
    Inspect {
        component: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Inspect or maintain host-owned application state.
    State {
        #[command(subcommand)]
        command: StateCommand,
    },
}

#[derive(Debug, Subcommand)]
enum StateCommand {
    /// Print a state database summary without values.
    Inspect(StateTarget),
    /// Clear application state while preserving the schema.
    Reset {
        #[command(flatten)]
        target: StateTarget,
        /// Confirm the destructive operation.
        #[arg(long)]
        yes: bool,
    },
    /// Verify schema, integrity, typed rows, and derived usage.
    Verify(StateTarget),
    /// Repair derived usage after creating an explicit backup.
    RepairUsage {
        #[command(flatten)]
        target: StateTarget,
        #[arg(long)]
        backup: PathBuf,
        /// Confirm the maintenance operation.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, clap::Args)]
struct StateTarget {
    #[arg(long)]
    app_id: AppId,
    #[arg(long)]
    state_dir: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    init_tracing();
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn init_tracing() {
    let Ok(filter) = EnvFilter::try_from_default_env() else {
        return;
    };
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

async fn run(cli: Cli) -> Result<(), CliError> {
    match cli.command {
        Command::Validate { component } => {
            let app = YouthAppHandle::spawn_ephemeral(&component).map_err(CliError::Runtime)?;
            let inspection = app.inspect().await.map_err(CliError::Runtime)?;
            println!(
                "validated: {} ({})",
                component_name(&component),
                inspection.world
            );
        }
        Command::Mount { component } => {
            let app = YouthAppHandle::spawn_ephemeral(&component).map_err(CliError::Runtime)?;
            app.mount().await.map_err(CliError::Runtime)?;
            let inspection = app.inspect().await.map_err(CliError::Runtime)?;
            println!("component: {}", component_name(&component));
            println!("world: {}", inspection.world);
            println!("lifecycle: {}", inspection.lifecycle);
            println!("revision: {}", inspection.current_revision.unwrap_or(0));
            println!("nodes: {}", inspection.node_count);
            println!("depth: {}", inspection.depth);
            println!();
            print!("{}", inspection.canonical_tree);
        }
        Command::Activate { component, node } => {
            let node = parse_node_id(node)?;
            let app = YouthAppHandle::spawn_ephemeral(&component).map_err(CliError::Runtime)?;
            app.mount().await.map_err(CliError::Runtime)?;
            match app.activate(node).await {
                Ok(receipt) => {
                    print_receipt(&receipt, "committed");
                    let inspection = app.inspect().await.map_err(CliError::Runtime)?;
                    println!();
                    print!("{}", inspection.canonical_tree);
                }
                Err(error) if error.category() == RuntimeErrorCategory::GuestRejected => {
                    let inspection = app.inspect().await.map_err(CliError::Runtime)?;
                    if let Some(receipt) = inspection.last_turn {
                        print_receipt(
                            &receipt,
                            &format!("rejected ({})", category_name(error.category())),
                        );
                    } else {
                        println!("status: rejected ({})", category_name(error.category()));
                    }
                    return Err(CliError::Runtime(error));
                }
                Err(error) => return Err(CliError::Runtime(error)),
            }
        }
        Command::Script { component, events } => {
            let nodes = read_script(&events)?;
            let app = YouthAppHandle::spawn_ephemeral(&component).map_err(CliError::Runtime)?;
            app.mount().await.map_err(CliError::Runtime)?;
            for node in nodes {
                app.activate(node).await.map_err(CliError::Runtime)?;
            }
            let inspection = app.inspect().await.map_err(CliError::Runtime)?;
            print!("{}", inspection.canonical_tree);
        }
        Command::Inspect { component, json } => {
            let app = YouthAppHandle::spawn_ephemeral(&component).map_err(CliError::Runtime)?;
            app.mount().await.map_err(CliError::Runtime)?;
            let inspection = app.inspect().await.map_err(CliError::Runtime)?;
            if json {
                println!("{}", inspection_json(&inspection));
            } else {
                print_inspection(&inspection);
            }
        }
        Command::State { command } => run_state_command(command)?,
    }
    Ok(())
}

fn run_state_command(command: StateCommand) -> Result<(), CliError> {
    match command {
        StateCommand::Inspect(target) => {
            let path = state_path(&target);
            let store = StateStore::open(StateLocation::File(path), StateLimits::default())
                .map_err(CliError::State)?;
            let summary = store.summary().map_err(CliError::State)?;
            println!("app: {}", target.app_id);
            println!("schema: {}", summary.schema_version);
            println!("keys: {}", summary.key_count);
            println!("bytes: {}", summary.logical_bytes);
        }
        StateCommand::Reset { target, yes } => {
            require_confirmation(yes)?;
            let path = state_path(&target);
            let mut store = StateStore::open(StateLocation::File(path), StateLimits::default())
                .map_err(CliError::State)?;
            store.reset().map_err(CliError::State)?;
            println!("reset: {}", target.app_id);
        }
        StateCommand::Verify(target) => {
            let verification = verify_file(&state_path(&target)).map_err(CliError::State)?;
            println!("app: {}", target.app_id);
            println!("schema: {}", verification.schema_version);
            println!(
                "integrity: {}",
                if verification.integrity_ok {
                    "ok"
                } else {
                    "failed"
                }
            );
            println!(
                "usage: {}",
                if verification.usage_matches() {
                    "ok"
                } else {
                    "mismatch"
                }
            );
            println!("stored keys: {}", verification.stored.key_count);
            println!("computed keys: {}", verification.computed.key_count);
            println!("stored bytes: {}", verification.stored.logical_bytes);
            println!("computed bytes: {}", verification.computed.logical_bytes);
            if !verification.integrity_ok || !verification.usage_matches() {
                return Err(CliError::Message("state verification failed".to_owned()));
            }
        }
        StateCommand::RepairUsage {
            target,
            backup,
            yes,
        } => {
            require_confirmation(yes)?;
            let verification =
                repair_usage(&state_path(&target), &backup).map_err(CliError::State)?;
            println!("repaired: {}", target.app_id);
            println!("backup: {}", backup.display());
            println!("keys: {}", verification.computed.key_count);
            println!("bytes: {}", verification.computed.logical_bytes);
        }
    }
    Ok(())
}

fn state_path(target: &StateTarget) -> PathBuf {
    target
        .state_dir
        .join(target.app_id.as_str())
        .join("state.sqlite3")
}

fn require_confirmation(yes: bool) -> Result<(), CliError> {
    if yes {
        Ok(())
    } else {
        Err(CliError::Message(
            "refusing state maintenance without --yes".to_owned(),
        ))
    }
}

fn print_receipt(receipt: &TurnReceipt, status: &str) {
    println!("turn: {}", receipt.turn_id);
    println!("event sequence: {}", receipt.event_sequence);
    println!("base revision: {}", receipt.base_revision);
    println!("next revision: {}", receipt.next_revision);
    println!("patches: {}", receipt.patch_count);
    println!("status: {status}");
}

fn print_inspection(inspection: &AppInspection) {
    println!("lifecycle: {}", inspection.lifecycle);
    println!("world: {}", inspection.world);
    println!(
        "current revision: {}",
        display_option(inspection.current_revision)
    );
    println!(
        "next event sequence: {}",
        display_option(inspection.next_event_sequence)
    );
    println!(
        "last event sequence: {}",
        display_option(inspection.last_event_sequence)
    );
    println!("nodes: {}", inspection.node_count);
    println!("depth: {}", inspection.depth);
    match &inspection.last_turn {
        Some(turn) => {
            println!("last turn: {}", turn.turn_id);
            println!("last turn event sequence: {}", turn.event_sequence);
            println!("last turn base revision: {}", turn.base_revision);
            println!("last turn next revision: {}", turn.next_revision);
            println!("last turn patches: {}", turn.patch_count);
            println!("last turn committed: {}", turn.committed);
        }
        None => println!("last turn: none"),
    }
    match &inspection.fault {
        Some(fault) => println!(
            "fault: {}: {}",
            category_name(fault.category),
            fault.message
        ),
        None => println!("fault: none"),
    }
    println!();
    print!("{}", inspection.canonical_tree);
}

fn inspection_json(inspection: &AppInspection) -> Value {
    json!({
        "lifecycle": inspection.lifecycle.to_string(),
        "world": inspection.world,
        "current_revision": inspection.current_revision,
        "next_event_sequence": inspection.next_event_sequence,
        "last_event_sequence": inspection.last_event_sequence,
        "node_count": inspection.node_count,
        "depth": inspection.depth,
        "last_turn": inspection.last_turn.as_ref().map(turn_json),
        "fault": inspection.fault.as_ref().map(fault_json),
        "canonical_tree": inspection.canonical_tree,
    })
}

fn turn_json(turn: &TurnReceipt) -> Value {
    json!({
        "turn_id": turn.turn_id,
        "event_sequence": turn.event_sequence,
        "base_revision": turn.base_revision,
        "next_revision": turn.next_revision,
        "patch_count": turn.patch_count,
        "committed": turn.committed,
    })
}

fn fault_json(fault: &AppFault) -> Value {
    json!({
        "category": category_name(fault.category),
        "message": fault.message,
    })
}

fn read_script(path: &Path) -> Result<Vec<NodeId>, CliError> {
    let source = std::fs::read_to_string(path).map_err(|error| {
        CliError::Message(format!("failed to read script {}: {error}", path.display()))
    })?;
    let value: Value = serde_json::from_str(&source).map_err(|error| {
        CliError::Message(format!(
            "invalid script JSON in {}: {error}",
            path.display()
        ))
    })?;
    let root = value.as_object().ok_or_else(|| schema_error(path))?;
    if root.len() != 1 || !root.contains_key("events") {
        return Err(schema_error(path));
    }
    let events = root["events"]
        .as_array()
        .ok_or_else(|| schema_error(path))?;
    events
        .iter()
        .enumerate()
        .map(|(index, event)| {
            let event = event
                .as_object()
                .ok_or_else(|| event_schema_error(path, index))?;
            if event.len() != 1 {
                return Err(event_schema_error(path, index));
            }
            let node = event
                .get("activate")
                .and_then(Value::as_u64)
                .ok_or_else(|| event_schema_error(path, index))?;
            parse_node_id(node)
        })
        .collect()
}

fn schema_error(path: &Path) -> CliError {
    CliError::Message(format!(
        "invalid script schema in {}: expected {{\"events\":[{{\"activate\":4}}]}}",
        path.display()
    ))
}

fn event_schema_error(path: &Path, index: usize) -> CliError {
    CliError::Message(format!(
        "invalid event {index} in {}: expected exactly {{\"activate\":<non-zero integer>}}",
        path.display()
    ))
}

fn parse_node_id(value: u64) -> Result<NodeId, CliError> {
    NodeId::new(value)
        .ok_or_else(|| CliError::Message("node ID must be a non-zero integer".to_owned()))
}

fn component_name(path: &Path) -> String {
    path.file_name()
        .and_then(|name| name.to_str())
        .map_or_else(|| path.display().to_string(), String::from)
}

fn display_option(value: Option<u64>) -> String {
    value.map_or_else(|| "none".to_owned(), |value| value.to_string())
}

fn category_name(category: RuntimeErrorCategory) -> &'static str {
    match category {
        RuntimeErrorCategory::ComponentTooLarge => "component_too_large",
        RuntimeErrorCategory::InvalidComponent => "invalid_component",
        RuntimeErrorCategory::UnsupportedWorld => "unsupported_world",
        RuntimeErrorCategory::LinkFailure => "link_failure",
        RuntimeErrorCategory::InstantiationFailure => "instantiation_failure",
        RuntimeErrorCategory::InvalidLifecycle => "invalid_lifecycle",
        RuntimeErrorCategory::GuestRejected => "guest_rejected",
        RuntimeErrorCategory::GuestTrap => "guest_trap",
        RuntimeErrorCategory::FuelExhausted => "fuel_exhausted",
        RuntimeErrorCategory::DeadlineExceeded => "deadline_exceeded",
        RuntimeErrorCategory::MemoryLimitExceeded => "memory_limit_exceeded",
        RuntimeErrorCategory::TransferLimitExceeded => "transfer_limit_exceeded",
        RuntimeErrorCategory::InvalidSnapshot => "invalid_snapshot",
        RuntimeErrorCategory::InvalidPatchBatch => "invalid_patch_batch",
        RuntimeErrorCategory::RevisionMismatch => "revision_mismatch",
        RuntimeErrorCategory::EventSequenceViolation => "event_sequence_violation",
        RuntimeErrorCategory::StateUnavailable => "state_unavailable",
        RuntimeErrorCategory::StateCommitFailed => "state_commit_failed",
        RuntimeErrorCategory::WorkerStopped => "worker_stopped",
        RuntimeErrorCategory::Internal => "internal",
    }
}

#[derive(Debug)]
enum CliError {
    Runtime(RuntimeError),
    State(youth_state::StateError),
    Message(String),
}

impl std::fmt::Display for CliError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime(error) => write!(
                formatter,
                "{}: {}",
                category_name(error.category()),
                error.context()
            ),
            Self::State(error) => write!(formatter, "state: {error}"),
            Self::Message(message) => formatter.write_str(message),
        }
    }
}
