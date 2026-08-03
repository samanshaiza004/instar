#![forbid(unsafe_code)]

use std::env;
use std::io;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use thiserror::Error;
use youth_capsule::CapsuleManifest;
use youth_capsule::verify::{CapsuleVerifyError, verify_capsule};
use youth_runtime::{
    ComponentValidation, ComponentValidationError, SUPPORTED_APPLICATION_PROTOCOLS, StateLocation,
    validate_component,
};

const APPLICATION_WORLD_PREFIX: &str = "youth:app/application@";

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Verifies a packaged Youth application before launch."
)]
struct LauncherArgs {
    #[arg(long, value_name = "PATH", help = "Use a capsule directory directly")]
    capsule_dir: Option<PathBuf>,
    #[arg(
        long,
        help = "Verify the capsule without opening a window or mounting the app"
    )]
    verify_only: bool,
}

pub trait CapsuleLocator {
    fn locate(&self) -> Result<PathBuf, CapsuleLocateError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExplicitCapsuleLocator(PathBuf);

impl ExplicitCapsuleLocator {
    pub fn new(capsule_directory: PathBuf) -> Self {
        Self(capsule_directory)
    }
}

impl CapsuleLocator for ExplicitCapsuleLocator {
    fn locate(&self) -> Result<PathBuf, CapsuleLocateError> {
        if self.0.is_dir() {
            Ok(self.0.clone())
        } else {
            Err(CapsuleLocateError::ExplicitDirectoryNotFound)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdjacentCapsuleLocator {
    exe_path: PathBuf,
}

impl AdjacentCapsuleLocator {
    pub fn new(exe_path: PathBuf) -> Self {
        Self { exe_path }
    }
}

impl CapsuleLocator for AdjacentCapsuleLocator {
    fn locate(&self) -> Result<PathBuf, CapsuleLocateError> {
        let exe_directory = self
            .exe_path
            .parent()
            .ok_or(CapsuleLocateError::AdjacentExecutablePathMalformed)?;
        let capsule_directory = exe_directory.join("capsule");
        if capsule_directory.is_dir() {
            Ok(capsule_directory)
        } else {
            Err(CapsuleLocateError::AdjacentDirectoryNotFound)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MacBundleCapsuleLocator {
    exe_path: PathBuf,
}

impl MacBundleCapsuleLocator {
    pub fn new(exe_path: PathBuf) -> Self {
        Self { exe_path }
    }
}

impl CapsuleLocator for MacBundleCapsuleLocator {
    fn locate(&self) -> Result<PathBuf, CapsuleLocateError> {
        let macos_directory = self
            .exe_path
            .parent()
            .ok_or(CapsuleLocateError::MacBundleExecutablePathMalformed)?;
        let contents_directory = macos_directory
            .parent()
            .ok_or(CapsuleLocateError::MacBundleExecutablePathMalformed)?;
        if macos_directory.file_name().and_then(|name| name.to_str()) != Some("MacOS")
            || contents_directory
                .file_name()
                .and_then(|name| name.to_str())
                != Some("Contents")
        {
            return Err(CapsuleLocateError::MacBundleExecutablePathMalformed);
        }

        let capsule_directory = contents_directory.join("Resources").join("capsule");
        if capsule_directory.is_dir() {
            Ok(capsule_directory)
        } else {
            Err(CapsuleLocateError::MacBundleDirectoryNotFound)
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CapsuleLocateError {
    #[error("The selected capsule directory was not found.")]
    ExplicitDirectoryNotFound,
    #[error("The capsule directory was not found beside this launcher.")]
    AdjacentDirectoryNotFound,
    #[error("This launcher's installation path cannot locate an adjacent capsule.")]
    AdjacentExecutablePathMalformed,
    #[error("The capsule directory was not found in this application bundle.")]
    MacBundleDirectoryNotFound,
    #[error("This launcher's installation path is not a valid application bundle.")]
    MacBundleExecutablePathMalformed,
}

#[derive(Debug, Error)]
pub enum LauncherError {
    #[error("The launcher could not determine its installation location.")]
    CurrentExecutable {
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Locate(#[from] CapsuleLocateError),
    #[error(transparent)]
    Verify(#[from] CapsuleVerifyError),
    #[error("The app component could not be validated by this launcher.")]
    ComponentValidation {
        #[source]
        source: ComponentValidationError,
    },
    #[error("The app component does not declare a valid Youth application version.")]
    InvalidApplicationWorld,
    #[error(
        "The app package failed its profile check because its manifest and component declare different versions."
    )]
    ProfileMismatch,
    #[error("This launcher doesn't support this app's version.")]
    UnsupportedProtocol,
    #[error("The platform application-data directory is unavailable.")]
    DataDirectoryUnavailable,
    #[error(transparent)]
    Desktop(#[from] youth_desktop::DesktopError),
}

struct PreflightSuccess {
    capsule_directory: PathBuf,
    manifest: CapsuleManifest,
    validation: ComponentValidation,
    protocol: String,
}

fn main() -> ExitCode {
    match run_launcher(LauncherArgs::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run_launcher(args: LauncherArgs) -> Result<(), LauncherError> {
    let verify_only = args.verify_only;
    let success = launch(args)?;
    let app_name = single_line(&success.manifest.app_name);
    println!(
        "Verified {app_name} ({}) — protocol {}, component SHA-256 {}.",
        success.manifest.app_id, success.protocol, success.validation.sha256
    );
    if verify_only {
        return Ok(());
    }

    let component_path = success
        .capsule_directory
        .join(success.manifest.component.path.as_ref());
    let app_id = success.manifest.app_id.clone();
    let state = production_state(&app_id)?;
    youth_desktop::run_capsule_launch(youth_desktop::CapsuleLaunchOptions {
        picker: Box::new(youth_desktop::RfdDocumentPicker::new()),
        component_path,
        app_id,
        app_name,
        state,
        width: 1024,
        height: 720,
    })?;
    Ok(())
}

fn launch(args: LauncherArgs) -> Result<PreflightSuccess, LauncherError> {
    let capsule_directory = locate_capsule(args.capsule_dir)?;
    preflight(&capsule_directory)
}

fn production_state(app_id: &youth_runtime::AppId) -> Result<StateLocation, LauncherError> {
    let data_directory = directories::BaseDirs::new()
        .ok_or(LauncherError::DataDirectoryUnavailable)?
        .data_local_dir()
        .join("youth")
        .join("apps");
    Ok(StateLocation::File(
        data_directory.join(app_id.as_str()).join("state.sqlite3"),
    ))
}

fn locate_capsule(explicit: Option<PathBuf>) -> Result<PathBuf, LauncherError> {
    if let Some(capsule_directory) = explicit {
        return ExplicitCapsuleLocator::new(capsule_directory)
            .locate()
            .map_err(LauncherError::from);
    }

    let exe_path =
        env::current_exe().map_err(|source| LauncherError::CurrentExecutable { source })?;
    locate_packaged_capsule(exe_path).map_err(LauncherError::from)
}

#[cfg(target_os = "macos")]
fn locate_packaged_capsule(exe_path: PathBuf) -> Result<PathBuf, CapsuleLocateError> {
    // Development binaries are not bundle-shaped, so macOS also accepts the
    // portable adjacent layout after checking the packaged layout first.
    MacBundleCapsuleLocator::new(exe_path.clone())
        .locate()
        .or_else(|_| AdjacentCapsuleLocator::new(exe_path).locate())
}

#[cfg(not(target_os = "macos"))]
fn locate_packaged_capsule(exe_path: PathBuf) -> Result<PathBuf, CapsuleLocateError> {
    AdjacentCapsuleLocator::new(exe_path).locate()
}

fn preflight(capsule_directory: &Path) -> Result<PreflightSuccess, LauncherError> {
    let manifest = verify_capsule(capsule_directory)?;
    let component_path = capsule_directory.join(manifest.component.path.as_ref());
    let validation = validate_component(component_path)
        .map_err(|source| LauncherError::ComponentValidation { source })?;
    let protocol = application_protocol(&validation.world)?.to_owned();

    if protocol != manifest.required_profile.protocol {
        return Err(LauncherError::ProfileMismatch);
    }
    if !SUPPORTED_APPLICATION_PROTOCOLS.contains(&protocol.as_str()) {
        return Err(LauncherError::UnsupportedProtocol);
    }

    Ok(PreflightSuccess {
        capsule_directory: capsule_directory.to_owned(),
        manifest,
        validation,
        protocol,
    })
}

fn application_protocol(world: &str) -> Result<&str, LauncherError> {
    world
        .strip_prefix(APPLICATION_WORLD_PREFIX)
        .filter(|protocol| !protocol.is_empty())
        .ok_or(LauncherError::InvalidApplicationWorld)
}

fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn explicit_locator_accepts_a_directory() {
        let directory = tempdir().unwrap();
        let locator = ExplicitCapsuleLocator::new(directory.path().to_path_buf());

        assert_eq!(locator.locate().unwrap(), directory.path());
    }

    #[test]
    fn explicit_locator_rejects_a_missing_directory() {
        let directory = tempdir().unwrap();
        let locator = ExplicitCapsuleLocator::new(directory.path().join("missing"));

        assert_eq!(
            locator.locate(),
            Err(CapsuleLocateError::ExplicitDirectoryNotFound)
        );
    }

    #[test]
    fn adjacent_locator_accepts_a_sibling_capsule_directory() {
        let directory = tempdir().unwrap();
        let capsule_directory = directory.path().join("bin/capsule");
        fs::create_dir_all(&capsule_directory).unwrap();
        let locator = AdjacentCapsuleLocator::new(directory.path().join("bin/launcher"));

        assert_eq!(locator.locate().unwrap(), capsule_directory);
    }

    #[test]
    fn adjacent_locator_rejects_a_missing_directory() {
        let directory = tempdir().unwrap();
        let locator = AdjacentCapsuleLocator::new(directory.path().join("bin/launcher"));

        assert_eq!(
            locator.locate(),
            Err(CapsuleLocateError::AdjacentDirectoryNotFound)
        );
    }

    #[test]
    fn adjacent_locator_rejects_a_path_without_a_parent() {
        let locator = AdjacentCapsuleLocator::new(PathBuf::new());

        assert_eq!(
            locator.locate(),
            Err(CapsuleLocateError::AdjacentExecutablePathMalformed)
        );
    }

    #[test]
    fn mac_bundle_locator_accepts_resources_capsule_directory() {
        let directory = tempdir().unwrap();
        let capsule_directory = directory
            .path()
            .join("Scratchpad.app/Contents/Resources/capsule");
        fs::create_dir_all(&capsule_directory).unwrap();
        let locator = MacBundleCapsuleLocator::new(
            directory
                .path()
                .join("Scratchpad.app/Contents/MacOS/Scratchpad"),
        );

        assert_eq!(locator.locate().unwrap(), capsule_directory);
    }

    #[test]
    fn mac_bundle_locator_rejects_a_missing_directory() {
        let directory = tempdir().unwrap();
        let locator = MacBundleCapsuleLocator::new(
            directory
                .path()
                .join("Scratchpad.app/Contents/MacOS/Scratchpad"),
        );

        assert_eq!(
            locator.locate(),
            Err(CapsuleLocateError::MacBundleDirectoryNotFound)
        );
    }

    #[test]
    fn mac_bundle_locator_rejects_a_non_bundle_path() {
        let directory = tempdir().unwrap();
        let locator = MacBundleCapsuleLocator::new(directory.path().join("bin/launcher"));

        assert_eq!(
            locator.locate(),
            Err(CapsuleLocateError::MacBundleExecutablePathMalformed)
        );
    }

    #[test]
    fn application_protocol_is_derived_from_the_observed_world() {
        assert_eq!(
            application_protocol("youth:app/application@0.0.8").unwrap(),
            "0.0.8"
        );
    }

    #[test]
    fn application_protocol_rejects_a_malformed_world() {
        assert!(matches!(
            application_protocol("other:app/application@0.0.8"),
            Err(LauncherError::InvalidApplicationWorld)
        ));
        assert!(matches!(
            application_protocol("youth:app/application@"),
            Err(LauncherError::InvalidApplicationWorld)
        ));
    }

    #[test]
    fn single_line_replaces_control_characters() {
        assert_eq!(single_line("Scratch\npad\t"), "Scratch pad ");
    }
}
