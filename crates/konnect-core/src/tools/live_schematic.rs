//! Editing a schematic that is open in KiCad.
//!
//! A KiCad that reports a schematic's modified state over IPC (the fredo fork,
//! or KiCad 11) lets Konnect write the schematic while the editor is open,
//! provided the editor holds no unsaved changes; the editor then reloads what
//! was written. The write-side rule lives in [`konnect_sexp::live_editor`];
//! this module supplies its probe and performs the reload.
//!
//! Without such a KiCad, or with no KiCad running, the probe answers
//! `Unknown` and writes fall back to the editor-lock rule.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use konnect_ipc::gen::kiapi::common::commands::DocumentModifiedState;
use konnect_ipc::gen::kiapi::common::types::DocumentSpecifier;
use konnect_ipc::{IpcEditorKind, KiCadIpcClient};
use konnect_sexp::live_editor::{self, LiveEditorState, LiveWrite};
use konnect_sexp::SexpNode;

/// A probe runs before every schematic write, so it must not stall a tool
/// behind a busy editor for long.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// Install the probe for the configured IPC address. An empty address is
/// resolved on each probe, so a KiCad started after Konnect is still found.
pub fn install_probe(configured_address: &str) {
    let configured = configured_address.trim().to_string();
    live_editor::install_probe(Box::new(move |path| probe(&configured, path)));
}

pub(crate) fn endpoint(configured: &str) -> Option<String> {
    if configured.is_empty() {
        konnect_ipc::detect_ipc_address()
    } else {
        Some(configured.to_string())
    }
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| {
        path.parent()
            .and_then(|parent| parent.canonicalize().ok())
            .and_then(|parent| path.file_name().map(|name| parent.join(name)))
            .unwrap_or_else(|| path.to_path_buf())
    })
}

/// The open schematic whose project directory contains `path`. KiCad 10.0
/// releases report no project for a schematic, so they never match.
fn open_schematic_for(client: &KiCadIpcClient, path: &Path) -> Option<DocumentSpecifier> {
    let target = canonical(path);
    client
        .get_open_documents_within(IpcEditorKind::Schematic, PROBE_TIMEOUT)
        .ok()?
        .into_iter()
        .find(|document| {
            document.project.as_ref().is_some_and(|project| {
                !project.path.is_empty() && target.starts_with(canonical(Path::new(&project.path)))
            })
        })
}

fn probe(configured: &str, path: &Path) -> LiveEditorState {
    let Some(address) = endpoint(configured) else {
        return LiveEditorState::Unknown;
    };
    let client = KiCadIpcClient::new(address);
    let Some(document) = open_schematic_for(&client, path) else {
        return LiveEditorState::Unknown;
    };
    match client.get_document_modified_state(&document, PROBE_TIMEOUT) {
        Ok(DocumentModifiedState::DmsUnmodified) => LiveEditorState::Unmodified,
        Ok(DocumentModifiedState::DmsModified) => LiveEditorState::Modified,
        _ => LiveEditorState::Unknown,
    }
}

/// Ask KiCad to reload every open schematic written since the last call, and
/// describe the outcome for the tool result. `None` when nothing was written.
pub async fn reload_written(configured_address: &str, label: &str) -> Option<String> {
    let written = live_editor::take_live_writes();
    if written.is_empty() {
        return None;
    }
    let configured = configured_address.trim().to_string();
    let label = label.to_string();
    let outcome = tokio::task::spawn_blocking(move || reload(&configured, &written, &label)).await;
    Some(outcome.unwrap_or_else(|error| {
        format!("KiCad live reload did not run ({error}); use File > Revert in KiCad.")
    }))
}

fn reload(configured: &str, written: &[LiveWrite], label: &str) -> String {
    let Some(address) = endpoint(configured) else {
        return "KiCad live reload failed: the KiCad IPC endpoint disappeared. Reopen the \
                schematic in KiCad before editing it there."
            .to_string();
    };
    let client = KiCadIpcClient::new(address);

    // Each open schematic with the writes that belong to it
    let mut documents: Vec<(DocumentSpecifier, Vec<&LiveWrite>)> = Vec::new();
    for write in written {
        if let Some(document) = open_schematic_for(&client, &write.path) {
            match documents.iter_mut().find(|(open, _)| *open == document) {
                Some((_, writes)) => writes.push(write),
                None => documents.push((document, vec![write])),
            }
        }
    }
    if documents.is_empty() {
        return "KiCad live reload skipped: the schematic is no longer open in KiCad.".to_string();
    }

    let mut notes = Vec::new();
    for (document, writes) in &documents {
        let name = &document_name(document);
        match client.revert_document(document) {
            Ok(()) => {
                notes.push(format!("KiCad reloaded {name} from disk."));
                notes.push(highlight_changes(&client, document, writes, label));
            }
            Err(error) => notes.push(format!(
                "KiCad did not reload {name}: {error:#}. The files on disk hold this change but \
                 the editor still shows the previous version; saving there would overwrite \
                 this change. Resolve the editor's unsaved changes, then reload with File > \
                 Revert."
            )),
        }
    }
    notes.join(" ")
}

/// Brighten the items the writes added or changed, so the person at the
/// editor can see where the edit landed.
fn highlight_changes(
    client: &KiCadIpcClient,
    document: &DocumentSpecifier,
    writes: &[&LiveWrite],
    label: &str,
) -> String {
    let mut changed = Vec::new();
    let mut moved = Vec::new();
    for write in writes {
        let Ok(after) = std::fs::read_to_string(&write.path) else {
            continue;
        };
        let ids = changed_item_ids(&write.path, write.before.as_deref(), &after);
        if let Some(before) = write.before.as_deref() {
            moved.extend(moved_items(before, &after, &ids));
        }
        changed.extend(ids);
    }
    match client.highlight_items(document, &changed, label, &moved) {
        Ok(0) if changed.is_empty() => {
            "No added or changed item to highlight (the edit removed items).".to_string()
        }
        Ok(count) => format!("KiCad highlighted {count} added or changed item(s)."),
        Err(error) => format!("KiCad did not highlight the change: {error:#}."),
    }
}

/// UUIDs of the top-level items in `after` that are new or differ from
/// `before`. Konnect writes a whole document in its own canonical layout, so
/// an item counts as changed only when it differs both from the editor's copy
/// and from that copy in Konnect's layout.
pub(crate) fn changed_item_ids(path: &Path, before: Option<&str>, after: &str) -> Vec<String> {
    let Ok(after) = konnect_sexp::parse_sexp(after) else {
        return Vec::new();
    };
    let before_layouts: Vec<HashMap<String, SexpNode>> = before
        .into_iter()
        .flat_map(|before| {
            let canonical =
                konnect_schematic_editor::schematic::Schematic::from_source(path, before)
                    .ok()
                    .map(|schematic| schematic.to_source());
            [Some(before.to_string()), canonical]
        })
        .flatten()
        .filter_map(|source| konnect_sexp::parse_sexp(&source).ok())
        .map(|root| {
            top_level_items(&root)
                .into_iter()
                .map(|(id, node)| (id, node.clone()))
                .collect()
        })
        .collect();

    top_level_items(&after)
        .into_iter()
        .filter(|(id, node)| {
            !before_layouts
                .iter()
                .any(|items| items.get(id) == Some(*node))
        })
        .map(|(id, _)| id)
        .collect()
}

/// Schematic API positions are in schematic internal units: 10,000 per mm.
const SCHEMATIC_UNITS_PER_MM: f64 = 10_000.0;

/// Of the `changed` items, those whose `(at x y ...)` differs between
/// `before` and `after`, with their position in `before` in schematic API
/// units. KiCad shows these being dragged.
pub(crate) fn moved_items(
    before: &str,
    after: &str,
    changed: &[String],
) -> Vec<(String, (i64, i64))> {
    let (Ok(before), Ok(after)) = (
        konnect_sexp::parse_sexp(before),
        konnect_sexp::parse_sexp(after),
    ) else {
        return Vec::new();
    };
    let before: HashMap<String, &SexpNode> = top_level_items(&before).into_iter().collect();
    let after: HashMap<String, &SexpNode> = top_level_items(&after).into_iter().collect();

    changed
        .iter()
        .filter_map(|id| {
            let was = position(before.get(id)?)?;
            let now = position(after.get(id)?)?;
            (was != now).then(|| {
                let units = |mm: f64| (mm * SCHEMATIC_UNITS_PER_MM).round() as i64;
                (id.clone(), (units(was.0), units(was.1)))
            })
        })
        .collect()
}

/// An item's own `(at x y ...)`, in mm.
fn position(item: &SexpNode) -> Option<(f64, f64)> {
    let SexpNode::List(fields) = item else {
        return None;
    };
    fields.iter().find_map(|field| match field {
        SexpNode::List(at) if at.first().and_then(SexpNode::as_str) == Some("at") => {
            let number = |i: usize| at.get(i)?.as_str()?.parse::<f64>().ok();
            Some((number(1)?, number(2)?))
        }
        _ => None,
    })
}

/// Top-level items of a schematic that carry their own `(uuid ...)`.
fn top_level_items(root: &SexpNode) -> Vec<(String, &SexpNode)> {
    let SexpNode::List(children) = root else {
        return Vec::new();
    };
    children
        .iter()
        .filter_map(|child| {
            let SexpNode::List(fields) = child else {
                return None;
            };
            let id = fields.iter().find_map(|field| match field {
                SexpNode::List(uuid) if uuid.first().and_then(SexpNode::as_str) == Some("uuid") => {
                    uuid.get(1).and_then(SexpNode::as_str).map(str::to_string)
                }
                _ => None,
            })?;
            Some((id, child))
        })
        .collect()
}

fn document_name(document: &DocumentSpecifier) -> String {
    use konnect_ipc::gen::kiapi::common::types::document_specifier::Identifier;
    match &document.identifier {
        Some(Identifier::BoardFilename(name)) if !name.is_empty() => format!("'{name}'"),
        _ => "the schematic".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moved_items_report_where_a_changed_item_was() {
        let before = r#"(kicad_sch
  (symbol (lib_id "Device:R") (at 100 50 0) (uuid "aaa"))
  (label "VBAT" (at 10.16 20.32 0) (uuid "bbb"))
  (wire (pts (xy 0 0) (xy 1 1)) (uuid "ccc"))
)"#;
        let after = r#"(kicad_sch
  (symbol (lib_id "Device:R") (at 120.5 50 90) (uuid "aaa"))
  (label "VBAT2" (at 10.16 20.32 0) (uuid "bbb"))
  (wire (pts (xy 0 0) (xy 2 2)) (uuid "ccc"))
)"#;
        let changed = vec!["aaa".to_string(), "bbb".to_string(), "ccc".to_string()];
        assert_eq!(
            moved_items(before, after, &changed),
            vec![("aaa".to_string(), (1_000_000, 500_000))]
        );
    }

    /// Run against a real edit: `LIVE_BEFORE` is the editor's copy, `LIVE_AFTER`
    /// Konnect's rewrite, `LIVE_EXPECT` the comma-separated UUIDs it changed.
    #[test]
    #[ignore = "needs LIVE_BEFORE, LIVE_AFTER and LIVE_EXPECT"]
    fn real_edit_reports_only_the_items_it_changed() {
        let before = std::fs::read_to_string(std::env::var("LIVE_BEFORE").unwrap()).unwrap();
        let after_path = std::env::var("LIVE_AFTER").unwrap();
        let after = std::fs::read_to_string(&after_path).unwrap();
        let mut expected: Vec<String> = std::env::var("LIVE_EXPECT")
            .unwrap()
            .split(',')
            .map(str::to_string)
            .collect();
        let mut changed = changed_item_ids(Path::new(&after_path), Some(&before), &after);
        expected.sort();
        changed.sort();
        assert_eq!(changed, expected);
    }

    const SHEET: &str = r#"(kicad_sch
	(version 20250114)
	(generator "eeschema")
	(uuid "11111111-1111-1111-1111-111111111111")
	(paper "A4")
	(label "KEEP"
		(at 10.16 10.16 0)
		(effects (font (size 1.27 1.27)) (justify left bottom))
		(uuid "22222222-2222-2222-2222-222222222222")
	)
	(label "EDIT"
		(at 20.32 10.16 0)
		(effects (font (size 1.27 1.27)) (justify left bottom))
		(uuid "33333333-3333-3333-3333-333333333333")
	)
)
"#;

    #[test]
    fn a_new_document_reports_every_item() {
        let changed = changed_item_ids(Path::new("/p/new.kicad_sch"), None, SHEET);
        assert_eq!(changed.len(), 2);
    }

    #[test]
    fn only_added_and_edited_items_are_reported() {
        let after = SHEET
            .replace("(at 20.32 10.16 0)", "(at 25.4 10.16 0)")
            .replace(
                "\n)\n",
                "\n\t(label \"NEW\"\n\t\t(at 30.48 10.16 0)\n\t\t(effects (font (size 1.27 1.27)) (justify left bottom))\n\t\t(uuid \"44444444-4444-4444-4444-444444444444\")\n\t)\n)\n",
            );
        let mut changed = changed_item_ids(Path::new("/p/sheet.kicad_sch"), Some(SHEET), &after);
        changed.sort();
        assert_eq!(
            changed,
            [
                "33333333-3333-3333-3333-333333333333",
                "44444444-4444-4444-4444-444444444444"
            ]
        );
    }

    #[test]
    fn a_reformatted_unchanged_document_reports_nothing() {
        let reformatted = SHEET.replace("\t", "  ");
        assert!(
            changed_item_ids(Path::new("/p/sheet.kicad_sch"), Some(SHEET), &reformatted).is_empty()
        );
    }
}
