use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use thiserror::Error;

pub type DocumentPickerFuture =
    Pin<Box<dyn Future<Output = DocumentPickerResult> + Send + 'static>>;

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum DialogError {
    #[error("The document picker could not be opened.")]
    Unavailable,
    #[error("The selected document path could not be used.")]
    InvalidSelection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DocumentPickerResult {
    Picked(PathBuf),
    Cancelled,
    Failed(DialogError),
}

pub trait DocumentPicker {
    fn pick_text_document(&self) -> DocumentPickerResult;

    fn begin_pick_text_document(&self) -> DocumentPickerFuture {
        let result = self.pick_text_document();
        Box::pin(async move { result })
    }
}

#[derive(Clone, Debug)]
pub struct FixedDocumentPicker {
    result: DocumentPickerResult,
}

impl FixedDocumentPicker {
    pub fn new(result: DocumentPickerResult) -> Self {
        Self { result }
    }
}

impl DocumentPicker for FixedDocumentPicker {
    fn pick_text_document(&self) -> DocumentPickerResult {
        self.result.clone()
    }
}

#[derive(Clone, Debug)]
pub struct RecordingDocumentPicker {
    result: DocumentPickerResult,
    calls: Arc<AtomicUsize>,
}

impl RecordingDocumentPicker {
    pub fn new(result: DocumentPickerResult) -> Self {
        Self {
            result,
            calls: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    pub fn was_called(&self) -> bool {
        self.call_count() != 0
    }
}

impl DocumentPicker for RecordingDocumentPicker {
    fn pick_text_document(&self) -> DocumentPickerResult {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.result.clone()
    }
}

/// Uses rfd's XDG desktop portal backend on Linux, with rfd's documented
/// Zenity runtime fallback when a portal is unavailable. Its `.txt`/`.md`
/// filter is only a dialog convenience; document validation remains the
/// responsibility of the runtime when it opens the selected path.
#[derive(Clone, Copy, Debug, Default)]
pub struct RfdDocumentPicker;

impl RfdDocumentPicker {
    pub fn new() -> Self {
        Self
    }

    fn dialog() -> rfd::AsyncFileDialog {
        rfd::AsyncFileDialog::new().add_filter("Text documents", &["txt", "md"])
    }
}

impl DocumentPicker for RfdDocumentPicker {
    fn pick_text_document(&self) -> DocumentPickerResult {
        rfd::FileDialog::new()
            .add_filter("Text documents", &["txt", "md"])
            .pick_file()
            .map_or(
                DocumentPickerResult::Cancelled,
                DocumentPickerResult::Picked,
            )
    }

    fn begin_pick_text_document(&self) -> DocumentPickerFuture {
        // rfd recommends constructing macOS async dialogs on the main thread
        // in an already-windowed application, then awaiting them elsewhere.
        let future = Self::dialog().pick_file();
        Box::pin(async move {
            future
                .await
                .map_or(DocumentPickerResult::Cancelled, |file| {
                    DocumentPickerResult::Picked(file.path().to_path_buf())
                })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_picker_returns_its_configured_result() {
        let path = PathBuf::from("notes.md");
        let picker = FixedDocumentPicker::new(DocumentPickerResult::Picked(path.clone()));

        assert_eq!(
            picker.pick_text_document(),
            DocumentPickerResult::Picked(path)
        );
    }

    #[test]
    fn recording_picker_counts_calls_and_returns_its_configured_result() {
        let picker = RecordingDocumentPicker::new(DocumentPickerResult::Cancelled);

        assert!(!picker.was_called());
        assert_eq!(picker.pick_text_document(), DocumentPickerResult::Cancelled);
        assert_eq!(picker.pick_text_document(), DocumentPickerResult::Cancelled);
        assert_eq!(picker.call_count(), 2);
    }
}
