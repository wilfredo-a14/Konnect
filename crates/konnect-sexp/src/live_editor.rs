//! Writes to a schematic that is open in a live KiCad editor.
//!
//! KiCad locks only a schematic's root file, and its lock cannot say whether
//! the editor holds unsaved changes. A KiCad whose IPC API reports the
//! schematic's modified state (`GetDocumentModifiedState`) can prove that it
//! does not, which makes the file on disk the authoritative copy: a write is
//! then safe, provided the editor reloads the file afterwards
//! (`RevertDocument`).
//!
//! This crate has no IPC client, so the server installs a [`LiveEditorProbe`]
//! that asks KiCad. Writes made on a [`LiveEditorState::Unmodified`] verdict
//! are recorded until [`take_live_writes`] drains them, so the caller can ask
//! the editor to reload.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// What the open KiCad editor says about a design document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEditorState {
    /// No editor that the probe can reach has this document open, or the
    /// editor cannot report its modified state. The sibling lock decides.
    Unknown,
    /// An editor has the document open with no unsaved changes.
    Unmodified,
    /// An editor has the document open with unsaved changes.
    Modified,
}

pub type LiveEditorProbe = dyn Fn(&Path) -> LiveEditorState + Send + Sync;

static PROBE: OnceLock<Box<LiveEditorProbe>> = OnceLock::new();
static LIVE_WRITES: Mutex<Vec<LiveWrite>> = Mutex::new(Vec::new());

/// A document written on an [`LiveEditorState::Unmodified`] verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveWrite {
    pub path: PathBuf,
    /// The document as the editor had it, read before the first write;
    /// `None` for a document that did not exist yet.
    pub before: Option<String>,
}

/// Install the process-wide probe. Only the first installation takes effect.
pub fn install_probe(probe: Box<LiveEditorProbe>) -> bool {
    PROBE.set(probe).is_ok()
}

/// Ask the installed probe about `path`; [`LiveEditorState::Unknown`] without one.
pub(crate) fn probe(path: &Path) -> LiveEditorState {
    PROBE
        .get()
        .map_or(LiveEditorState::Unknown, |probe| probe(path))
}

/// Called before each write; keeps the content from before the first one.
pub(crate) fn record_live_write(path: &Path) {
    if let Ok(mut writes) = LIVE_WRITES.lock() {
        if !writes.iter().any(|written| written.path == path) {
            writes.push(LiveWrite {
                path: path.to_path_buf(),
                before: std::fs::read_to_string(path).ok(),
            });
        }
    }
}

/// Documents written on an [`LiveEditorState::Unmodified`] verdict since the
/// last call, so their editor can be asked to reload them.
pub fn take_live_writes() -> Vec<LiveWrite> {
    LIVE_WRITES
        .lock()
        .map(|mut writes| std::mem::take(&mut *writes))
        .unwrap_or_default()
}
