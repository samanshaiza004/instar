//! Central component compatibility and Rust guest-profile validation.

use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{RuntimeError, YouthApp, component_imports};

pub const APPLICATION_PROTOCOL: &str = "0.0.8";
pub const APPLICATION_WORLD: &str = "youth:app/application@0.0.8";
pub const SUPPORTED_APPLICATION_PROTOCOLS: &[&str] = &[
    "0.0.2",
    "0.0.3",
    "0.0.4",
    "0.0.5",
    "0.0.6",
    "0.0.7",
    APPLICATION_PROTOCOL,
];

pub const REQUIRED_GUEST_IMPORTS: &[&str] = &["youth:app/ui"];

pub const PERMITTED_GUEST_IMPORTS: &[&str] = &[
    "youth:time/scheduler",
    "youth:editor/session",
    "youth:text-document/document",
    "youth:state/store",
    "wasi:cli/environment",
    "wasi:cli/exit",
    "wasi:cli/stderr",
    "wasi:cli/stdin",
    "wasi:cli/stdout",
    "wasi:cli/terminal-input",
    "wasi:cli/terminal-output",
    "wasi:cli/terminal-stderr",
    "wasi:cli/terminal-stdin",
    "wasi:cli/terminal-stdout",
    "wasi:clocks/monotonic-clock",
    "wasi:io/error",
    "wasi:io/poll",
    "wasi:io/streams",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComponentValidation {
    pub world: String,
    pub size: u64,
    pub sha256: String,
    pub imports: Vec<String>,
}

/// Validates component structure, exact host linkage, and the closed guest profile.
pub fn validate_component(
    path: impl AsRef<Path>,
) -> Result<ComponentValidation, ComponentValidationError> {
    let path = path.as_ref();
    let bytes = fs::read(path).map_err(|source| ComponentValidationError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let imports = component_imports(path)?;
    validate_imports(&imports)?;

    // Loading performs Wasmtime component compilation and exact host linking. It
    // also identifies the actual supported world so callers never infer a
    // component's protocol from the current template version.
    let app = YouthApp::load(path)?;
    let world = app.inspect().world;

    Ok(ComponentValidation {
        world,
        size: u64::try_from(bytes.len()).unwrap_or(u64::MAX),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        imports,
    })
}

fn validate_imports(imports: &[String]) -> Result<(), ComponentValidationError> {
    let unexpected = imports
        .iter()
        .filter(|import| {
            !REQUIRED_GUEST_IMPORTS.contains(&import.as_str())
                && !PERMITTED_GUEST_IMPORTS.contains(&import.as_str())
        })
        .cloned()
        .collect::<Vec<_>>();
    if !unexpected.is_empty() {
        return Err(ComponentValidationError::UnexpectedImports(unexpected));
    }
    let missing = REQUIRED_GUEST_IMPORTS
        .iter()
        .filter(|required| !imports.iter().any(|import| import == **required))
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(ComponentValidationError::MissingImports(missing));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ComponentValidationError {
    #[error("could not read component {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("component imports interfaces outside the Youth Rust Guest Profile: {0:?}")]
    UnexpectedImports(Vec<String>),
    #[error("component does not import the Youth contract: {0:?}")]
    MissingImports(Vec<String>),
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_accepts_exact_budget_and_rejects_changes() {
        let imports = REQUIRED_GUEST_IMPORTS
            .iter()
            .chain(PERMITTED_GUEST_IMPORTS)
            .map(|value| (*value).to_owned())
            .collect::<Vec<_>>();
        validate_imports(&imports).expect("exact profile");

        let legacy_imports = imports
            .iter()
            .filter(|value| *value != "youth:time/scheduler" && *value != "youth:editor/session")
            .cloned()
            .collect::<Vec<_>>();
        validate_imports(&legacy_imports)
            .expect("legacy protocols need not import scheduling or Editor sessions");

        let mut widened = imports.clone();
        widened.push("wasi:filesystem/types".into());
        assert!(matches!(
            validate_imports(&widened),
            Err(ComponentValidationError::UnexpectedImports(_))
        ));

        let missing = imports
            .into_iter()
            .filter(|value| value != "youth:app/ui")
            .collect::<Vec<_>>();
        assert!(matches!(
            validate_imports(&missing),
            Err(ComponentValidationError::MissingImports(_))
        ));
    }
}
