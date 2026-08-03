use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use cap_std::ambient_authority;
use cap_std::fs::{Dir, Metadata, Permissions};
use sha2::{Digest, Sha256};

use crate::WorkspaceGrant;

const MAX_RELATIVE_PATH_BYTES: usize = 1024;
const MAX_RELATIVE_COMPONENTS: usize = 32;
const UTF8_BOM: &[u8; 3] = b"\xef\xbb\xbf";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TextEncoding {
    Utf8,
    Utf8WithBom,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentHandle {
    pub(crate) id: u64,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DocumentVersion {
    pub(crate) id: u64,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SaveRequest {
    pub(crate) id: u64,
    pub(crate) generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaveFailure {
    Conflict,
    Missing,
    WrongType,
    PermissionDenied,
    Unavailable,
    Internal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SaveOutcome {
    Saved {
        document: DocumentHandle,
        version: DocumentVersion,
        saved_edit_sequence: u64,
        still_dirty: bool,
    },
    Failed(SaveFailure),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SaveCompletion {
    pub request: SaveRequest,
    pub outcome: SaveOutcome,
    pub(crate) editor: youth_tree::NodeId,
}

impl SaveCompletion {
    pub(crate) fn as_wire_v008(&self) -> crate::bindings::v008::youth::app::ui::SaveCompletion {
        use crate::bindings::v008::youth::app::ui;
        let outcome = match self.outcome {
            SaveOutcome::Saved {
                document,
                version,
                saved_edit_sequence,
                still_dirty,
            } => ui::SaveOutcome::Saved(ui::SavedDocument {
                document: ui::DocumentHandle {
                    id: document.id,
                    generation: document.generation,
                },
                version: ui::DocumentVersion {
                    id: version.id,
                    generation: version.generation,
                },
                saved_edit_sequence,
                still_dirty,
            }),
            SaveOutcome::Failed(failure) => ui::SaveOutcome::Failed(match failure {
                SaveFailure::Conflict => ui::TextDocumentErrorCode::Conflict,
                SaveFailure::Missing => ui::TextDocumentErrorCode::Missing,
                SaveFailure::WrongType => ui::TextDocumentErrorCode::WrongType,
                SaveFailure::PermissionDenied => ui::TextDocumentErrorCode::PermissionDenied,
                SaveFailure::Unavailable => ui::TextDocumentErrorCode::Unavailable,
                SaveFailure::Internal => ui::TextDocumentErrorCode::Internal,
            }),
        };
        ui::SaveCompletion {
            request: ui::SaveRequest {
                id: self.request.id,
                generation: self.request.generation,
            },
            outcome,
        }
    }

    pub(crate) fn as_wire_v009(&self) -> crate::bindings::v009::youth::app::ui::SaveCompletion {
        use crate::bindings::v009::youth::app::ui;
        let outcome = match self.outcome {
            SaveOutcome::Saved {
                document,
                version,
                saved_edit_sequence,
                still_dirty,
            } => ui::SaveOutcome::Saved(ui::SavedDocument {
                document: ui::DocumentHandle {
                    id: document.id,
                    generation: document.generation,
                },
                version: ui::DocumentVersion {
                    id: version.id,
                    generation: version.generation,
                },
                saved_edit_sequence,
                still_dirty,
            }),
            SaveOutcome::Failed(failure) => ui::SaveOutcome::Failed(match failure {
                SaveFailure::Conflict => ui::TextDocumentErrorCode::Conflict,
                SaveFailure::Missing => ui::TextDocumentErrorCode::Missing,
                SaveFailure::WrongType => ui::TextDocumentErrorCode::WrongType,
                SaveFailure::PermissionDenied => ui::TextDocumentErrorCode::PermissionDenied,
                SaveFailure::Unavailable => ui::TextDocumentErrorCode::Unavailable,
                SaveFailure::Internal => ui::TextDocumentErrorCode::Internal,
            }),
        };
        ui::SaveCompletion {
            request: ui::SaveRequest {
                id: self.request.id,
                generation: self.request.generation,
            },
            outcome,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TextDocumentError {
    #[error("the text-document path is invalid")]
    InvalidPath,
    #[error("the text-document path contains a symbolic link")]
    Symlink,
    #[error("the text document is missing")]
    Missing,
    #[error("the text-document path is not a regular file")]
    WrongType,
    #[error("the text document is not valid UTF-8")]
    InvalidUtf8,
    #[error("the text document exceeds the supported editor limit")]
    TooLarge,
    #[error("the text document is read-only")]
    PermissionDenied,
    #[error("text-document I/O failed")]
    Io(#[source] std::io::Error),
}

#[derive(Clone)]
pub(crate) struct TextDocumentSession {
    root: Arc<Dir>,
    relative_document: PathBuf,
    components: Arc<[OsString]>,
    filename: OsString,
    relative_path: String,
    filename_display: String,
    state: Arc<Mutex<DocumentState>>,
}

struct DocumentState {
    handle: DocumentHandle,
    version: DocumentVersion,
    encoding: TextEncoding,
    accepted_bytes: Arc<[u8]>,
    editor_text: Arc<str>,
    next_request: u64,
    pending: Option<SaveRequest>,
}

trait AtomicFileReplacer {
    fn replace(
        &self,
        candidate: cap_tempfile::TempFile<'_>,
        destination: &Path,
    ) -> std::io::Result<()>;
}

struct CapabilityRelativeReplacer;

impl AtomicFileReplacer for CapabilityRelativeReplacer {
    fn replace(
        &self,
        candidate: cap_tempfile::TempFile<'_>,
        destination: &Path,
    ) -> std::io::Result<()> {
        // `TempFile` is created in the retained destination directory and its
        // replacement operation remains relative to that directory
        // capability. This gives every supported host one containment model
        // and never reconstructs an ambient destination path.
        candidate.replace(destination)
    }
}

#[derive(Clone)]
pub(crate) struct SaveJob {
    session: TextDocumentSession,
    pub(crate) request: SaveRequest,
    pub(crate) editor: youth_tree::NodeId,
    pub(crate) edit_sequence: u64,
    candidate: Arc<[u8]>,
    expected: Arc<[u8]>,
}

#[derive(Clone, Debug)]
pub(crate) struct DocumentInfo {
    pub handle: DocumentHandle,
    pub version: DocumentVersion,
    pub relative_path: String,
    pub filename: String,
}

impl TextDocumentSession {
    pub(crate) fn open(
        grant: &WorkspaceGrant,
        max_editor_bytes: usize,
    ) -> Result<Self, TextDocumentError> {
        let components = validate_relative_path(&grant.document)?;
        let root = Dir::open_ambient_dir(&grant.root, ambient_authority())
            .map_err(TextDocumentError::Io)?;
        reject_symlinks(&root, &components)?;
        let metadata = root
            .metadata(&grant.document)
            .map_err(classify_open_error)?;
        if !metadata.is_file() {
            return Err(TextDocumentError::WrongType);
        }
        let bytes = read_file(&root, &grant.document)?;
        let (encoding, visible) = decode(&bytes, max_editor_bytes)?;
        let filename = grant
            .document
            .file_name()
            .ok_or(TextDocumentError::InvalidPath)?
            .to_os_string();
        let filename_display = filename
            .to_str()
            .ok_or(TextDocumentError::InvalidPath)?
            .to_owned();
        let relative_path = components
            .iter()
            .map(|component| component.to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        let version = file_version(&bytes, 1);
        Ok(Self {
            root: Arc::new(root),
            relative_document: grant.document.clone(),
            components: Arc::from(components),
            filename,
            relative_path,
            filename_display,
            state: Arc::new(Mutex::new(DocumentState {
                handle: DocumentHandle {
                    id: 1,
                    generation: 1,
                },
                version,
                encoding,
                accepted_bytes: Arc::from(bytes),
                editor_text: Arc::from(visible),
                next_request: 1,
                pending: None,
            })),
        })
    }

    pub(crate) fn info(&self) -> DocumentInfo {
        let state = self
            .state
            .lock()
            .expect("text-document mutex is not poisoned");
        DocumentInfo {
            handle: state.handle,
            version: state.version,
            relative_path: self.relative_path.clone(),
            filename: self.filename_display.clone(),
        }
    }

    pub(crate) fn editor_text(&self, handle: DocumentHandle) -> Option<(u64, Arc<str>)> {
        let state = self
            .state
            .lock()
            .expect("text-document mutex is not poisoned");
        (state.handle == handle).then(|| (state.version.generation, Arc::clone(&state.editor_text)))
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.state
            .lock()
            .expect("text-document mutex is not poisoned")
            .pending
            .is_some()
    }

    pub(crate) fn prepare_save(
        &self,
        document: DocumentHandle,
        editor: youth_tree::NodeId,
        edit_sequence: u64,
        text: String,
        max_editor_bytes: usize,
    ) -> Result<SaveJob, TextDocumentError> {
        if text.len() > max_editor_bytes {
            return Err(TextDocumentError::TooLarge);
        }
        let mut state = self
            .state
            .lock()
            .expect("text-document mutex is not poisoned");
        if state.handle != document {
            return Err(TextDocumentError::InvalidPath);
        }
        if state.pending.is_some() {
            return Err(TextDocumentError::Io(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "a save is already pending",
            )));
        }
        let request = SaveRequest {
            id: state.next_request,
            generation: state.handle.generation,
        };
        state.next_request = state.next_request.checked_add(1).unwrap_or(1);
        let candidate: Arc<[u8]> = match state.encoding {
            TextEncoding::Utf8 => Arc::from(text.into_bytes()),
            TextEncoding::Utf8WithBom => {
                let mut bytes = Vec::with_capacity(UTF8_BOM.len() + text.len());
                bytes.extend_from_slice(UTF8_BOM);
                bytes.extend_from_slice(text.as_bytes());
                Arc::from(bytes)
            }
        };
        Ok(SaveJob {
            session: self.clone(),
            request,
            editor,
            edit_sequence,
            candidate,
            expected: Arc::clone(&state.accepted_bytes),
        })
    }

    pub(crate) fn mark_pending(&self, request: SaveRequest) -> Result<(), TextDocumentError> {
        let mut state = self
            .state
            .lock()
            .expect("text-document mutex is not poisoned");
        if state.pending.is_some() {
            return Err(TextDocumentError::Io(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "a save is already pending",
            )));
        }
        state.pending = Some(request);
        Ok(())
    }

    fn finish(&self, request: SaveRequest, bytes: Arc<[u8]>) -> DocumentVersion {
        let mut state = self
            .state
            .lock()
            .expect("text-document mutex is not poisoned");
        state.pending = None;
        let generation = state.version.generation.checked_add(1).unwrap_or(1);
        state.version = file_version(&bytes, generation);
        state.accepted_bytes = Arc::clone(&bytes);
        let visible = if bytes.starts_with(UTF8_BOM) {
            &bytes[UTF8_BOM.len()..]
        } else {
            &bytes
        };
        state.editor_text = Arc::from(
            std::str::from_utf8(visible).expect("save candidates are encoded from UTF-8"),
        );
        debug_assert_eq!(state.pending, None);
        let _ = request;
        state.version
    }

    fn fail(&self, request: SaveRequest) {
        let mut state = self
            .state
            .lock()
            .expect("text-document mutex is not poisoned");
        if state.pending == Some(request) {
            state.pending = None;
        }
    }
}

impl SaveJob {
    pub(crate) fn execute(self) -> SaveCompletion {
        let result = execute_save(&self);
        let outcome = match result {
            Ok(()) => {
                let version = self
                    .session
                    .finish(self.request, Arc::clone(&self.candidate));
                let document = self.session.info().handle;
                SaveOutcome::Saved {
                    document,
                    version,
                    saved_edit_sequence: self.edit_sequence,
                    still_dirty: false,
                }
            }
            Err(failure) => {
                self.session.fail(self.request);
                SaveOutcome::Failed(failure)
            }
        };
        SaveCompletion {
            request: self.request,
            outcome,
            editor: self.editor,
        }
    }

    pub(crate) fn enqueue_failed(self) -> SaveCompletion {
        self.session.fail(self.request);
        SaveCompletion {
            request: self.request,
            outcome: SaveOutcome::Failed(SaveFailure::Unavailable),
            editor: self.editor,
        }
    }
}

fn execute_save(job: &SaveJob) -> Result<(), SaveFailure> {
    reject_symlinks(&job.session.root, &job.session.components).map_err(classify_save_open)?;
    let parent = job
        .session
        .relative_document
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory = job
        .session
        .root
        .open_dir(parent)
        .map_err(classify_save_io)?;
    let current_metadata = directory
        .symlink_metadata(&job.session.filename)
        .map_err(classify_save_io)?;
    if current_metadata.file_type().is_symlink() {
        return Err(SaveFailure::Conflict);
    }
    if !current_metadata.is_file() {
        return Err(SaveFailure::WrongType);
    }
    if current_metadata.permissions().readonly() {
        return Err(SaveFailure::PermissionDenied);
    }

    let mut candidate = cap_tempfile::TempFile::new(&directory).map_err(classify_save_io)?;
    set_private_permissions(candidate.as_file()).map_err(classify_save_io)?;
    candidate
        .write_all(&job.candidate)
        .map_err(classify_save_io)?;
    candidate.flush().map_err(classify_save_io)?;
    candidate.as_file().sync_all().map_err(classify_save_io)?;
    candidate
        .as_file()
        .set_permissions(current_metadata.permissions())
        .map_err(classify_save_io)?;

    let current =
        read_file(&directory, Path::new(&job.session.filename)).map_err(|error| match error {
            TextDocumentError::Missing => SaveFailure::Missing,
            TextDocumentError::WrongType | TextDocumentError::Symlink => SaveFailure::WrongType,
            TextDocumentError::PermissionDenied => SaveFailure::PermissionDenied,
            TextDocumentError::InvalidPath
            | TextDocumentError::InvalidUtf8
            | TextDocumentError::TooLarge
            | TextDocumentError::Io(_) => SaveFailure::Unavailable,
        })?;
    if current.as_slice() != job.expected.as_ref() {
        return Err(SaveFailure::Conflict);
    }
    CapabilityRelativeReplacer
        .replace(candidate, Path::new(&job.session.filename))
        .map_err(classify_save_io)?;
    sync_directory(&directory).map_err(classify_save_io)?;
    Ok(())
}

fn validate_relative_path(path: &Path) -> Result<Vec<OsString>, TextDocumentError> {
    let text = path.to_str().ok_or(TextDocumentError::InvalidPath)?;
    if text.is_empty() || text.len() > MAX_RELATIVE_PATH_BYTES {
        return Err(TextDocumentError::InvalidPath);
    }
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) if !value.is_empty() => components.push(value.to_os_string()),
            Component::Prefix(_)
            | Component::RootDir
            | Component::CurDir
            | Component::ParentDir
            | Component::Normal(_) => return Err(TextDocumentError::InvalidPath),
        }
    }
    if components.is_empty() || components.len() > MAX_RELATIVE_COMPONENTS {
        return Err(TextDocumentError::InvalidPath);
    }
    Ok(components)
}

fn reject_symlinks(dir: &Dir, components: &[OsString]) -> Result<(), TextDocumentError> {
    let mut path = PathBuf::new();
    for component in components {
        path.push(component);
        let metadata = dir.symlink_metadata(&path).map_err(classify_open_error)?;
        if metadata.file_type().is_symlink() {
            return Err(TextDocumentError::Symlink);
        }
    }
    Ok(())
}

fn read_file(dir: &Dir, path: &Path) -> Result<Vec<u8>, TextDocumentError> {
    let metadata = dir.symlink_metadata(path).map_err(classify_open_error)?;
    if metadata.file_type().is_symlink() {
        return Err(TextDocumentError::Symlink);
    }
    if !metadata.is_file() {
        return Err(TextDocumentError::WrongType);
    }
    let mut file = dir.open(path).map_err(classify_open_error)?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(TextDocumentError::Io)?;
    Ok(bytes)
}

fn decode(
    bytes: &[u8],
    max_editor_bytes: usize,
) -> Result<(TextEncoding, String), TextDocumentError> {
    let (encoding, visible) = if let Some(visible) = bytes.strip_prefix(UTF8_BOM) {
        (TextEncoding::Utf8WithBom, visible)
    } else {
        (TextEncoding::Utf8, bytes)
    };
    if visible.len() > max_editor_bytes {
        return Err(TextDocumentError::TooLarge);
    }
    let text = std::str::from_utf8(visible).map_err(|_| TextDocumentError::InvalidUtf8)?;
    Ok((encoding, text.to_owned()))
}

fn file_version(bytes: &[u8], generation: u64) -> DocumentVersion {
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    let mut id_bytes = [0_u8; 8];
    id_bytes.copy_from_slice(&digest[..8]);
    let mut id = u64::from_be_bytes(id_bytes);
    if id == 0 {
        id = 1;
    }
    DocumentVersion { id, generation }
}

fn classify_open_error(error: std::io::Error) -> TextDocumentError {
    match error.kind() {
        std::io::ErrorKind::NotFound => TextDocumentError::Missing,
        std::io::ErrorKind::PermissionDenied => TextDocumentError::PermissionDenied,
        _ => TextDocumentError::Io(error),
    }
}

fn classify_save_io(error: std::io::Error) -> SaveFailure {
    match error.kind() {
        std::io::ErrorKind::NotFound => SaveFailure::Missing,
        std::io::ErrorKind::PermissionDenied => SaveFailure::PermissionDenied,
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::WouldBlock => {
            SaveFailure::Unavailable
        }
        _ => SaveFailure::Internal,
    }
}

fn classify_save_open(error: TextDocumentError) -> SaveFailure {
    match error {
        TextDocumentError::Missing => SaveFailure::Missing,
        TextDocumentError::Symlink => SaveFailure::Conflict,
        TextDocumentError::WrongType => SaveFailure::WrongType,
        TextDocumentError::PermissionDenied => SaveFailure::PermissionDenied,
        TextDocumentError::InvalidPath
        | TextDocumentError::InvalidUtf8
        | TextDocumentError::TooLarge
        | TextDocumentError::Io(_) => SaveFailure::Unavailable,
    }
}

#[cfg(unix)]
fn set_private_permissions(file: &cap_std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(Permissions::from_std(std::fs::Permissions::from_mode(
        0o600,
    )))
}

#[cfg(not(unix))]
fn set_private_permissions(_file: &cap_std::fs::File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(directory: &Dir) -> std::io::Result<()> {
    // cap-std may retain a Linux directory capability as an `O_PATH`
    // descriptor, which is intentionally unsuitable for `fsync`. Opening
    // `.` through that capability produces a real directory file handle
    // without reconstructing an ambient path.
    directory.open(".")?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_directory: &Dir) -> std::io::Result<()> {
    Ok(())
}

#[allow(dead_code)]
fn _metadata_contract(_: &Metadata, _: &Permissions) {}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn grant(root: &Path, document: &str) -> WorkspaceGrant {
        WorkspaceGrant::text_document(root, document)
    }

    #[test]
    fn validates_relative_paths() {
        for invalid in ["", ".", "../note.md", "/note.md", "folder/../note.md"] {
            assert!(
                validate_relative_path(Path::new(invalid)).is_err(),
                "{invalid}"
            );
        }
        assert!(validate_relative_path(Path::new("folder/note.md")).is_ok());
    }

    #[test]
    fn decodes_bom_and_enforces_visible_byte_limit() {
        assert_eq!(
            decode(b"\xef\xbb\xbfHello\r\n", 7).unwrap(),
            (TextEncoding::Utf8WithBom, "Hello\r\n".to_owned())
        );
        assert!(matches!(
            decode(b"1234", 3),
            Err(TextDocumentError::TooLarge)
        ));
        assert!(matches!(
            decode(&[0xff], 1),
            Err(TextDocumentError::InvalidUtf8)
        ));
    }

    #[test]
    fn saves_exact_bytes_and_preserves_bom_and_permissions() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("note.md");
        std::fs::write(&path, b"\xef\xbb\xbfHello\r\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        let session = TextDocumentSession::open(&grant(directory.path(), "note.md"), 1024).unwrap();
        let info = session.info();
        assert_eq!(
            session.editor_text(info.handle).unwrap().1.as_ref(),
            "Hello\r\n"
        );
        let job = session
            .prepare_save(
                info.handle,
                youth_tree::NodeId::new(1).unwrap(),
                4,
                "Changed\r\n".into(),
                1024,
            )
            .unwrap();
        session.mark_pending(job.request).unwrap();
        assert!(matches!(job.execute().outcome, SaveOutcome::Saved { .. }));
        assert_eq!(std::fs::read(&path).unwrap(), b"\xef\xbb\xbfChanged\r\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
    }

    #[test]
    fn exact_content_conflict_leaves_destination_untouched() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("note.txt");
        std::fs::write(&path, b"before").unwrap();
        let session =
            TextDocumentSession::open(&grant(directory.path(), "note.txt"), 1024).unwrap();
        let info = session.info();
        let job = session
            .prepare_save(
                info.handle,
                youth_tree::NodeId::new(1).unwrap(),
                1,
                "local".into(),
                1024,
            )
            .unwrap();
        session.mark_pending(job.request).unwrap();
        std::fs::write(&path, b"remote").unwrap();
        assert_eq!(
            job.execute().outcome,
            SaveOutcome::Failed(SaveFailure::Conflict)
        );
        assert_eq!(std::fs::read(path).unwrap(), b"remote");
    }

    #[cfg(unix)]
    #[test]
    fn replacing_a_parent_with_a_symlink_conflicts() {
        use std::os::unix::fs::symlink;

        let directory = tempdir().unwrap();
        let original = directory.path().join("folder");
        let moved = directory.path().join("moved");
        std::fs::create_dir(&original).unwrap();
        std::fs::write(original.join("note.txt"), b"before").unwrap();
        let session =
            TextDocumentSession::open(&grant(directory.path(), "folder/note.txt"), 1024).unwrap();
        let info = session.info();
        let job = session
            .prepare_save(
                info.handle,
                youth_tree::NodeId::new(1).unwrap(),
                1,
                "local".into(),
                1024,
            )
            .unwrap();
        session.mark_pending(job.request).unwrap();
        std::fs::rename(&original, &moved).unwrap();
        symlink("moved", &original).unwrap();
        assert_eq!(
            job.execute().outcome,
            SaveOutcome::Failed(SaveFailure::Conflict)
        );
        assert_eq!(std::fs::read(moved.join("note.txt")).unwrap(), b"before");
    }
}
