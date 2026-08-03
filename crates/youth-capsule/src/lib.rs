#![forbid(unsafe_code)]

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;
use youth_state::AppId;

pub const MANIFEST_NAME: &str = "manifest.toml";
pub const PROVENANCE_NAME: &str = "provenance.toml";
pub const CAPSULE_FORMAT_VERSION: u32 = 1;

const MAX_APP_NAME_BYTES: usize = 256;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct Sha256Digest(String);

impl TryFrom<String> for Sha256Digest {
    type Error = Sha256DigestError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.len() != 64 {
            return Err(Sha256DigestError {
                reason: "must contain exactly 64 characters",
            });
        }
        if !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(Sha256DigestError {
                reason: "must contain only lowercase hexadecimal characters",
            });
        }
        Ok(Self(value))
    }
}

impl FromStr for Sha256Digest {
    type Err = Sha256DigestError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::try_from(value.to_owned())
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for Sha256Digest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid SHA-256 digest: {reason}")]
pub struct Sha256DigestError {
    reason: &'static str,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CapsulePath(String);

impl TryFrom<String> for CapsulePath {
    type Error = CapsulePathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(CapsulePathError {
                reason: "must not be empty",
            });
        }
        if value.contains('\0') {
            return Err(CapsulePathError {
                reason: "must not contain a NUL byte",
            });
        }
        if value.contains('\\') {
            return Err(CapsulePathError {
                reason: "must use '/' separators",
            });
        }
        let bytes = value.as_bytes();
        if value.starts_with('/')
            || (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        {
            return Err(CapsulePathError {
                reason: "must be relative",
            });
        }
        if value.split('/').any(|component| component.is_empty()) {
            return Err(CapsulePathError {
                reason: "must not contain empty path components",
            });
        }
        if value
            .split('/')
            .any(|component| matches!(component, "." | ".."))
        {
            return Err(CapsulePathError {
                reason: "must not contain '.' or '..' components",
            });
        }
        Ok(Self(value))
    }
}

impl FromStr for CapsulePath {
    type Err = CapsulePathError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::try_from(value.to_owned())
    }
}

impl fmt::Display for CapsulePath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl AsRef<str> for CapsulePath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl Serialize for CapsulePath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for CapsulePath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::try_from(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
#[error("invalid capsule path: {reason}")]
pub struct CapsulePathError {
    reason: &'static str,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequiredProfile {
    pub protocol: String,
    pub wit_sha256: Sha256Digest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ComponentRecord {
    pub path: CapsulePath,
    pub sha256: Sha256Digest,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapsuleManifest {
    pub capsule_format_version: u32,
    #[serde(with = "app_id_serde")]
    pub app_id: AppId,
    #[serde(deserialize_with = "deserialize_app_name")]
    pub app_name: String,
    pub required_profile: RequiredProfile,
    pub component: ComponentRecord,
}

impl CapsuleManifest {
    pub fn new(
        capsule_format_version: u32,
        app_id: AppId,
        app_name: String,
        required_profile: RequiredProfile,
        component: ComponentRecord,
    ) -> Result<Self, CapsuleManifestError> {
        validate_app_name(&app_name)?;
        Ok(Self {
            capsule_format_version,
            app_id,
            app_name,
            required_profile,
            component,
        })
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum CapsuleManifestError {
    #[error("application name is {found_bytes} bytes; the maximum is {max_bytes} bytes")]
    AppNameTooLong {
        max_bytes: usize,
        found_bytes: usize,
    },
}

fn validate_app_name(value: &str) -> Result<(), CapsuleManifestError> {
    if value.len() > MAX_APP_NAME_BYTES {
        return Err(CapsuleManifestError::AppNameTooLong {
            max_bytes: MAX_APP_NAME_BYTES,
            found_bytes: value.len(),
        });
    }
    Ok(())
}

fn deserialize_app_name<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;
    validate_app_name(&value).map_err(serde::de::Error::custom)?;
    Ok(value)
}

mod app_id_serde {
    use serde::{Deserialize, Deserializer, Serializer};
    use youth_state::AppId;

    pub fn serialize<S>(app_id: &AppId, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(app_id.as_str())
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<AppId, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        AppId::parse(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CapsuleProvenance {
    pub app_commit: Option<String>,
    pub youth_commit: Option<String>,
    pub sdk_revision: String,
    pub template_version: u32,
    pub cli_version: String,
    pub build_profile: String,
    pub rustc_version: String,
    pub cargo_version: String,
    pub built_at: String,
}

pub mod verify {
    use std::fs::{self, File};
    use std::io::{self, Read};
    use std::path::Path;

    use sha2::{Digest, Sha256};
    use thiserror::Error;

    use crate::{
        CAPSULE_FORMAT_VERSION, CapsuleManifest, CapsulePath, MANIFEST_NAME, Sha256Digest,
    };

    const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

    pub fn verify_capsule(
        capsule_directory: impl AsRef<Path>,
    ) -> Result<CapsuleManifest, CapsuleVerifyError> {
        let capsule_directory = capsule_directory.as_ref();
        let manifest = read_manifest(capsule_directory)?;
        if manifest.capsule_format_version != CAPSULE_FORMAT_VERSION {
            return Err(CapsuleVerifyError::FormatVersion {
                expected: CAPSULE_FORMAT_VERSION,
                found: manifest.capsule_format_version,
            });
        }

        let relative_path = manifest.component.path.clone();
        let component_path = capsule_directory.join(relative_path.as_ref());
        let component = fs::read(&component_path).map_err(|source| match source.kind() {
            io::ErrorKind::NotFound => CapsuleVerifyError::ComponentNotFound {
                path: relative_path.clone(),
            },
            _ => CapsuleVerifyError::ComponentUnreadable {
                path: relative_path.clone(),
                source,
            },
        })?;

        let found_hash = Sha256Digest(format!("{:x}", Sha256::digest(&component)));
        if found_hash != manifest.component.sha256 {
            return Err(CapsuleVerifyError::HashMismatch {
                path: relative_path,
                expected: manifest.component.sha256.clone(),
                found: found_hash,
            });
        }

        let found_size = u64::try_from(component.len()).unwrap_or(u64::MAX);
        if found_size != manifest.component.size_bytes {
            return Err(CapsuleVerifyError::SizeMismatch {
                path: relative_path,
                expected: manifest.component.size_bytes,
                found: found_size,
            });
        }

        Ok(manifest)
    }

    fn read_manifest(capsule_directory: &Path) -> Result<CapsuleManifest, CapsuleVerifyError> {
        let manifest_path = capsule_directory.join(MANIFEST_NAME);
        let file = File::open(manifest_path).map_err(|source| match source.kind() {
            io::ErrorKind::NotFound => CapsuleVerifyError::ManifestNotFound,
            _ => CapsuleVerifyError::ManifestUnreadable { source },
        })?;
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|source| CapsuleVerifyError::ManifestUnreadable { source })?;
        if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > MAX_MANIFEST_BYTES {
            return Err(CapsuleVerifyError::ManifestTooLarge {
                max_bytes: MAX_MANIFEST_BYTES,
            });
        }
        let text = std::str::from_utf8(&bytes).map_err(|_| CapsuleVerifyError::ManifestParse)?;
        toml::from_str(text).map_err(|_| CapsuleVerifyError::ManifestParse)
    }

    #[derive(Debug, Error)]
    pub enum CapsuleVerifyError {
        #[error("The capsule manifest 'manifest.toml' was not found.")]
        ManifestNotFound,
        #[error("The capsule manifest 'manifest.toml' could not be read.")]
        ManifestUnreadable {
            #[source]
            source: io::Error,
        },
        #[error(
            "The capsule manifest 'manifest.toml' is larger than the supported {max_bytes}-byte limit."
        )]
        ManifestTooLarge { max_bytes: u64 },
        #[error("The capsule manifest 'manifest.toml' is not valid.")]
        ManifestParse,
        #[error(
            "This capsule uses format version {found}, but this launcher supports version {expected}."
        )]
        FormatVersion { expected: u32, found: u32 },
        #[error("The capsule component '{path}' was not found.")]
        ComponentNotFound { path: CapsulePath },
        #[error("The capsule component '{path}' could not be read.")]
        ComponentUnreadable {
            path: CapsulePath,
            #[source]
            source: io::Error,
        },
        #[error(
            "The capsule component '{path}' failed its integrity check: expected SHA-256 {expected}, found {found}."
        )]
        HashMismatch {
            path: CapsulePath,
            expected: Sha256Digest,
            found: Sha256Digest,
        },
        #[error(
            "The capsule component '{path}' has the wrong size: expected {expected} bytes, found {found} bytes."
        )]
        SizeMismatch {
            path: CapsulePath,
            expected: u64,
            found: u64,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use sha2::{Digest, Sha256};

    use super::verify::{CapsuleVerifyError, verify_capsule};
    use super::*;

    const VALID_DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn digest(bytes: &[u8]) -> Sha256Digest {
        format!("{:x}", Sha256::digest(bytes)).parse().unwrap()
    }

    fn manifest(component: &[u8]) -> CapsuleManifest {
        CapsuleManifest::new(
            CAPSULE_FORMAT_VERSION,
            AppId::parse("dev.youth.scratchpad").unwrap(),
            "Scratchpad".to_owned(),
            RequiredProfile {
                protocol: "0.0.8".to_owned(),
                wit_sha256: VALID_DIGEST.parse().unwrap(),
            },
            ComponentRecord {
                path: "component.wasm".parse().unwrap(),
                sha256: digest(component),
                size_bytes: u64::try_from(component.len()).unwrap(),
            },
        )
        .unwrap()
    }

    fn write_manifest(directory: &Path, manifest: &CapsuleManifest) {
        fs::write(
            directory.join(MANIFEST_NAME),
            toml::to_string(manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn manifest_toml_round_trip_is_byte_for_byte_equivalent() {
        let manifest = manifest(b"component");
        let encoded = toml::to_string(&manifest).unwrap();
        let decoded: CapsuleManifest = toml::from_str(&encoded).unwrap();
        let reencoded = toml::to_string(&decoded).unwrap();

        assert_eq!(decoded, manifest);
        assert_eq!(reencoded, encoded);
    }

    #[test]
    fn sha256_digest_rejects_invalid_values() {
        assert!(Sha256Digest::from_str(&"a".repeat(63)).is_err());
        assert!(Sha256Digest::from_str(&"a".repeat(65)).is_err());
        assert!(Sha256Digest::from_str(&"A".repeat(64)).is_err());
        assert!(Sha256Digest::from_str(&"g".repeat(64)).is_err());
    }

    #[test]
    fn capsule_path_rejects_invalid_values() {
        for invalid in [
            "",
            "/component.wasm",
            "C:/component.wasm",
            "./component.wasm",
            "payload/../component.wasm",
            "payload\\component.wasm",
            "payload/\0component.wasm",
            "payload//component.wasm",
        ] {
            assert!(
                CapsulePath::from_str(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
    }

    #[test]
    fn manifest_rejects_unknown_top_level_field() {
        let encoded = toml::to_string(&manifest(b"component")).unwrap();
        let encoded = encoded.replace(
            "[required_profile]",
            "unexpected = true\n\n[required_profile]",
        );
        assert!(toml::from_str::<CapsuleManifest>(&encoded).is_err());
    }

    #[test]
    fn manifest_rejects_oversized_app_name() {
        let mut encoded = toml::to_string(&manifest(b"component")).unwrap();
        encoded = encoded.replace("Scratchpad", &"a".repeat(257));
        assert!(toml::from_str::<CapsuleManifest>(&encoded).is_err());
    }

    #[test]
    fn verifier_rejects_wrong_format_version() {
        let directory = tempfile::tempdir().unwrap();
        let mut manifest = manifest(b"component");
        manifest.capsule_format_version = CAPSULE_FORMAT_VERSION + 1;
        write_manifest(directory.path(), &manifest);

        assert!(matches!(
            verify_capsule(directory.path()),
            Err(CapsuleVerifyError::FormatVersion {
                expected: CAPSULE_FORMAT_VERSION,
                found
            }) if found == CAPSULE_FORMAT_VERSION + 1
        ));
    }

    #[test]
    fn verifier_accepts_matching_component() {
        let directory = tempfile::tempdir().unwrap();
        let component = b"valid WebAssembly component bytes";
        fs::write(directory.path().join("component.wasm"), component).unwrap();
        let manifest = manifest(component);
        write_manifest(directory.path(), &manifest);

        assert_eq!(verify_capsule(directory.path()).unwrap(), manifest);
    }

    #[test]
    fn verifier_rejects_tampered_component() {
        let directory = tempfile::tempdir().unwrap();
        let component = b"valid WebAssembly component bytes";
        fs::write(directory.path().join("component.wasm"), component).unwrap();
        write_manifest(directory.path(), &manifest(component));
        fs::write(
            directory.path().join("component.wasm"),
            b"valid WebAssembly component byte!",
        )
        .unwrap();

        assert!(matches!(
            verify_capsule(directory.path()),
            Err(CapsuleVerifyError::HashMismatch { .. })
        ));
    }

    #[test]
    fn verifier_rejects_recorded_hash_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let component = b"component";
        fs::write(directory.path().join("component.wasm"), component).unwrap();
        let mut manifest = manifest(component);
        manifest.component.sha256 = VALID_DIGEST.parse().unwrap();
        write_manifest(directory.path(), &manifest);

        assert!(matches!(
            verify_capsule(directory.path()),
            Err(CapsuleVerifyError::HashMismatch { .. })
        ));
    }

    #[test]
    fn verifier_rejects_recorded_size_mismatch() {
        let directory = tempfile::tempdir().unwrap();
        let component = b"component";
        fs::write(directory.path().join("component.wasm"), component).unwrap();
        let mut manifest = manifest(component);
        manifest.component.size_bytes += 1;
        write_manifest(directory.path(), &manifest);

        assert!(matches!(
            verify_capsule(directory.path()),
            Err(CapsuleVerifyError::SizeMismatch { .. })
        ));
    }
}
