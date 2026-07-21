//! `youth` CLI: validate, mount, activate, script, inspect.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;
use youth_desktop::DesktopOptions;
use youth_runtime::{
    AppFault, AppInspection, RuntimeError, RuntimeErrorCategory, TurnReceipt, YouthAppHandle,
};
use youth_state::{AppId, StateLimits, StateLocation, StateStore, repair_usage, verify_file};
use youth_tree::NodeId;

mod project_commands;

/// Inspection and test surface for the Youth runtime.
#[derive(Debug, Parser)]
#[command(name = "youth", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create the proven Rust Tally project in a new directory.
    New {
        destination: PathBuf,
        #[arg(long)]
        id: AppId,
    },
    /// Diagnose the local Youth development environment.
    Doctor {
        /// Also create and present a native smoke window.
        #[arg(long)]
        full: bool,
    },
    /// Validate the current external project and development component.
    Check,
    /// Check that a component loads and exports the Youth world. Does not mount.
    Validate { component: PathBuf },
    /// Mount a component and print its initial canonical tree.
    Mount {
        component: PathBuf,
        #[command(flatten)]
        state: HeadlessState,
    },
    /// Mount, deliver one activation, and print the resulting tree.
    Activate {
        component: PathBuf,
        #[arg(long)]
        node: u64,
        #[command(flatten)]
        state: HeadlessState,
    },
    /// Replay a deterministic event script and print one canonical snapshot.
    Script {
        component: PathBuf,
        events: PathBuf,
        #[command(flatten)]
        state: HeadlessState,
    },
    /// Mount and report full application state.
    Inspect {
        component: PathBuf,
        #[arg(long)]
        json: bool,
        #[command(flatten)]
        state: HeadlessState,
    },
    /// Run an application in one native desktop window.
    Run {
        component: PathBuf,
        #[arg(long)]
        app_id: AppId,
        #[arg(long, conflicts_with = "ephemeral")]
        state_dir: Option<PathBuf>,
        #[arg(long, conflicts_with = "state_dir")]
        ephemeral: bool,
        #[arg(long, default_value_t = 640)]
        window_width: u32,
        #[arg(long, default_value_t = 360)]
        window_height: u32,
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

#[derive(Debug, Default, clap::Args)]
struct HeadlessState {
    #[arg(long, requires = "state_dir")]
    app_id: Option<AppId>,
    #[arg(long, requires = "app_id")]
    state_dir: Option<PathBuf>,
}

fn main() -> ExitCode {
    init_tracing();
    let command = Cli::parse().command;
    let result = match command {
        Command::Doctor { full } => project_commands::doctor(full),
        Command::Run {
            component,
            app_id,
            state_dir,
            ephemeral,
            window_width,
            window_height,
        } => run_desktop(
            component,
            app_id,
            state_dir,
            ephemeral,
            window_width,
            window_height,
        ),
        command => tokio::runtime::Runtime::new()
            .map_err(|error| CliError::Message(format!("failed to start CLI runtime: {error}")))
            .and_then(|runtime| runtime.block_on(run(command))),
    };
    match result {
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

async fn run(command: Command) -> Result<(), CliError> {
    match command {
        Command::New { destination, id } => project_commands::new_project(&destination, &id)?,
        Command::Check => project_commands::check_project()?,
        Command::Validate { component } => {
            let app = YouthAppHandle::spawn_ephemeral(&component).map_err(CliError::Runtime)?;
            let inspection = app.inspect().await.map_err(CliError::Runtime)?;
            println!(
                "validated: {} ({})",
                component_name(&component),
                inspection.world
            );
        }
        Command::Mount { component, state } => {
            let app = spawn_headless(&component, state)?;
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
        Command::Activate {
            component,
            node,
            state,
        } => {
            let node = parse_node_id(node)?;
            let app = spawn_headless(&component, state)?;
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
        Command::Script {
            component,
            events,
            state,
        } => {
            let nodes = read_script(&events)?;
            let app = spawn_headless(&component, state)?;
            app.mount().await.map_err(CliError::Runtime)?;
            for node in nodes {
                app.activate(node).await.map_err(CliError::Runtime)?;
            }
            let inspection = app.inspect().await.map_err(CliError::Runtime)?;
            print!("{}", inspection.canonical_tree);
        }
        Command::Inspect {
            component,
            json,
            state,
        } => {
            let app = spawn_headless(&component, state)?;
            app.mount().await.map_err(CliError::Runtime)?;
            let inspection = app.inspect().await.map_err(CliError::Runtime)?;
            if json {
                println!("{}", inspection_json(&inspection));
            } else {
                print_inspection(&inspection);
            }
        }
        Command::State { command } => run_state_command(command)?,
        Command::Run { .. } | Command::Doctor { .. } => {
            unreachable!("main-thread command was already dispatched")
        }
    }
    Ok(())
}

fn run_desktop(
    component: PathBuf,
    app_id: AppId,
    state_dir: Option<PathBuf>,
    ephemeral: bool,
    width: u32,
    height: u32,
) -> Result<(), CliError> {
    let state = if ephemeral {
        StateLocation::Memory
    } else {
        let root = match state_dir {
            Some(root) => root,
            None => directories::BaseDirs::new()
                .ok_or_else(|| CliError::Message("platform data directory is unavailable".into()))?
                .data_local_dir()
                .join("youth")
                .join("state"),
        };
        StateLocation::File(root.join(app_id.as_str()).join("state.sqlite3"))
    };
    youth_desktop::run(DesktopOptions {
        config: youth_runtime::YouthAppConfig {
            component_path: component,
            app_id,
            state,
            limits: youth_runtime::RuntimeLimits::default(),
        },
        width,
        height,
    })
    .map_err(CliError::Desktop)
}

fn spawn_headless(component: &Path, state: HeadlessState) -> Result<YouthAppHandle, CliError> {
    match (state.app_id, state.state_dir) {
        (Some(app_id), Some(root)) => YouthAppHandle::spawn(youth_runtime::YouthAppConfig {
            component_path: component.to_owned(),
            state: StateLocation::File(root.join(app_id.as_str()).join("state.sqlite3")),
            app_id,
            limits: youth_runtime::RuntimeLimits::default(),
        })
        .map_err(CliError::Runtime),
        (None, None) => YouthAppHandle::spawn_ephemeral(component).map_err(CliError::Runtime),
        _ => Err(CliError::Message(
            "--app-id and --state-dir must be supplied together".into(),
        )),
    }
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
    println!("app ID: {}", inspection.app_id);
    println!("state location: {}", inspection.state_location);
    println!(
        "state schema: {}",
        inspection.state_summary.map_or_else(
            || "unavailable".to_owned(),
            |summary| summary.schema_version.to_string()
        )
    );
    println!(
        "state transaction active: {}",
        inspection.state_transaction_active
    );
    println!("state phase: {:?}", inspection.state_phase);
    println!("state calls: {}", inspection.state_metrics.calls);
    println!("state writes: {}", inspection.state_metrics.writes);
    println!(
        "state bytes: {}",
        inspection
            .state_summary
            .map_or(0, |summary| summary.logical_bytes)
    );
    println!("last state commit: {}", inspection.state_metrics.committed);
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
        "app_id": inspection.app_id,
        "state_location": inspection.state_location,
        "state_schema": inspection.state_summary.map(|summary| summary.schema_version),
        "state_transaction_active": inspection.state_transaction_active,
        "state_phase": format!("{:?}", inspection.state_phase).to_lowercase(),
        "state_calls": inspection.state_metrics.calls,
        "state_writes": inspection.state_metrics.writes,
        "state_bytes": inspection.state_summary.map(|summary| summary.logical_bytes),
        "last_state_commit": inspection.state_metrics.committed,
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
        "first_event_sequence": turn.first_event_sequence,
        "processed_through": turn.processed_through,
        "event_sequence": turn.event_sequence,
        "base_revision": turn.base_revision,
        "next_revision": turn.next_revision,
        "patch_count": turn.patch_count,
        "patch_batch": turn.patch_batch,
        "state_writes": turn.state_writes,
        "elapsed_microseconds": turn.elapsed.as_micros(),
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
    Desktop(youth_desktop::DesktopError),
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
            Self::Desktop(error) => write!(formatter, "desktop: {error}"),
            Self::State(error) => write!(formatter, "state: {error}"),
            Self::Message(message) => formatter.write_str(message),
        }
    }
}
