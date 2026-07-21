//! Strict, language-neutral project metadata for external Youth applications.

#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;
use walkdir::WalkDir;
use youth_state::AppId;

pub const MANIFEST_NAME: &str = "Youth.toml";
pub const LOCK_NAME: &str = "Youth.lock";
pub const SUPPORTED_PROTOCOL: &str = "0.0.2";
pub const SUPPORTED_LANGUAGE: &str = "rust";
pub const SUPPORTED_TARGET: &str = "wasm32-wasip2";
pub const SDK_SOURCE: &str = "https://github.com/samanshaiza004/youth";
pub const SDK_REVISION: &str = "c5e2b0f2b96746e2251e8d9e0da440a3f7b7e46a";
pub const TEMPLATE_VERSION: u32 = 1;
pub const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");
pub const TEMPLATE_WIT_SHA256: &str =
    "71ddf56e68c9fada51c9dbe39878e520a1afe77637a8b9081d1074fe597545d7";

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub app: App,
    pub build: Build,
    pub development: Development,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct App {
    pub id: String,
    pub name: String,
    pub protocol: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub language: String,
    pub package: String,
    pub target: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Development {
    pub state: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Lock {
    pub lock_version: u32,
    pub protocol: String,
    pub sdk_source: String,
    pub sdk_revision: String,
    pub wit_sha256: String,
    pub cli_version: String,
    pub template_version: u32,
}

#[derive(Clone, Debug)]
pub struct Project {
    root: PathBuf,
    pub manifest: Manifest,
    pub lock: Lock,
    pub app_id: AppId,
}

impl Project {
    /// Discovers `Youth.toml` upward, then validates the complete static contract.
    pub fn discover(start: impl AsRef<Path>) -> Result<Self, ProjectError> {
        let start = start.as_ref();
        let mut directory = if start.is_file() {
            start.parent().unwrap_or(start)
        } else {
            start
        };
        loop {
            if directory.join(MANIFEST_NAME).is_file() {
                return Self::load(directory);
            }
            directory = directory.parent().ok_or_else(|| {
                ProjectError::Contract(format!(
                    "could not find {MANIFEST_NAME} from {} or any parent",
                    start.display()
                ))
            })?;
        }
    }

    pub fn load(root: impl AsRef<Path>) -> Result<Self, ProjectError> {
        let root = root.as_ref().to_path_buf();
        let manifest: Manifest = read_toml(&root.join(MANIFEST_NAME))?;
        let lock: Lock = read_toml(&root.join(LOCK_NAME))?;
        let app_id = AppId::parse(manifest.app.id.clone())
            .map_err(|error| ProjectError::Contract(error.to_string()))?;
        validate_manifest(&manifest)?;
        validate_lock(&manifest, &lock)?;
        validate_relative_path(&manifest.development.state, "development.state")?;
        Ok(Self {
            root,
            manifest,
            lock,
            app_id,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn state_root(&self) -> PathBuf {
        self.root.join(&self.manifest.development.state)
    }

    #[must_use]
    pub fn component_name(&self) -> String {
        format!("{}.wasm", self.manifest.build.package.replace('-', "_"))
    }

    #[must_use]
    pub fn cargo_component(&self, release: bool) -> PathBuf {
        self.root
            .join("target")
            .join(SUPPORTED_TARGET)
            .join(if release { "release" } else { "debug" })
            .join(self.component_name())
    }

    #[must_use]
    pub fn dist_component(&self) -> PathBuf {
        self.root
            .join("dist")
            .join(format!("{}.wasm", self.app_id.as_str()))
    }

    /// Verifies the inspectable WIT snapshot and both Cargo dependency records.
    pub fn verify_locked_inputs(&self, running_cli_version: &str) -> Result<(), ProjectError> {
        compare(
            "lock.cli-version",
            &self.lock.cli_version,
            running_cli_version,
        )?;
        let digest = hash_wit_tree(self.root.join("wit/youth"))?;
        compare("lock.wit-sha256", &self.lock.wit_sha256, &digest)?;
        compare("embedded-template.wit-sha256", TEMPLATE_WIT_SHA256, &digest)?;
        verify_cargo_manifest(&self.root.join("Cargo.toml"), &self.lock)?;
        verify_cargo_lock(&self.root.join("Cargo.lock"), &self.lock)?;
        Ok(())
    }
}

fn read_toml<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, ProjectError> {
    let text = fs::read_to_string(path).map_err(|source| ProjectError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ProjectError::Toml {
        path: path.to_path_buf(),
        source,
    })
}

fn validate_manifest(manifest: &Manifest) -> Result<(), ProjectError> {
    compare("app.protocol", &manifest.app.protocol, SUPPORTED_PROTOCOL)?;
    compare(
        "build.language",
        &manifest.build.language,
        SUPPORTED_LANGUAGE,
    )?;
    compare("build.target", &manifest.build.target, SUPPORTED_TARGET)?;
    if manifest.app.name.trim().is_empty() {
        return Err(ProjectError::Contract("app.name must not be empty".into()));
    }
    if manifest.build.package.trim().is_empty() {
        return Err(ProjectError::Contract(
            "build.package must not be empty".into(),
        ));
    }
    Ok(())
}

fn validate_lock(manifest: &Manifest, lock: &Lock) -> Result<(), ProjectError> {
    if lock.lock_version != 1 {
        return mismatch("lock-version", lock.lock_version, 1);
    }
    compare(
        "Youth.toml app.protocol",
        &manifest.app.protocol,
        &lock.protocol,
    )?;
    compare("runtime protocol", &lock.protocol, SUPPORTED_PROTOCOL)?;
    compare("sdk-source", &lock.sdk_source, SDK_SOURCE)?;
    compare("sdk-revision", &lock.sdk_revision, SDK_REVISION)?;
    compare("cli-version", &lock.cli_version, CLI_VERSION)?;
    if lock.template_version != TEMPLATE_VERSION {
        return mismatch("template-version", lock.template_version, TEMPLATE_VERSION);
    }
    if !is_lower_hex(&lock.sdk_revision, 40) {
        return Err(ProjectError::Contract(
            "sdk-revision must be exactly 40 lowercase hexadecimal characters".into(),
        ));
    }
    if !is_lower_hex(&lock.wit_sha256, 64) {
        return Err(ProjectError::Contract(
            "wit-sha256 must be exactly 64 lowercase hexadecimal characters".into(),
        ));
    }
    Ok(())
}

fn validate_relative_path(path: &Path, field: &str) -> Result<(), ProjectError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(ProjectError::Contract(format!(
            "{field} must be a nonempty path within the project"
        )));
    }
    if path.components().any(|part| {
        matches!(
            part,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return Err(ProjectError::Contract(format!(
            "{field} must not escape the project"
        )));
    }
    Ok(())
}

fn compare(field: &str, actual: &str, expected: &str) -> Result<(), ProjectError> {
    if actual == expected {
        Ok(())
    } else {
        Err(ProjectError::Contract(format!(
            "{field} mismatch: found {actual:?}, expected {expected:?}; regenerate the project with this Youth CLI or restore the locked input"
        )))
    }
}

fn mismatch<T: std::fmt::Display>(field: &str, actual: T, expected: T) -> Result<(), ProjectError> {
    Err(ProjectError::Contract(format!(
        "{field} mismatch: found {actual}, expected {expected}; regenerate the project with this Youth CLI"
    )))
}

fn is_lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Computes the portable, length-delimited digest of a vendored WIT tree.
pub fn hash_wit_tree(root: impl AsRef<Path>) -> Result<String, ProjectError> {
    let root = root.as_ref();
    let mut files = BTreeMap::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| ProjectError::Contract(error.to_string()))?;
        let file_type = entry.file_type();
        if file_type.is_symlink() {
            return Err(ProjectError::Contract(format!(
                "vendored WIT contains a symlink: {}",
                entry.path().display()
            )));
        }
        if file_type.is_file() {
            let relative = entry
                .path()
                .strip_prefix(root)
                .map_err(|error| ProjectError::Contract(format!("invalid WIT path: {error}")))?;
            let normalized = relative
                .components()
                .map(|part| part.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            files.insert(normalized, entry.path().to_path_buf());
        }
    }
    if files.is_empty() {
        return Err(ProjectError::Contract(
            "vendored WIT directory contains no regular files".into(),
        ));
    }
    let mut digest = Sha256::new();
    for (relative, path) in files {
        let path_bytes = relative.as_bytes();
        let path_len = u32::try_from(path_bytes.len())
            .map_err(|_| ProjectError::Contract("WIT relative path is too long".into()))?;
        let mut file = fs::File::open(&path).map_err(|source| ProjectError::Io {
            path: path.clone(),
            source,
        })?;
        let length = file
            .metadata()
            .map_err(|source| ProjectError::Io {
                path: path.clone(),
                source,
            })?
            .len();
        digest.update(path_len.to_be_bytes());
        digest.update(path_bytes);
        digest.update(length.to_be_bytes());
        let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(0));
        file.read_to_end(&mut bytes)
            .map_err(|source| ProjectError::Io { path, source })?;
        digest.update(bytes);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn verify_cargo_manifest(path: &Path, lock: &Lock) -> Result<(), ProjectError> {
    let value: toml::Value = read_toml(path)?;
    let dependency = value
        .get("dependencies")
        .and_then(|value| value.get("youth-sdk"))
        .ok_or_else(|| ProjectError::Contract("Cargo.toml has no youth-sdk dependency".into()))?;
    let table = dependency.as_table().ok_or_else(|| {
        ProjectError::Contract("Cargo.toml youth-sdk dependency must pin git and rev".into())
    })?;
    let git = table.get("git").and_then(toml::Value::as_str).unwrap_or("");
    let revision = table.get("rev").and_then(toml::Value::as_str).unwrap_or("");
    compare("Cargo.toml youth-sdk.git", git, &lock.sdk_source)?;
    compare("Cargo.toml youth-sdk.rev", revision, &lock.sdk_revision)
}

fn verify_cargo_lock(path: &Path, lock: &Lock) -> Result<(), ProjectError> {
    let value: toml::Value = read_toml(path)?;
    let packages = value
        .get("package")
        .and_then(toml::Value::as_array)
        .ok_or_else(|| ProjectError::Contract("Cargo.lock has no package records".into()))?;
    let sdk = packages
        .iter()
        .filter(|package| package.get("name").and_then(toml::Value::as_str) == Some("youth-sdk"))
        .collect::<Vec<_>>();
    if sdk.len() != 1 {
        return Err(ProjectError::Contract(format!(
            "Cargo.lock must contain exactly one youth-sdk package, found {}",
            sdk.len()
        )));
    }
    let source = sdk[0]
        .get("source")
        .and_then(toml::Value::as_str)
        .unwrap_or("");
    if !source.starts_with(&format!("git+{}?rev=", lock.sdk_source))
        || !source.ends_with(&format!("#{}", lock.sdk_revision))
    {
        return Err(ProjectError::Contract(format!(
            "Cargo.lock youth-sdk source mismatch: found {source:?}, expected Git revision {:?}; run cargo update -p youth-sdk --precise {}",
            lock.sdk_revision, lock.sdk_revision
        )));
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum ProjectError {
    #[error("{0}")]
    Contract(String),
    #[error("could not read {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid TOML in {path}: {source}")]
    Toml {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::*;

    const WIT: &str = "package youth:app@0.0.2;\n";

    fn fixture() -> TempDir {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = temporary.path();
        fs::create_dir_all(root.join("wit/youth")).expect("WIT directory");
        fs::write(root.join("wit/youth/app.wit"), WIT).expect("WIT fixture");
        let hash = hash_wit_tree(root.join("wit/youth")).expect("WIT hash");
        fs::write(
            root.join(MANIFEST_NAME),
            r#"[app]
id = "dev.saman.tally"
name = "Tally"
protocol = "0.0.2"

[build]
language = "rust"
package = "tally"
target = "wasm32-wasip2"

[development]
state = ".youth/state"
"#,
        )
        .expect("manifest fixture");
        fs::write(
            root.join(LOCK_NAME),
            format!(
                r#"lock-version = 1
protocol = "0.0.2"
sdk-source = "{SDK_SOURCE}"
sdk-revision = "{SDK_REVISION}"
wit-sha256 = "{hash}"
cli-version = "{CLI_VERSION}"
template-version = 1
"#
            ),
        )
        .expect("lock fixture");
        fs::write(
            root.join("Cargo.toml"),
            format!(
                r#"[package]
name = "tally"
version = "0.0.1"

[dependencies]
youth-sdk = {{ git = "{SDK_SOURCE}", rev = "{SDK_REVISION}" }}
"#
            ),
        )
        .expect("Cargo manifest fixture");
        fs::write(
            root.join("Cargo.lock"),
            format!(
                r#"version = 4

[[package]]
name = "youth-sdk"
version = "0.0.1"
source = "git+{SDK_SOURCE}?rev={SDK_REVISION}#{SDK_REVISION}"
"#
            ),
        )
        .expect("Cargo lock fixture");
        temporary
    }

    #[test]
    fn discovers_upward_and_derives_paths() {
        let fixture = fixture();
        let nested = fixture.path().join("src/deep");
        fs::create_dir_all(&nested).expect("nested directory");
        let project = Project::discover(&nested).expect("project");
        assert_eq!(project.root(), fixture.path());
        assert_eq!(
            project.cargo_component(false),
            fixture.path().join("target/wasm32-wasip2/debug/tally.wasm")
        );
        assert_eq!(
            project.dist_component(),
            fixture.path().join("dist/dev.saman.tally.wasm")
        );
    }

    #[test]
    fn verifies_exact_locked_cargo_and_wit_inputs() {
        let fixture = fixture();
        let mut project = Project::load(fixture.path()).expect("project");
        project.lock.wit_sha256 = hash_wit_tree(fixture.path().join("wit/youth")).expect("hash");
        // This fixture does not use the production template snapshot, so the
        // lower-level Cargo checks are exercised directly.
        verify_cargo_manifest(&fixture.path().join("Cargo.toml"), &project.lock)
            .expect("Cargo manifest");
        verify_cargo_lock(&fixture.path().join("Cargo.lock"), &project.lock).expect("Cargo lock");
    }

    #[test]
    fn rejects_unknown_fields_and_escaping_state_paths() {
        let unknown_fixture = fixture();
        let manifest = unknown_fixture.path().join(MANIFEST_NAME);
        let text = fs::read_to_string(&manifest).expect("manifest");
        fs::write(&manifest, format!("{text}\nunknown = true\n")).expect("change manifest");
        assert!(Project::load(unknown_fixture.path()).is_err());

        let escaping_fixture = fixture();
        let manifest = escaping_fixture.path().join(MANIFEST_NAME);
        let text = fs::read_to_string(&manifest)
            .expect("manifest")
            .replace(".youth/state", "../outside");
        fs::write(&manifest, text).expect("change manifest");
        assert!(
            Project::load(escaping_fixture.path())
                .expect_err("escaping path")
                .to_string()
                .contains("must not escape")
        );
    }

    #[test]
    fn hash_is_path_and_content_sensitive() {
        let fixture = fixture();
        let root = fixture.path().join("wit/youth");
        let first = hash_wit_tree(&root).expect("first hash");
        fs::write(root.join("app.wit"), format!("{WIT}// changed\n")).expect("changed WIT");
        let second = hash_wit_tree(&root).expect("second hash");
        assert_ne!(first, second);
        fs::rename(root.join("app.wit"), root.join("renamed.wit")).expect("renamed WIT");
        let third = hash_wit_tree(&root).expect("third hash");
        assert_ne!(second, third);
    }

    #[cfg(unix)]
    #[test]
    fn rejects_wit_symlinks() {
        use std::os::unix::fs::symlink;

        let fixture = fixture();
        let root = fixture.path().join("wit/youth");
        symlink(root.join("app.wit"), root.join("alias.wit")).expect("symlink");
        assert!(
            hash_wit_tree(root)
                .expect_err("symlink rejection")
                .to_string()
                .contains("symlink")
        );
    }
}
