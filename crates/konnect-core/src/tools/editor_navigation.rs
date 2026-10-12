//! Semantic KiCad editor observation and navigation.
//!
//! Public names in this module are provisional while upstream design issue
//! #395 is under review. The implementation deliberately exposes only typed
//! semantic operations; KiCad's unstable raw `RunAction` strings are not part
//! of this surface.

use crate::mcp::{error::ToolErrorKind, protocol::CallToolResult};
use crate::tool;
use crate::tools::{invalid_arg, opt_str, require_array, require_str, ToolContext, ToolDef};
use konnect_ipc::{
    IpcEditorDocument, IpcEditorKind, IpcProjectIdentity, IpcSelectionMutation,
    IpcSelectionMutationErrorKind, IpcSelectionObservationErrorKind, IpcSheetInstancePath,
};
use serde_json::json;
use std::path::PathBuf;

use super::cross_probe::{resolve_cross_probe, CrossProbeError, CrossProbeRequest};
use super::navigation_target::{
    resolve_navigation_target, NavigationTargetErrorKind, NavigationTargetRequest,
};

pub fn tools() -> Vec<ToolDef> {
    vec![
        tool!(
            "open_editor",
            "Open a KiCad editor window, or bring it to the front, as the project manager's launcher buttons do: 'schematic' and 'pcb' load the open project's schematic or board, 'symbol' and 'footprint' open the library editors, '3d' opens the 3D viewer from the PCB editor (which must be open), 'project' raises the project manager. Works whichever KiCad window is in front. Needs the fredo KiCad fork; refuses instead of prompting when no project is open or the project has no such file.",
            json!({
                "type": "object",
                "properties": {
                    "editor": { "type": "string", "enum": ["project", "schematic", "pcb", "symbol", "footprint", "3d"] }
                },
                "required": ["editor"]
            }),
            |args, ctx| async move { handle_open_editor(args, ctx).await }
        ),
        tool!(
            "run_editor_action",
            "Run a KiCad editor action by name in the PCB or schematic editor, as clicking its toolbar button or menu entry does, e.g. 'common.Control.zoomFitScreen', 'pcbnew.EditorControl.boardSetup', 'eeschema.EditorControl.schematicSetup' or 'common.SuiteControl.openPreferences'. Set deferred for actions that open a modal dialog (setup and preferences dialogs), so KiCad replies before the dialog blocks; close it afterwards with close_dialog. The agent cursor points at the button. Needs the fredo KiCad fork for deferred actions and schematic actions.",
            json!({
                "type": "object",
                "properties": {
                    "editor": { "type": "string", "enum": ["pcb", "schematic", "footprint", "3d"] },
                    "action": { "type": "string", "description": "The action name, e.g. common.Control.zoomFitScreen, or 3DViewer.Control.viewTop in the 3D viewer" },
                    "deferred": { "type": "boolean", "description": "Run after KiCad replies; required for actions that open a modal dialog", "default": false }
                },
                "required": ["editor", "action"]
            }),
            |args, ctx| async move { handle_run_editor_action(args, ctx).await }
        ),
        tool!(
            "run_checker",
            "Run the design check in the open editor as a person would: DRC in the PCB editor or ERC in the schematic editor. Opens the checker dialog, clicks Run, and leaves the results on screen; the API Activity log gets the error and warning counts when the run finishes. Returns once the run has started. Needs the fredo KiCad fork and the editor open.",
            json!({
                "type": "object",
                "properties": {
                    "editor": { "type": "string", "enum": ["pcb", "schematic"] }
                },
                "required": ["editor"]
            }),
            |args, ctx| async move { handle_run_checker(args, ctx).await }
        ),
        tool!(
            "close_dialog",
            "Click OK or Cancel on the frontmost KiCad dialog, as a person would, with the agent cursor on the button. Use it to dismiss a dialog opened with run_editor_action or left by run_checker. Reports whether a dialog was found and its title. Needs the fredo KiCad fork.",
            json!({
                "type": "object",
                "properties": {
                    "button": { "type": "string", "enum": ["ok", "cancel"] }
                },
                "required": ["button"]
            }),
            |args, ctx| async move { handle_close_dialog(args, ctx).await }
        ),
        tool!(
            "capture_3d_view",
            "Save what KiCad's 3D viewer currently shows as a PNG at an absolute path, so the rendered board can be looked at without a screenshot. Open the viewer first with open_editor('3d') and set the view with run_editor_action('3d', '3DViewer.Control.viewTop' etc.). Needs the fredo KiCad fork and the 3D viewer open on screen.",
            json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Absolute .png path in an existing folder" }
                },
                "required": ["path"]
            }),
            |args, ctx| async move { handle_capture_3d_view(args, ctx).await }
        ),
        tool!(
            "open_footprint_editor",
            "Load a footprint from a library into KiCad's footprint editor, as its library tree does, for example library 'Resistor_SMD' and name 'R_0603_1608Metric'. The editor's items (pads, graphics, fields) are then reachable through the board item API, and save_footprint writes it back. Refused while the editor holds unsaved changes. Needs the fredo KiCad fork; opens the editor if it is closed.",
            json!({
                "type": "object",
                "properties": {
                    "library": { "type": "string", "description": "Library nickname, e.g. Resistor_SMD" },
                    "name": { "type": "string", "description": "Footprint name, e.g. R_0603_1608Metric" }
                },
                "required": ["library", "name"]
            }),
            |args, ctx| async move { handle_open_footprint_editor(args, ctx).await }
        ),
        tool!(
            "save_footprint",
            "Save the footprint the footprint editor holds back to its library, as its Save button does. Needs the fredo KiCad fork and the footprint editor open.",
            json!({ "type": "object", "properties": {}, "required": [] }),
            |args, ctx| async move { handle_save_footprint(args, ctx).await }
        ),
        tool!(
            "edit_item_properties",
            "Edit a part through its Properties dialog as a person would: select the footprint (PCB) or symbol (schematic), open its Properties dialog, type each field's new value into the field grid with the agent cursor on the cell, and click OK. Field names are as the dialog lists them, e.g. Datasheet, MPN, Value. A field the dialog does not list cancels the dialog. The schematic form needs the sheet file that holds the symbol. Returns once the dialog is opening; each step is in the API Activity log. Needs the fredo KiCad fork and the editor open.",
            json!({
                "type": "object",
                "properties": {
                    "editor": { "type": "string", "enum": ["pcb", "schematic"] },
                    "reference": { "type": "string", "description": "Reference designator, e.g. R101" },
                    "schematic": { "type": "string", "description": "Path of the .kicad_sch sheet holding the symbol (schematic only)" },
                    "fields": { "type": "object", "additionalProperties": { "type": "string" }, "description": "Field name to new value" }
                },
                "required": ["editor", "reference", "fields"]
            }),
            |args, ctx| async move { handle_edit_item_properties(args, ctx).await }
        ),
        tool!(
            "get_editor_state",
            "Observe the configured KiCad IPC endpoint: running KiCad version, addressable schematic and PCB editors, exact open document identities, and semantic navigation capabilities. Active editor/document/sheet fields are null when KiCad has no stable typed query; open-document order is never treated as active state.",
            json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            |args, ctx| async move { handle_get_editor_state(args, ctx).await }
        ),
        tool!(
            "get_editor_selection",
            "Read the current selection from one exact KiCad editor, project, document, and schematic sheet instance. Returns stable KIID/UUID identities and refuses stale, ambiguous, cross-project, cross-document, or unsupported selected state instead of retargeting.",
            json!({
                "type": "object",
                "properties": {
                    "editor": { "type": "string", "enum": ["schematic", "pcb"] },
                    "project_name": { "type": "string", "description": "Exact project name returned by get_editor_state" },
                    "project_path": { "type": "string", "description": "Exact project path returned by get_editor_state" },
                    "document_path": { "type": "string", "description": "Exact PCB path; required only for editor=pcb" },
                    "sheet_instance_path": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Canonical root-to-leaf sheet KIIIDs; required only for editor=schematic"
                    },
                    "sheet_path_human_readable": { "type": "string", "description": "Optional display path returned by get_editor_state; not used as identity" }
                },
                "required": ["editor", "project_name", "project_path"]
            }),
            |args, ctx| async move { handle_get_editor_selection(args, ctx).await }
        ),
        tool!(
            "resolve_navigation_target",
            "Deterministically resolve one exact saved KiCad object in an explicitly open project, document, editor, and schematic hierarchy instance. Stable KIID/UUID identity is primary; a human reference is accepted only when it resolves uniquely and ambiguity is returned with candidates.",
            json!({
                "type": "object",
                "properties": {
                    "editor": { "type": "string", "enum": ["schematic", "pcb"] },
                    "project_name": { "type": "string" },
                    "project_path": { "type": "string" },
                    "document_path": { "type": "string", "description": "Exact saved .kicad_sch or .kicad_pcb document" },
                    "sheet_instance_path": { "type": "array", "items": { "type": "string" } },
                    "sheet_path_human_readable": { "type": "string" },
                    "object_kiid": { "type": "string", "description": "Preferred stable KIID/UUID" },
                    "human_reference": { "type": "string", "description": "Fallback reference such as C10; must resolve uniquely" }
                },
                "required": ["editor", "project_name", "project_path", "document_path"]
            }),
            |args, ctx| async move { handle_resolve_navigation_target(args, ctx).await }
        ),
        tool!(
            "mutate_editor_selection",
            "Clear, add to, or remove from one exact KiCad editor selection. Every non-clear KIID is first resolved in the explicit saved project/document/sheet, and success is derived only from a fresh exact GetSelection readback.",
            json!({
                "type": "object",
                "properties": {
                    "operation": { "type": "string", "enum": ["clear", "add", "remove"] },
                    "editor": { "type": "string", "enum": ["schematic", "pcb"] },
                    "project_name": { "type": "string" },
                    "project_path": { "type": "string" },
                    "document_path": { "type": "string", "description": "Exact saved .kicad_sch or .kicad_pcb document" },
                    "sheet_instance_path": { "type": "array", "items": { "type": "string" } },
                    "sheet_path_human_readable": { "type": "string" },
                    "object_kiids": { "type": "array", "items": { "type": "string" }, "description": "Empty for clear; one or more stable KIIIDs for add/remove" }
                },
                "required": ["operation", "editor", "project_name", "project_path", "document_path", "object_kiids"]
            }),
            |args, ctx| async move { handle_mutate_editor_selection(args, ctx).await }
        ),
        tool!(
            "resolve_cross_probe_target",
            "Resolve an exact schematic-symbol/PCB-footprint relationship in either direction from stable saved KiCad symbol-path linkage, while proving both explicit live document contexts. This resolve-only operation does not activate or select; pin/pad/net expansion remains unsupported unless a unique stable destination can be modeled.",
            json!({
                "type": "object",
                "properties": {
                    "source_editor": { "type": "string", "enum": ["schematic", "pcb"] },
                    "project_name": { "type": "string" },
                    "project_path": { "type": "string" },
                    "schematic_document_path": { "type": "string" },
                    "pcb_document_path": { "type": "string" },
                    "schematic_sheet_instance_path": { "type": "array", "items": { "type": "string" } },
                    "sheet_path_human_readable": { "type": "string" },
                    "source_object_kiid": { "type": "string" }
                },
                "required": ["source_editor", "project_name", "project_path", "schematic_document_path", "pcb_document_path", "schematic_sheet_instance_path", "source_object_kiid"]
            }),
            |args, ctx| async move { handle_resolve_cross_probe_target(args, ctx).await }
        ),
    ]
}

async fn handle_open_editor(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    use konnect_ipc::gen::kiapi::common::types::FrameType;

    let editor = match require_str(args, "editor") {
        Ok(editor) => editor.to_string(),
        Err(error) => return Ok(error),
    };
    let frame = match editor.as_str() {
        "project" => FrameType::FtProjectManager,
        "schematic" => FrameType::FtSchematicEditor,
        "pcb" => FrameType::FtPcbEditor,
        "symbol" => FrameType::FtSymbolEditor,
        "footprint" => FrameType::FtFootprintEditor,
        "3d" => FrameType::Ft3dViewer,
        _ => {
            return Ok(invalid_arg(
                "editor",
                "expected 'project', 'schematic', 'pcb', 'symbol', 'footprint' or '3d'",
            ))
        }
    };
    let Some(address) = super::live_schematic::endpoint(&ctx.config.ipc_address) else {
        return Ok(editor_unavailable(
            "no KiCad IPC endpoint found; start KiCad with its API server enabled",
        ));
    };

    let result = tokio::task::spawn_blocking(move || {
        konnect_ipc::KiCadIpcClient::new(address).open_editor(frame)
    })
    .await?;
    match result {
        Ok(()) => Ok(CallToolResult::json(&json!({ "opened": editor }))),
        Err(error) => Ok(editor_unavailable(&format!(
            "KiCad did not open the {editor} editor: {error:#}"
        ))),
    }
}

fn frame_for(editor: &str) -> Option<konnect_ipc::gen::kiapi::common::types::FrameType> {
    use konnect_ipc::gen::kiapi::common::types::FrameType;
    match editor {
        "pcb" => Some(FrameType::FtPcbEditor),
        "schematic" => Some(FrameType::FtSchematicEditor),
        "footprint" => Some(FrameType::FtFootprintEditor),
        "3d" => Some(FrameType::Ft3dViewer),
        _ => None,
    }
}

fn kind_for(editor: &str) -> Option<konnect_ipc::IpcEditorKind> {
    match editor {
        "pcb" => Some(konnect_ipc::IpcEditorKind::Pcb),
        "schematic" => Some(konnect_ipc::IpcEditorKind::Schematic),
        _ => None,
    }
}

/// The first document the editor of `kind` holds open, or the refusal to return.
fn open_document(
    client: &konnect_ipc::KiCadIpcClient,
    kind: konnect_ipc::IpcEditorKind,
) -> Result<konnect_ipc::gen::kiapi::common::types::DocumentSpecifier, CallToolResult> {
    match client.get_open_documents_for(kind) {
        Ok(documents) => documents.into_iter().next().ok_or_else(|| {
            editor_unavailable(&format!(
                "the {} editor holds no open document; open it first",
                kind.as_str()
            ))
        }),
        Err(error) => Err(editor_unavailable(&format!(
            "KiCad did not list the {} editor's documents: {error:#}",
            kind.as_str()
        ))),
    }
}

async fn handle_run_editor_action(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let editor = match require_str(args, "editor") {
        Ok(editor) => editor.to_string(),
        Err(error) => return Ok(error),
    };
    let Some(frame) = frame_for(&editor) else {
        return Ok(invalid_arg(
            "editor",
            "expected 'pcb', 'schematic', 'footprint' or '3d'",
        ));
    };
    let action = match require_str(args, "action") {
        Ok(action) => action.to_string(),
        Err(error) => return Ok(error),
    };
    let deferred = args
        .get("deferred")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let Some(address) = super::live_schematic::endpoint(&ctx.config.ipc_address) else {
        return Ok(editor_unavailable(
            "no KiCad IPC endpoint found; start KiCad with its API server enabled",
        ));
    };

    let action_ipc = action.clone();
    let result = tokio::task::spawn_blocking(move || {
        konnect_ipc::KiCadIpcClient::new(address).run_action_in(frame, &action_ipc, deferred)
    })
    .await?;
    match result {
        Ok(true) => Ok(CallToolResult::json(&json!({
            "ran": action,
            "editor": editor,
            "deferred": deferred,
            "note": if deferred { "KiCad runs the action right after this reply; a dialog it opens stays open until close_dialog or the person closes it" } else { "the action ran" }
        }))),
        Ok(false) => Ok(invalid_arg(
            "action",
            &format!("the {editor} editor has no action named '{action}'"),
        )),
        Err(error) => Ok(editor_unavailable(&format!(
            "KiCad did not run '{action}' in the {editor} editor: {error:#}"
        ))),
    }
}

async fn handle_run_checker(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let editor = match require_str(args, "editor") {
        Ok(editor) => editor.to_string(),
        Err(error) => return Ok(error),
    };
    let Some(kind) = kind_for(&editor) else {
        return Ok(invalid_arg("editor", "expected 'pcb' or 'schematic'"));
    };
    let Some(address) = super::live_schematic::endpoint(&ctx.config.ipc_address) else {
        return Ok(editor_unavailable(
            "no KiCad IPC endpoint found; start KiCad with its API server enabled",
        ));
    };
    let checker = if kind == konnect_ipc::IpcEditorKind::Pcb {
        "DRC"
    } else {
        "ERC"
    };

    let result = tokio::task::spawn_blocking(move || {
        let client = konnect_ipc::KiCadIpcClient::new(address);
        let document = open_document(&client, kind)?;
        client.run_checker(document).map_err(|error| {
            editor_unavailable(&format!("KiCad did not start the {checker}: {error:#}"))
        })
    })
    .await?;
    match result {
        Ok(started) => Ok(CallToolResult::json(&json!({
            "checker": checker,
            "started": started,
            "note": "the results stay in the checker dialog; the API Activity log reports the counts when the run finishes"
        }))),
        Err(refusal) => Ok(refusal),
    }
}

async fn handle_close_dialog(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let button = match require_str(args, "button") {
        Ok(button) if matches!(button, "ok" | "cancel") => button.to_string(),
        Ok(_) => return Ok(invalid_arg("button", "expected 'ok' or 'cancel'")),
        Err(error) => return Ok(error),
    };
    let Some(address) = super::live_schematic::endpoint(&ctx.config.ipc_address) else {
        return Ok(editor_unavailable(
            "no KiCad IPC endpoint found; start KiCad with its API server enabled",
        ));
    };

    let button_ipc = button.clone();
    let result = tokio::task::spawn_blocking(move || {
        konnect_ipc::KiCadIpcClient::new(address).close_dialog(&button_ipc)
    })
    .await?;
    match result {
        Ok((true, title)) => Ok(CallToolResult::json(&json!({
            "closed_with": button,
            "dialog": title
        }))),
        Ok((false, _)) => Ok(CallToolResult::json(&json!({
            "closed_with": serde_json::Value::Null,
            "dialog": serde_json::Value::Null,
            "note": "no dialog is open"
        }))),
        Err(error) => Ok(editor_unavailable(&format!(
            "KiCad did not close the dialog: {error:#}"
        ))),
    }
}

async fn handle_capture_3d_view(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let path = match require_str(args, "path") {
        Ok(path) => path.to_string(),
        Err(error) => return Ok(error),
    };
    if !std::path::Path::new(&path).is_absolute() {
        return Ok(invalid_arg("path", "must be an absolute path"));
    }
    let Some(address) = super::live_schematic::endpoint(&ctx.config.ipc_address) else {
        return Ok(editor_unavailable(
            "no KiCad IPC endpoint found; start KiCad with its API server enabled",
        ));
    };
    let path_ipc = path.clone();
    let result = tokio::task::spawn_blocking(move || {
        konnect_ipc::KiCadIpcClient::new(address).capture_3d_view(&path_ipc)
    })
    .await?;
    match result {
        Ok((width, height)) => Ok(CallToolResult::json(&json!({
            "path": path,
            "width": width,
            "height": height
        }))),
        Err(error) => Ok(editor_unavailable(&format!(
            "KiCad did not save the 3D view: {error:#}. Open the 3D viewer first (open_editor '3d')."
        ))),
    }
}

async fn handle_open_footprint_editor(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    use konnect_ipc::gen::kiapi::common::types::FrameType;

    let library = match require_str(args, "library") {
        Ok(library) => library.to_string(),
        Err(error) => return Ok(error),
    };
    let name = match require_str(args, "name") {
        Ok(name) => name.to_string(),
        Err(error) => return Ok(error),
    };
    let Some(address) = super::live_schematic::endpoint(&ctx.config.ipc_address) else {
        return Ok(editor_unavailable(
            "no KiCad IPC endpoint found; start KiCad with its API server enabled",
        ));
    };
    let (library_ipc, name_ipc) = (library.clone(), name.clone());
    let result = tokio::task::spawn_blocking(move || {
        let client = konnect_ipc::KiCadIpcClient::new(address);
        // The editor must exist before it can load anything
        client.open_editor(FrameType::FtFootprintEditor)?;
        client.open_footprint(&library_ipc, &name_ipc)
    })
    .await?;
    match result {
        Ok(()) => Ok(CallToolResult::json(&json!({
            "opened": format!("{library}:{name}"),
            "editor": "footprint"
        }))),
        Err(error) => Ok(editor_unavailable(&format!(
            "KiCad did not open {library}:{name} in the footprint editor: {error:#}"
        ))),
    }
}

async fn handle_save_footprint(
    _args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let Some(address) = super::live_schematic::endpoint(&ctx.config.ipc_address) else {
        return Ok(editor_unavailable(
            "no KiCad IPC endpoint found; start KiCad with its API server enabled",
        ));
    };
    let result = tokio::task::spawn_blocking(move || {
        konnect_ipc::KiCadIpcClient::new(address).save_footprint()
    })
    .await?;
    match result {
        Ok(()) => Ok(CallToolResult::json(&json!({ "saved": true }))),
        Err(error) => Ok(editor_unavailable(&format!(
            "KiCad did not save the footprint: {error:#}"
        ))),
    }
}

async fn handle_edit_item_properties(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let editor = match require_str(args, "editor") {
        Ok(editor) => editor.to_string(),
        Err(error) => return Ok(error),
    };
    let Some(kind) = kind_for(&editor) else {
        return Ok(invalid_arg("editor", "expected 'pcb' or 'schematic'"));
    };
    let reference = match require_str(args, "reference") {
        Ok(reference) => reference.to_string(),
        Err(error) => return Ok(error),
    };
    let fields: Vec<(String, String)> =
        match args.get("fields").and_then(serde_json::Value::as_object) {
            Some(map) if !map.is_empty() => map
                .iter()
                .filter_map(|(name, value)| value.as_str().map(|v| (name.clone(), v.to_string())))
                .collect(),
            _ => {
                return Ok(invalid_arg(
                    "fields",
                    "an object of field name to new value, with at least one field",
                ))
            }
        };
    if fields.is_empty() {
        return Ok(invalid_arg("fields", "every value must be a string"));
    }

    // A symbol's KIID comes from the saved sheet; KiCad resolves it on the sheet it belongs to
    let symbol_kiid = if kind == konnect_ipc::IpcEditorKind::Schematic {
        let sch_path = match args.get("schematic").and_then(serde_json::Value::as_str) {
            Some(path) => std::path::PathBuf::from(path),
            None => {
                return Ok(invalid_arg(
                    "schematic",
                    "the .kicad_sch sheet holding the symbol is needed",
                ))
            }
        };
        let sheet = match konnect_schematic_editor::schematic::Schematic::load(&sch_path) {
            Ok(sheet) => sheet,
            Err(error) => {
                return Ok(CallToolResult::error(format!(
                    "could not read '{}': {error:#}",
                    sch_path.display()
                )))
            }
        };
        match sheet
            .symbols
            .iter()
            .filter(|symbol| symbol.reference() == Some(reference.as_str()))
            .min_by_key(|symbol| symbol.unit)
        {
            Some(symbol) => Some(symbol.uuid.clone()),
            None => {
                return Ok(CallToolResult::error(format!(
                    "'{reference}' is not on '{}'",
                    sch_path.display()
                )))
            }
        }
    } else {
        None
    };

    let Some(address) = super::live_schematic::endpoint(&ctx.config.ipc_address) else {
        return Ok(editor_unavailable(
            "no KiCad IPC endpoint found; start KiCad with its API server enabled",
        ));
    };

    let reference_ipc = reference.clone();
    let fields_ipc = fields.clone();
    let result = tokio::task::spawn_blocking(move || {
        let client = konnect_ipc::KiCadIpcClient::new(address);
        let document = open_document(&client, kind)?;
        let kiid = match symbol_kiid {
            Some(kiid) => kiid,
            None => match client.footprint_kiid(&reference_ipc) {
                Ok(Some(kiid)) => kiid,
                Ok(None) => {
                    return Err(CallToolResult::error(format!(
                        "'{reference_ipc}' is not on the open board"
                    )))
                }
                Err(error) => {
                    return Err(editor_unavailable(&format!(
                        "KiCad did not list the board's footprints: {error:#}"
                    )))
                }
            },
        };
        client
            .edit_item_properties(document, &kiid, &fields_ipc)
            .map_err(|error| {
                editor_unavailable(&format!(
                    "KiCad did not open the Properties dialog of {reference_ipc}: {error:#}"
                ))
            })
    })
    .await?;
    match result {
        Ok(started) => Ok(CallToolResult::json(&json!({
            "reference": reference,
            "editor": editor,
            "fields": fields.iter().map(|(name, value)| json!({ "name": name, "value": value })).collect::<Vec<_>>(),
            "started": started,
            "note": "the dialog is being worked on screen; the API Activity log shows each field typed and the OK click"
        }))),
        Err(refusal) => Ok(refusal),
    }
}

async fn handle_get_editor_state(
    _args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let address = ctx.config.ipc_address.clone();
    if address.is_empty() {
        return Ok(editor_unavailable("no KiCad IPC endpoint is configured"));
    }

    let result = tokio::task::spawn_blocking(move || {
        konnect_ipc::KiCadIpcClient::new(address).observe_editor_state()
    })
    .await?;
    match result {
        Ok(observation) => Ok(CallToolResult::json(&observation)),
        Err(error) => {
            if let Some(document_error) = error
                .chain()
                .find_map(|cause| cause.downcast_ref::<konnect_ipc::IpcDocumentObservationError>())
            {
                return Ok(CallToolResult::error_kind(
                    ToolErrorKind::StaleTarget {
                        target: format!("{} editor document state", document_error.editor.as_str()),
                        reason: document_error.reason.clone(),
                    },
                    document_error.to_string(),
                ));
            }
            if let Some(status) = konnect_ipc::ApiStatusError::from_error(&error) {
                if status.is_unsupported() {
                    return Ok(CallToolResult::error_kind(
                        ToolErrorKind::UnsupportedCapability {
                            capability: "editor_state_observation".to_string(),
                            kicad_version: None,
                        },
                        "The running KiCad endpoint does not support editor-state observation.",
                    ));
                }
                return Ok(CallToolResult::error_kind(
                    ToolErrorKind::StaleTarget {
                        target: "configured KiCad IPC endpoint".to_string(),
                        reason: status.code_name.clone(),
                    },
                    "The configured KiCad endpoint responded but could not provide a stable editor-state observation.",
                ));
            }
            match konnect_ipc::IpcFailure::from_error(error) {
                konnect_ipc::IpcFailure::Unreachable(_) => Ok(editor_unavailable(
                    "the configured KiCad IPC endpoint is unreachable",
                )),
                _ => Ok(CallToolResult::error_kind(
                    ToolErrorKind::StaleTarget {
                        target: "configured KiCad IPC endpoint".to_string(),
                        reason: "the endpoint did not return a complete typed observation"
                            .to_string(),
                    },
                    "The configured KiCad endpoint did not return a complete typed editor-state observation.",
                )),
            }
        }
    }
}

fn editor_unavailable(reason: &str) -> CallToolResult {
    CallToolResult::error_kind(
        ToolErrorKind::EditorUnavailable {
            editor: "configured_endpoint".to_string(),
            reason: reason.to_string(),
        },
        format!("KiCad editor state is unavailable: {reason}."),
    )
}

async fn handle_get_editor_selection(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let target = match parse_selection_target(args) {
        Ok(target) => target,
        Err(result) => return Ok(result),
    };
    let address = ctx.config.ipc_address.clone();
    if address.is_empty() {
        return Ok(editor_unavailable("no KiCad IPC endpoint is configured"));
    }
    let result = tokio::task::spawn_blocking(move || {
        konnect_ipc::KiCadIpcClient::new(address).observe_selection(&target)
    })
    .await?;
    match result {
        Ok(observation) => Ok(CallToolResult::json(&observation)),
        Err(error) => Ok(selection_error_result(error)),
    }
}

fn parse_selection_target(args: &serde_json::Value) -> Result<IpcEditorDocument, CallToolResult> {
    let editor = match require_str(args, "editor")? {
        "schematic" => IpcEditorKind::Schematic,
        "pcb" => IpcEditorKind::Pcb,
        _ => return Err(invalid_arg("editor", "expected 'schematic' or 'pcb'")),
    };
    let project_name = require_str(args, "project_name")?;
    let project_path = require_str(args, "project_path")?;
    if project_name.is_empty() {
        return Err(invalid_arg("project_name", "must not be empty"));
    }
    if project_path.is_empty() {
        return Err(invalid_arg("project_path", "must not be empty"));
    }
    let project = Some(IpcProjectIdentity {
        name: project_name.to_string(),
        path: project_path.to_string(),
    });
    match editor {
        IpcEditorKind::Pcb => {
            let document_path = require_str(args, "document_path")?;
            if document_path.is_empty() {
                return Err(invalid_arg("document_path", "must not be empty"));
            }
            // Read the schematic-only arguments too so schema/handler drift
            // checks can prove every advertised parameter is intentional.
            if !args["sheet_instance_path"].is_null()
                || !args["sheet_path_human_readable"].is_null()
            {
                return Err(invalid_arg(
                    "sheet_instance_path",
                    "schematic sheet identity is not valid for editor=pcb",
                ));
            }
            Ok(IpcEditorDocument {
                editor,
                project,
                document_path: Some(document_path.to_string()),
                sheet_instance_path: None,
            })
        }
        IpcEditorKind::Schematic => {
            if !args["document_path"].is_null() {
                return Err(invalid_arg(
                    "document_path",
                    "PCB document paths are not schematic sheet identity",
                ));
            }
            let ids = require_array(args, "sheet_instance_path")?
                .iter()
                .map(|value| value.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    invalid_arg("sheet_instance_path", "every entry must be a string")
                })?;
            if ids.is_empty() || ids.iter().any(String::is_empty) {
                return Err(invalid_arg(
                    "sheet_instance_path",
                    "must contain non-empty root-to-leaf KIIIDs",
                ));
            }
            Ok(IpcEditorDocument {
                editor,
                project,
                document_path: None,
                sheet_instance_path: Some(IpcSheetInstancePath {
                    kiids: ids,
                    human_readable: opt_str(args, "sheet_path_human_readable")
                        .unwrap_or("")
                        .to_string(),
                }),
            })
        }
    }
}

fn selection_error_result(error: anyhow::Error) -> CallToolResult {
    if let Some(selection) = konnect_ipc::IpcSelectionObservationError::from_error(&error) {
        let kind = match selection.kind {
            IpcSelectionObservationErrorKind::WrongProject => ToolErrorKind::WrongProject {
                requested: selection.requested.clone(),
                open_projects: selection.candidates.clone(),
            },
            IpcSelectionObservationErrorKind::WrongDocument => ToolErrorKind::WrongDocument {
                requested: selection.requested.clone(),
                open_documents: selection.candidates.clone(),
            },
            IpcSelectionObservationErrorKind::WrongSheetInstance => {
                ToolErrorKind::WrongSheetInstance {
                    requested: selection.requested.clone(),
                    open_sheet_instances: selection.candidates.clone(),
                }
            }
            IpcSelectionObservationErrorKind::AmbiguousDocument => ToolErrorKind::AmbiguousTarget {
                target: selection.requested.clone(),
                candidates: selection.candidates.clone(),
            },
            IpcSelectionObservationErrorKind::UnsupportedObjectType => {
                ToolErrorKind::UnsupportedCapability {
                    capability: format!("decode selected object type {}", selection.requested),
                    kicad_version: None,
                }
            }
            IpcSelectionObservationErrorKind::StaleEditorState
            | IpcSelectionObservationErrorKind::MalformedSelectedObject => {
                ToolErrorKind::StaleTarget {
                    target: selection.requested.clone(),
                    reason: selection.reason.clone(),
                }
            }
        };
        return CallToolResult::error_kind(kind, selection.to_string());
    }
    if let Some(status) = konnect_ipc::ApiStatusError::from_error(&error) {
        if status.is_unsupported() {
            return CallToolResult::error_kind(
                ToolErrorKind::UnsupportedCapability {
                    capability: "selection_observation".to_string(),
                    kicad_version: None,
                },
                "The running KiCad endpoint does not support typed selection observation.",
            );
        }
    }
    match konnect_ipc::IpcFailure::from_error(error) {
        konnect_ipc::IpcFailure::Unreachable(_) => {
            editor_unavailable("the configured KiCad IPC endpoint is unreachable")
        }
        _ => CallToolResult::error_kind(
            ToolErrorKind::StaleTarget {
                target: "requested editor selection".to_string(),
                reason: "KiCad did not return a complete typed selection readback".to_string(),
            },
            "KiCad did not return a complete typed selection readback.",
        ),
    }
}

async fn handle_resolve_navigation_target(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let request = match parse_navigation_target_request(args) {
        Ok(request) => request,
        Err(result) => return Ok(result),
    };
    let live_document = IpcEditorDocument {
        editor: request.editor,
        project: Some(request.project.clone()),
        document_path: (request.editor == IpcEditorKind::Pcb)
            .then(|| request.document_path.display().to_string()),
        sheet_instance_path: request.sheet_instance_path.clone(),
    };
    let address = ctx.config.ipc_address.clone();
    if address.is_empty() {
        return Ok(editor_unavailable("no KiCad IPC endpoint is configured"));
    }
    let observed = tokio::task::spawn_blocking(move || {
        konnect_ipc::KiCadIpcClient::new(address).observe_exact_open_document(&live_document)
    })
    .await?;
    let observed = match observed {
        Ok(observed) => observed,
        Err(error) => return Ok(selection_error_result(error)),
    };

    let resolved = tokio::task::spawn_blocking(move || resolve_navigation_target(&request)).await?;
    match resolved {
        Ok(target) => Ok(CallToolResult::json(&json!({
            "target": target,
            "live_context_evidence": {
                "source": "kicad_ipc_get_open_documents",
                "document": observed
            }
        }))),
        Err(error) => Ok(navigation_target_error_result(error)),
    }
}

fn parse_navigation_target_request(
    args: &serde_json::Value,
) -> Result<NavigationTargetRequest, CallToolResult> {
    let editor = match require_str(args, "editor")? {
        "schematic" => IpcEditorKind::Schematic,
        "pcb" => IpcEditorKind::Pcb,
        _ => return Err(invalid_arg("editor", "expected 'schematic' or 'pcb'")),
    };
    let project_name = require_str(args, "project_name")?;
    let project_path = require_str(args, "project_path")?;
    let document_path = require_str(args, "document_path")?;
    if project_name.is_empty() || project_path.is_empty() || document_path.is_empty() {
        return Err(invalid_arg(
            "project_name",
            "project and document identity strings must not be empty",
        ));
    }

    let sheet_instance_path = match editor {
        IpcEditorKind::Pcb => {
            if !args["sheet_instance_path"].is_null()
                || !args["sheet_path_human_readable"].is_null()
            {
                return Err(invalid_arg(
                    "sheet_instance_path",
                    "PCB targets cannot carry schematic sheet identity",
                ));
            }
            None
        }
        IpcEditorKind::Schematic => {
            let ids = require_array(args, "sheet_instance_path")?
                .iter()
                .map(|value| value.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    invalid_arg("sheet_instance_path", "every entry must be a string")
                })?;
            if ids.is_empty() || ids.iter().any(String::is_empty) {
                return Err(invalid_arg(
                    "sheet_instance_path",
                    "must contain non-empty root-to-leaf KIIIDs",
                ));
            }
            Some(IpcSheetInstancePath {
                kiids: ids,
                human_readable: opt_str(args, "sheet_path_human_readable")
                    .unwrap_or("")
                    .to_string(),
            })
        }
    };
    let object_kiid = opt_str(args, "object_kiid").map(str::to_string);
    let human_reference = opt_str(args, "human_reference").map(str::to_string);
    if object_kiid.as_deref().is_some_and(str::is_empty)
        || human_reference.as_deref().is_some_and(str::is_empty)
        || object_kiid.is_some() == human_reference.is_some()
    {
        return Err(invalid_arg(
            "object_kiid",
            "provide exactly one non-empty object_kiid or human_reference",
        ));
    }
    Ok(NavigationTargetRequest {
        editor,
        project: IpcProjectIdentity {
            name: project_name.to_string(),
            path: project_path.to_string(),
        },
        document_path: PathBuf::from(document_path),
        sheet_instance_path,
        object_kiid,
        human_reference,
    })
}

fn navigation_target_error_result(
    error: super::navigation_target::NavigationTargetError,
) -> CallToolResult {
    let kind = match error.kind {
        NavigationTargetErrorKind::WrongProject => ToolErrorKind::WrongProject {
            requested: error.target.clone(),
            open_projects: error.candidates.clone(),
        },
        NavigationTargetErrorKind::WrongDocument => ToolErrorKind::WrongDocument {
            requested: error.target.clone(),
            open_documents: error.candidates.clone(),
        },
        NavigationTargetErrorKind::WrongSheetInstance => ToolErrorKind::WrongSheetInstance {
            requested: error.target.clone(),
            open_sheet_instances: error.candidates.clone(),
        },
        NavigationTargetErrorKind::AmbiguousTarget => ToolErrorKind::AmbiguousTarget {
            target: error.target.clone(),
            candidates: error.candidates.clone(),
        },
        NavigationTargetErrorKind::StaleTarget => ToolErrorKind::StaleTarget {
            target: error.target.clone(),
            reason: error.reason.clone(),
        },
    };
    CallToolResult::error_kind(kind, error.to_string())
}

#[derive(Debug)]
struct SelectionMutationRequest {
    operation: IpcSelectionMutation,
    live_document: IpcEditorDocument,
    object_kiids: Vec<String>,
    structural_targets: Vec<NavigationTargetRequest>,
}

async fn handle_mutate_editor_selection(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let request = match parse_selection_mutation_request(args) {
        Ok(request) => request,
        Err(result) => return Ok(result),
    };
    let address = ctx.config.ipc_address.clone();
    if address.is_empty() {
        return Ok(editor_unavailable("no KiCad IPC endpoint is configured"));
    }
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let resolved_targets = request
            .structural_targets
            .iter()
            .map(resolve_navigation_target)
            .collect::<Result<Vec<_>, _>>()?;
        let mutation = konnect_ipc::KiCadIpcClient::new(address).mutate_selection(
            &request.live_document,
            request.operation,
            &request.object_kiids,
        )?;
        Ok((resolved_targets, mutation))
    })
    .await?;
    match result {
        Ok((resolved_targets, mutation)) => Ok(CallToolResult::json(&json!({
            "resolved_targets": resolved_targets,
            "mutation": mutation
        }))),
        Err(error) => Ok(selection_mutation_error_result(error)),
    }
}

fn parse_selection_mutation_request(
    args: &serde_json::Value,
) -> Result<SelectionMutationRequest, CallToolResult> {
    let operation = match require_str(args, "operation")? {
        "clear" => IpcSelectionMutation::Clear,
        "add" => IpcSelectionMutation::Add,
        "remove" => IpcSelectionMutation::Remove,
        _ => {
            return Err(invalid_arg(
                "operation",
                "expected 'clear', 'add', or 'remove'",
            ))
        }
    };
    let editor = match require_str(args, "editor")? {
        "schematic" => IpcEditorKind::Schematic,
        "pcb" => IpcEditorKind::Pcb,
        _ => return Err(invalid_arg("editor", "expected 'schematic' or 'pcb'")),
    };
    let project_name = require_str(args, "project_name")?;
    let project_path = require_str(args, "project_path")?;
    let document_path = require_str(args, "document_path")?;
    if project_name.is_empty() || project_path.is_empty() || document_path.is_empty() {
        return Err(invalid_arg(
            "project_name",
            "project and document identity strings must not be empty",
        ));
    }
    let object_kiids = require_array(args, "object_kiids")?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| invalid_arg("object_kiids", "every entry must be a string"))?;
    if object_kiids.iter().any(String::is_empty) {
        return Err(invalid_arg("object_kiids", "KIIIDs must not be empty"));
    }
    let unique = object_kiids
        .iter()
        .collect::<std::collections::BTreeSet<_>>();
    if unique.len() != object_kiids.len() {
        return Err(invalid_arg(
            "object_kiids",
            "duplicate KIIIDs are not allowed",
        ));
    }
    match operation {
        IpcSelectionMutation::Clear if !object_kiids.is_empty() => {
            return Err(invalid_arg("object_kiids", "clear requires an empty array"));
        }
        IpcSelectionMutation::Add | IpcSelectionMutation::Remove if object_kiids.is_empty() => {
            return Err(invalid_arg(
                "object_kiids",
                "add and remove require at least one KIID",
            ));
        }
        _ => {}
    }

    let project = IpcProjectIdentity {
        name: project_name.to_string(),
        path: project_path.to_string(),
    };
    let sheet_instance_path = match editor {
        IpcEditorKind::Pcb => {
            if !args["sheet_instance_path"].is_null()
                || !args["sheet_path_human_readable"].is_null()
            {
                return Err(invalid_arg(
                    "sheet_instance_path",
                    "PCB targets cannot carry schematic sheet identity",
                ));
            }
            None
        }
        IpcEditorKind::Schematic => {
            let ids = require_array(args, "sheet_instance_path")?
                .iter()
                .map(|value| value.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .ok_or_else(|| {
                    invalid_arg("sheet_instance_path", "every entry must be a string")
                })?;
            if ids.is_empty() || ids.iter().any(String::is_empty) {
                return Err(invalid_arg(
                    "sheet_instance_path",
                    "must contain non-empty root-to-leaf KIIIDs",
                ));
            }
            Some(IpcSheetInstancePath {
                kiids: ids,
                human_readable: opt_str(args, "sheet_path_human_readable")
                    .unwrap_or("")
                    .to_string(),
            })
        }
    };
    let saved_document = PathBuf::from(document_path);
    let structural_targets = object_kiids
        .iter()
        .map(|kiid| NavigationTargetRequest {
            editor,
            project: project.clone(),
            document_path: saved_document.clone(),
            sheet_instance_path: sheet_instance_path.clone(),
            object_kiid: Some(kiid.clone()),
            human_reference: None,
        })
        .collect();
    Ok(SelectionMutationRequest {
        operation,
        live_document: IpcEditorDocument {
            editor,
            project: Some(project),
            document_path: (editor == IpcEditorKind::Pcb)
                .then(|| saved_document.display().to_string()),
            sheet_instance_path,
        },
        object_kiids,
        structural_targets,
    })
}

fn selection_mutation_error_result(error: anyhow::Error) -> CallToolResult {
    if let Some(target) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<super::navigation_target::NavigationTargetError>())
    {
        return navigation_target_error_result(target.clone());
    }
    if let Some(mutation) = konnect_ipc::IpcSelectionMutationError::from_error(&error) {
        let kind = match mutation.kind {
            IpcSelectionMutationErrorKind::InvalidRequest => ToolErrorKind::InvalidArgument {
                field: "object_kiids".to_string(),
                reason: mutation.reason.clone(),
            },
            IpcSelectionMutationErrorKind::ReadbackMismatch => ToolErrorKind::ReadbackMismatch {
                operation: mutation.operation.as_str().to_string(),
                requested_kiids: mutation.requested_kiids.clone(),
                before_kiids: mutation.before_kiids.clone(),
                after_kiids: mutation.after_kiids.clone(),
            },
        };
        return CallToolResult::error_kind(kind, mutation.to_string());
    }
    if konnect_ipc::IpcSelectionObservationError::from_error(&error).is_some() {
        return selection_error_result(error);
    }
    if let Some(status) = konnect_ipc::ApiStatusError::from_error(&error) {
        if status.is_unsupported() {
            return CallToolResult::error_kind(
                ToolErrorKind::UnsupportedCapability {
                    capability: "selection_mutation".to_string(),
                    kicad_version: None,
                },
                "The running KiCad endpoint does not support typed selection mutation.",
            );
        }
    }
    match konnect_ipc::IpcFailure::from_error(error) {
        konnect_ipc::IpcFailure::Unreachable(_) => {
            editor_unavailable("the configured KiCad IPC endpoint is unreachable")
        }
        _ => CallToolResult::error_kind(
            ToolErrorKind::StaleTarget {
                target: "requested editor selection mutation".to_string(),
                reason: "KiCad did not return a complete typed mutation/readback sequence"
                    .to_string(),
            },
            "KiCad did not return a complete typed selection mutation/readback sequence.",
        ),
    }
}

async fn handle_resolve_cross_probe_target(
    args: &serde_json::Value,
    ctx: &ToolContext,
) -> anyhow::Result<CallToolResult> {
    let request = match parse_cross_probe_request(args) {
        Ok(request) => request,
        Err(result) => return Ok(result),
    };
    let address = ctx.config.ipc_address.clone();
    if address.is_empty() {
        return Ok(editor_unavailable("no KiCad IPC endpoint is configured"));
    }
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let resolution = resolve_cross_probe(&request)?;
        let source_live = live_cross_probe_document(&request, request.source_editor);
        let destination_editor = match request.source_editor {
            IpcEditorKind::Schematic => IpcEditorKind::Pcb,
            IpcEditorKind::Pcb => IpcEditorKind::Schematic,
        };
        let destination_live = live_cross_probe_document(&request, destination_editor);
        let client = konnect_ipc::KiCadIpcClient::new(address);
        let source_observed = client.observe_exact_open_document(&source_live)?;
        let destination_observed = client.observe_exact_open_document(&destination_live)?;
        Ok((resolution, source_observed, destination_observed))
    })
    .await?;
    match result {
        Ok((resolution, source_observed, destination_observed)) => {
            Ok(CallToolResult::json(&json!({
                "resolution": resolution,
                "live_context_evidence": {
                    "source": "kicad_ipc_get_open_documents",
                    "source_document": source_observed,
                    "destination_document": destination_observed
                },
                "destination_navigation": {
                    "attempted": false,
                    "status": "not_attempted",
                    "reason": "exact activation/reveal is unsupported by the bundled stable protocol; resolution does not imply a state change"
                }
            })))
        }
        Err(error) => Ok(cross_probe_error_result(error)),
    }
}

fn parse_cross_probe_request(
    args: &serde_json::Value,
) -> Result<CrossProbeRequest, CallToolResult> {
    let source_editor = match require_str(args, "source_editor")? {
        "schematic" => IpcEditorKind::Schematic,
        "pcb" => IpcEditorKind::Pcb,
        _ => {
            return Err(invalid_arg(
                "source_editor",
                "expected 'schematic' or 'pcb'",
            ))
        }
    };
    let project_name = require_str(args, "project_name")?;
    let project_path = require_str(args, "project_path")?;
    let schematic_document_path = require_str(args, "schematic_document_path")?;
    let pcb_document_path = require_str(args, "pcb_document_path")?;
    let source_object_kiid = require_str(args, "source_object_kiid")?;
    if [
        project_name,
        project_path,
        schematic_document_path,
        pcb_document_path,
        source_object_kiid,
    ]
    .iter()
    .any(|value| value.is_empty())
    {
        return Err(invalid_arg(
            "source_object_kiid",
            "project, document, and source KIID strings must not be empty",
        ));
    }
    let sheet_kiids = require_array(args, "schematic_sheet_instance_path")?
        .iter()
        .map(|value| value.as_str().map(str::to_string))
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            invalid_arg(
                "schematic_sheet_instance_path",
                "every entry must be a string",
            )
        })?;
    if sheet_kiids.is_empty() || sheet_kiids.iter().any(String::is_empty) {
        return Err(invalid_arg(
            "schematic_sheet_instance_path",
            "must contain non-empty root-to-leaf KIIIDs",
        ));
    }
    Ok(CrossProbeRequest {
        project: IpcProjectIdentity {
            name: project_name.to_string(),
            path: project_path.to_string(),
        },
        schematic_document_path: PathBuf::from(schematic_document_path),
        pcb_document_path: PathBuf::from(pcb_document_path),
        schematic_sheet_instance_path: IpcSheetInstancePath {
            kiids: sheet_kiids,
            human_readable: opt_str(args, "sheet_path_human_readable")
                .unwrap_or("")
                .to_string(),
        },
        source_editor,
        source_object_kiid: source_object_kiid.to_string(),
    })
}

fn live_cross_probe_document(
    request: &CrossProbeRequest,
    editor: IpcEditorKind,
) -> IpcEditorDocument {
    IpcEditorDocument {
        editor,
        project: Some(request.project.clone()),
        document_path: (editor == IpcEditorKind::Pcb)
            .then(|| request.pcb_document_path.display().to_string()),
        sheet_instance_path: (editor == IpcEditorKind::Schematic)
            .then(|| request.schematic_sheet_instance_path.clone()),
    }
}

fn cross_probe_error_result(error: anyhow::Error) -> CallToolResult {
    if let Some(cross_probe) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<CrossProbeError>())
    {
        return match cross_probe {
            CrossProbeError::Target(target) => navigation_target_error_result(target.clone()),
            CrossProbeError::UnresolvedDestination {
                source_kiid,
                reason,
                candidates,
            } => CallToolResult::error_kind(
                ToolErrorKind::UnresolvedCrossProbeDestination {
                    source_kiid: source_kiid.clone(),
                    candidates: candidates.clone(),
                    reason: reason.clone(),
                },
                cross_probe.to_string(),
            ),
        };
    }
    if konnect_ipc::IpcSelectionObservationError::from_error(&error).is_some() {
        return selection_error_result(error);
    }
    match konnect_ipc::IpcFailure::from_error(error) {
        konnect_ipc::IpcFailure::Unreachable(_) => {
            editor_unavailable("a requested cross-probe editor endpoint is unreachable")
        }
        _ => CallToolResult::error_kind(
            ToolErrorKind::StaleTarget {
                target: "requested cross-probe context".to_string(),
                reason: "live or structural evidence was incomplete".to_string(),
            },
            "Cross-probe resolution did not return complete live and structural evidence.",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::error::extract_error_kind;
    use crate::mcp::protocol::ToolContent;
    use crate::router::ToolRouter;
    use crate::tools::ServerConfig;
    use konnect_ipc::builders;
    use konnect_ipc::gen::kiapi;
    use nng::options::Options;
    use prost::Message;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn context(ipc_address: String) -> ToolContext {
        ToolContext::new(
            ServerConfig {
                ipc_address,
                ..Default::default()
            },
            Arc::new(ToolRouter::new()),
        )
    }

    fn selection_project_path() -> &'static str {
        if cfg!(windows) {
            r"C:\design"
        } else {
            "/design"
        }
    }

    fn spawn_observation_mock() -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let url = format!(
            "inproc://editor-state-core-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock socket");
        socket
            .set_opt::<nng::options::RecvTimeout>(Some(Duration::from_secs(10)))
            .expect("timeout");
        socket.listen(&url).expect("listen");
        std::thread::spawn(move || {
            for _ in 0..3 {
                let message = socket.recv().expect("request");
                let request =
                    kiapi::common::ApiRequest::decode(message.as_slice()).expect("decode request");
                let command = request.message.expect("command");
                let response_any = if command.type_url.ends_with("GetVersion") {
                    builders::pack_any(
                        &kiapi::common::commands::GetVersionResponse {
                            version: Some(kiapi::common::types::KiCadVersion {
                                major: 10,
                                minor: 0,
                                patch: 5,
                                full_version: "10.0.5".to_string(),
                            }),
                        },
                        "kiapi.common.commands.GetVersionResponse",
                    )
                } else {
                    builders::pack_any(
                        &kiapi::common::commands::GetOpenDocumentsResponse {
                            documents: Vec::new(),
                        },
                        "kiapi.common.commands.GetOpenDocumentsResponse",
                    )
                };
                let response = kiapi::common::ApiResponse {
                    status: Some(kiapi::common::ApiResponseStatus {
                        status: kiapi::common::ApiStatusCode::AsOk as i32,
                        error_message: String::new(),
                    }),
                    header: None,
                    message: Some(response_any),
                };
                socket
                    .send(nng::Message::from(response.encode_to_vec().as_slice()))
                    .expect("response");
            }
        });
        url
    }

    fn spawn_selection_mock() -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let url = format!(
            "inproc://editor-selection-core-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock socket");
        socket.listen(&url).expect("listen");
        std::thread::spawn(move || {
            for _ in 0..3 {
                let message = socket.recv().expect("request");
                let request =
                    kiapi::common::ApiRequest::decode(message.as_slice()).expect("decode request");
                let command = request.message.expect("command");
                let response_any = if command.type_url.ends_with("GetOpenDocuments") {
                    builders::pack_any(
                        &kiapi::common::commands::GetOpenDocumentsResponse {
                            documents: vec![kiapi::common::types::DocumentSpecifier {
                                r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
                                identifier: Some(
                                    kiapi::common::types::document_specifier::Identifier::BoardFilename(
                                        "navigation.kicad_pcb".to_string(),
                                    ),
                                ),
                                project: Some(kiapi::common::types::ProjectSpecifier {
                                    name: "navigation".to_string(),
                                    path: selection_project_path().to_string(),
                                }),
                            }],
                        },
                        "kiapi.common.commands.GetOpenDocumentsResponse",
                    )
                } else {
                    let footprint = kiapi::board::types::FootprintInstance {
                        id: Some(kiapi::common::types::Kiid {
                            value: "footprint-kiid".to_string(),
                        }),
                        ..Default::default()
                    };
                    builders::pack_any(
                        &kiapi::common::commands::SelectionResponse {
                            items: vec![builders::pack_any(
                                &footprint,
                                "kiapi.board.types.FootprintInstance",
                            )],
                        },
                        "kiapi.common.commands.SelectionResponse",
                    )
                };
                socket
                    .send(nng::Message::from(
                        kiapi::common::ApiResponse {
                            status: Some(kiapi::common::ApiResponseStatus {
                                status: kiapi::common::ApiStatusCode::AsOk as i32,
                                error_message: String::new(),
                            }),
                            header: None,
                            message: Some(response_any),
                        }
                        .encode_to_vec()
                        .as_slice(),
                    ))
                    .expect("response");
            }
        });
        url
    }

    fn spawn_open_board_mock(project_path: String) -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let url = format!(
            "inproc://navigation-resolver-core-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock socket");
        socket.listen(&url).expect("listen");
        std::thread::spawn(move || {
            let message = socket.recv().expect("request");
            let request =
                kiapi::common::ApiRequest::decode(message.as_slice()).expect("decode request");
            assert!(request
                .message
                .as_ref()
                .is_some_and(|message| message.type_url.ends_with("GetOpenDocuments")));
            let response = kiapi::common::ApiResponse {
                status: Some(kiapi::common::ApiResponseStatus {
                    status: kiapi::common::ApiStatusCode::AsOk as i32,
                    error_message: String::new(),
                }),
                header: None,
                message: Some(builders::pack_any(
                    &kiapi::common::commands::GetOpenDocumentsResponse {
                        documents: vec![kiapi::common::types::DocumentSpecifier {
                            r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
                            identifier: Some(
                                kiapi::common::types::document_specifier::Identifier::BoardFilename(
                                    "layout.kicad_pcb".to_string(),
                                ),
                            ),
                            project: Some(kiapi::common::types::ProjectSpecifier {
                                name: "nav".to_string(),
                                path: project_path,
                            }),
                        }],
                    },
                    "kiapi.common.commands.GetOpenDocumentsResponse",
                )),
            };
            socket
                .send(nng::Message::from(response.encode_to_vec().as_slice()))
                .expect("response");
        });
        url
    }

    fn spawn_add_selection_mock(project_path: String, apply_mutation: bool) -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let url = format!(
            "inproc://selection-mutation-core-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock socket");
        socket.listen(&url).expect("listen");
        std::thread::spawn(move || {
            let mut selected = false;
            for _ in 0..8 {
                let message = socket.recv().expect("request");
                let request =
                    kiapi::common::ApiRequest::decode(message.as_slice()).expect("decode request");
                let command = request.message.expect("command");
                let response_any = if command.type_url.ends_with("GetOpenDocuments") {
                    builders::pack_any(
                        &kiapi::common::commands::GetOpenDocumentsResponse {
                            documents: vec![kiapi::common::types::DocumentSpecifier {
                                r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
                                identifier: Some(
                                    kiapi::common::types::document_specifier::Identifier::BoardFilename(
                                        "layout.kicad_pcb".to_string(),
                                    ),
                                ),
                                project: Some(kiapi::common::types::ProjectSpecifier {
                                    name: "nav".to_string(),
                                    path: project_path.clone(),
                                }),
                            }],
                        },
                        "kiapi.common.commands.GetOpenDocumentsResponse",
                    )
                } else {
                    if command.type_url.ends_with("AddToSelection") && apply_mutation {
                        let add = kiapi::common::commands::AddToSelection::decode(
                            command.value.as_slice(),
                        )
                        .expect("add selection");
                        assert_eq!(add.items[0].value, "fp-c10");
                        selected = true;
                    }
                    let items = selected
                        .then(|| {
                            builders::pack_any(
                                &kiapi::board::types::FootprintInstance {
                                    id: Some(kiapi::common::types::Kiid {
                                        value: "fp-c10".to_string(),
                                    }),
                                    ..Default::default()
                                },
                                "kiapi.board.types.FootprintInstance",
                            )
                        })
                        .into_iter()
                        .collect();
                    builders::pack_any(
                        &kiapi::common::commands::SelectionResponse { items },
                        "kiapi.common.commands.SelectionResponse",
                    )
                };
                let response = kiapi::common::ApiResponse {
                    status: Some(kiapi::common::ApiResponseStatus {
                        status: kiapi::common::ApiStatusCode::AsOk as i32,
                        error_message: String::new(),
                    }),
                    header: None,
                    message: Some(response_any),
                };
                socket
                    .send(nng::Message::from(response.encode_to_vec().as_slice()))
                    .expect("response");
            }
        });
        url
    }

    fn spawn_cross_probe_context_mock(
        project_path: String,
        commands: Arc<Mutex<Vec<String>>>,
    ) -> String {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let url = format!(
            "inproc://cross-probe-core-{}",
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let socket = nng::Socket::new(nng::Protocol::Rep0).expect("mock socket");
        socket.listen(&url).expect("listen");
        std::thread::spawn(move || {
            for _ in 0..2 {
                let message = socket.recv().expect("request");
                let request =
                    kiapi::common::ApiRequest::decode(message.as_slice()).expect("decode request");
                let command = request.message.expect("command");
                commands.lock().unwrap().push(command.type_url.clone());
                let query =
                    kiapi::common::commands::GetOpenDocuments::decode(command.value.as_slice())
                        .expect("open documents");
                let document = if query.r#type
                    == kiapi::common::types::DocumentType::DoctypeSchematic as i32
                {
                    kiapi::common::types::DocumentSpecifier {
                        r#type: query.r#type,
                        identifier: Some(
                            kiapi::common::types::document_specifier::Identifier::SheetPath(
                                kiapi::common::types::SheetPath {
                                    path: vec![kiapi::common::types::Kiid {
                                        value: "root".to_string(),
                                    }],
                                    path_human_readable: "/".to_string(),
                                },
                            ),
                        ),
                        project: Some(kiapi::common::types::ProjectSpecifier {
                            name: "nav".to_string(),
                            path: project_path.clone(),
                        }),
                    }
                } else {
                    kiapi::common::types::DocumentSpecifier {
                        r#type: query.r#type,
                        identifier: Some(
                            kiapi::common::types::document_specifier::Identifier::BoardFilename(
                                "nav.kicad_pcb".to_string(),
                            ),
                        ),
                        project: Some(kiapi::common::types::ProjectSpecifier {
                            name: "nav".to_string(),
                            path: project_path.clone(),
                        }),
                    }
                };
                let response = kiapi::common::ApiResponse {
                    status: Some(kiapi::common::ApiResponseStatus {
                        status: kiapi::common::ApiStatusCode::AsOk as i32,
                        error_message: String::new(),
                    }),
                    header: None,
                    message: Some(builders::pack_any(
                        &kiapi::common::commands::GetOpenDocumentsResponse {
                            documents: vec![document],
                        },
                        "kiapi.common.commands.GetOpenDocumentsResponse",
                    )),
                };
                socket
                    .send(nng::Message::from(response.encode_to_vec().as_slice()))
                    .expect("response");
            }
        });
        url
    }

    #[test]
    fn public_tool_is_read_only_and_takes_no_required_arguments() {
        let definitions = tools();
        assert_eq!(definitions.len(), 13);
        let open = definitions
            .iter()
            .find(|tool| tool.name == "open_editor")
            .expect("open tool");
        assert_eq!(open.input_schema["required"], json!(["editor"]));
        let state = definitions
            .iter()
            .find(|tool| tool.name == "get_editor_state")
            .expect("state tool");
        assert_eq!(state.input_schema["required"], json!([]));
        let selection = definitions
            .iter()
            .find(|tool| tool.name == "get_editor_selection")
            .expect("selection tool");
        assert_eq!(
            selection.input_schema["required"],
            json!(["editor", "project_name", "project_path"])
        );
        let resolver = definitions
            .iter()
            .find(|tool| tool.name == "resolve_navigation_target")
            .expect("resolver tool");
        assert_eq!(
            resolver.input_schema["required"],
            json!(["editor", "project_name", "project_path", "document_path"])
        );
        let mutation = definitions
            .iter()
            .find(|tool| tool.name == "mutate_editor_selection")
            .expect("selection mutation tool");
        assert_eq!(
            mutation.input_schema["required"],
            json!([
                "operation",
                "editor",
                "project_name",
                "project_path",
                "document_path",
                "object_kiids"
            ])
        );
        let cross_probe = definitions
            .iter()
            .find(|tool| tool.name == "resolve_cross_probe_target")
            .expect("cross-probe resolver tool");
        assert_eq!(
            cross_probe.input_schema["required"],
            json!([
                "source_editor",
                "project_name",
                "project_path",
                "schematic_document_path",
                "pcb_document_path",
                "schematic_sheet_instance_path",
                "source_object_kiid"
            ])
        );
    }

    #[tokio::test]
    async fn unconfigured_endpoint_is_a_structured_editor_unavailable_refusal() {
        let result = handle_get_editor_state(&json!({}), &context(String::new()))
            .await
            .expect("handler result");
        assert!(result.is_error);
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("editor_unavailable")
        );
    }

    #[tokio::test]
    async fn public_result_is_derived_from_typed_ipc_observation() {
        let result = handle_get_editor_state(&json!({}), &context(spawn_observation_mock()))
            .await
            .expect("handler result");
        assert!(!result.is_error);
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        let body: serde_json::Value = serde_json::from_str(text).expect("json result");
        assert_eq!(body["kicad_version"]["full_version"], "10.0.5");
        assert_eq!(body["evidence_source"], "kicad_ipc");
        assert_eq!(body["editors"].as_array().map(Vec::len), Some(2));
        assert!(body["active_editor"].is_null());
        assert!(body["active_document"].is_null());
        assert!(body["active_sheet_instance"].is_null());
    }

    #[tokio::test]
    async fn public_selection_result_is_derived_from_exact_document_readback() {
        let project_path = selection_project_path();
        let result = handle_get_editor_selection(
            &json!({
                "editor": "pcb",
                "project_name": "navigation",
                "project_path": project_path,
                "document_path": std::path::Path::new(project_path).join("navigation.kicad_pcb")
            }),
            &context(spawn_selection_mock()),
        )
        .await
        .expect("handler result");
        assert!(!result.is_error);
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        let body: serde_json::Value = serde_json::from_str(text).expect("json result");
        assert_eq!(body["editor"], "pcb");
        assert_eq!(body["selected_objects"][0]["kiid"], "footprint-kiid");
        assert_eq!(
            body["evidence_source"],
            "kicad_ipc_get_selection_with_document_readback"
        );
    }

    #[test]
    fn public_selection_parser_refuses_cross_editor_identity_fields() {
        let pcb_with_sheet = parse_selection_target(&json!({
            "editor": "pcb",
            "project_name": "navigation",
            "project_path": r"C:\design",
            "document_path": r"C:\design\navigation.kicad_pcb",
            "sheet_instance_path": ["root"]
        }))
        .expect_err("PCB must not accept a schematic identity");
        assert_eq!(
            extract_error_kind(&pcb_with_sheet).as_deref(),
            Some("invalid_argument")
        );

        let schematic_with_board = parse_selection_target(&json!({
            "editor": "schematic",
            "project_name": "navigation",
            "project_path": r"C:\design",
            "document_path": r"C:\design\navigation.kicad_sch",
            "sheet_instance_path": ["root"]
        }))
        .expect_err("schematic must not accept a PCB path");
        assert_eq!(
            extract_error_kind(&schematic_with_board).as_deref(),
            Some("invalid_argument")
        );
    }

    #[test]
    fn wrong_sheet_instance_maps_to_the_public_structured_refusal() {
        let result = selection_error_result(anyhow::Error::new(
            konnect_ipc::IpcSelectionObservationError {
                kind: IpcSelectionObservationErrorKind::WrongSheetInstance,
                editor: IpcEditorKind::Schematic,
                requested: "/root/requested".to_string(),
                candidates: vec!["/root/other".to_string()],
                reason: "wrong sheet".to_string(),
            },
        ));
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("wrong_sheet_instance")
        );
    }

    #[test]
    fn resolver_parser_requires_exactly_one_identifier() {
        let base = json!({
            "editor": "pcb",
            "project_name": "navigation",
            "project_path": r"C:\design",
            "document_path": r"C:\design\navigation.kicad_pcb"
        });
        let missing = parse_navigation_target_request(&base).expect_err("identifier required");
        assert_eq!(
            extract_error_kind(&missing).as_deref(),
            Some("invalid_argument")
        );

        let mut both = base;
        both["object_kiid"] = json!("id");
        both["human_reference"] = json!("C10");
        let ambiguous = parse_navigation_target_request(&both).expect_err("one identifier only");
        assert_eq!(
            extract_error_kind(&ambiguous).as_deref(),
            Some("invalid_argument")
        );
    }

    #[tokio::test]
    async fn public_resolver_keeps_live_and_structural_evidence_separate() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("nav.kicad_pro"), "{}").unwrap();
        let board = temp.path().join("layout.kicad_pcb");
        std::fs::write(
            &board,
            "(kicad_pcb (footprint \"Capacitor:C\" (layer \"F.Cu\") (at 1 2) \
             (uuid \"fp-c10\") (property \"Reference\" \"C10\")))",
        )
        .unwrap();
        let project_path = temp.path().display().to_string();
        let result = handle_resolve_navigation_target(
            &json!({
                "editor": "pcb",
                "project_name": "nav",
                "project_path": project_path.clone(),
                "document_path": board.display().to_string(),
                "human_reference": "C10"
            }),
            &context(spawn_open_board_mock(project_path)),
        )
        .await
        .expect("handler result");
        assert!(!result.is_error);
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["target"]["object"]["kiid"], "fp-c10");
        assert_eq!(
            body["target"]["structural_evidence"]["source"],
            "saved_kicad_structure"
        );
        assert_eq!(
            body["live_context_evidence"]["source"],
            "kicad_ipc_get_open_documents"
        );
    }

    #[test]
    fn mutation_parser_requires_operation_appropriate_unique_kiids() {
        let base = json!({
            "operation": "clear",
            "editor": "pcb",
            "project_name": "navigation",
            "project_path": r"C:\design",
            "document_path": r"C:\design\navigation.kicad_pcb",
            "object_kiids": []
        });
        assert!(parse_selection_mutation_request(&base).is_ok());

        for object_kiids in [json!(["a"]), json!(["a", "a"])] {
            let mut invalid = base.clone();
            invalid["object_kiids"] = object_kiids;
            if invalid["object_kiids"].as_array().map(Vec::len) == Some(2) {
                invalid["operation"] = json!("add");
            }
            let result = parse_selection_mutation_request(&invalid).expect_err("invalid KIIIDs");
            assert_eq!(
                extract_error_kind(&result).as_deref(),
                Some("invalid_argument")
            );
        }
    }

    #[tokio::test]
    async fn public_selection_mutation_success_is_derived_from_readback() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("nav.kicad_pro"), "{}").unwrap();
        let board = temp.path().join("layout.kicad_pcb");
        std::fs::write(
            &board,
            "(kicad_pcb (footprint \"Capacitor:C\" (layer \"F.Cu\") (at 1 2) \
             (uuid \"fp-c10\") (property \"Reference\" \"C10\")))",
        )
        .unwrap();
        let project_path = temp.path().display().to_string();
        let result = handle_mutate_editor_selection(
            &json!({
                "operation": "add",
                "editor": "pcb",
                "project_name": "nav",
                "project_path": project_path.clone(),
                "document_path": board.display().to_string(),
                "object_kiids": ["fp-c10"]
            }),
            &context(spawn_add_selection_mock(project_path, true)),
        )
        .await
        .expect("handler result");
        assert!(!result.is_error);
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["resolved_targets"][0]["object"]["kiid"], "fp-c10");
        assert_eq!(
            body["mutation"]["after"]["selected_objects"][0]["kiid"],
            "fp-c10"
        );
        assert_eq!(
            body["mutation"]["evidence_source"],
            "kicad_ipc_selection_mutation_with_get_selection_readback"
        );
    }

    #[tokio::test]
    async fn public_selection_mutation_reports_readback_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("nav.kicad_pro"), "{}").unwrap();
        let board = temp.path().join("layout.kicad_pcb");
        std::fs::write(
            &board,
            "(kicad_pcb (footprint \"Capacitor:C\" (layer \"F.Cu\") (at 1 2) \
             (uuid \"fp-c10\") (property \"Reference\" \"C10\")))",
        )
        .unwrap();
        let project_path = temp.path().display().to_string();
        let result = handle_mutate_editor_selection(
            &json!({
                "operation": "add",
                "editor": "pcb",
                "project_name": "nav",
                "project_path": project_path.clone(),
                "document_path": board.display().to_string(),
                "object_kiids": ["fp-c10"]
            }),
            &context(spawn_add_selection_mock(project_path, false)),
        )
        .await
        .expect("handler result");
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("readback_mismatch")
        );
    }

    #[test]
    fn unsupported_selection_mutation_maps_to_a_typed_capability_refusal() {
        let result =
            selection_mutation_error_result(anyhow::Error::new(konnect_ipc::ApiStatusError {
                code: kiapi::common::ApiStatusCode::AsUnhandled as i32,
                code_name: "AS_UNHANDLED".to_string(),
                message: "unsupported".to_string(),
            }));
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("unsupported_capability")
        );
    }

    #[tokio::test]
    async fn public_cross_probe_proves_both_live_contexts_without_state_change() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(temp.path().join("nav.kicad_pro"), "{}").unwrap();
        let schematic = temp.path().join("nav.kicad_sch");
        std::fs::write(
            &schematic,
            "(kicad_sch (uuid \"root\") \
             (symbol (lib_id \"Device:C\") (at 1 2) (uuid \"sym-c10\") \
               (property \"Reference\" \"C10\") \
               (instances (project \"nav\" (path \"/root\" (reference \"C10\") (unit 1))))) \
             (sheet_instances (path \"/\" (page \"1\"))))",
        )
        .unwrap();
        let board = temp.path().join("nav.kicad_pcb");
        std::fs::write(
            &board,
            "(kicad_pcb (footprint \"Capacitor:C\" (layer \"F.Cu\") (at 1 2) \
             (uuid \"fp-c10\") (property \"Reference\" \"C10\") (path \"/sym-c10\")))",
        )
        .unwrap();
        let project_path = temp.path().display().to_string();
        let commands = Arc::new(Mutex::new(Vec::new()));
        let result = handle_resolve_cross_probe_target(
            &json!({
                "source_editor": "schematic",
                "project_name": "nav",
                "project_path": project_path.clone(),
                "schematic_document_path": schematic.display().to_string(),
                "pcb_document_path": board.display().to_string(),
                "schematic_sheet_instance_path": ["root"],
                "sheet_path_human_readable": "/",
                "source_object_kiid": "sym-c10"
            }),
            &context(spawn_cross_probe_context_mock(
                project_path,
                commands.clone(),
            )),
        )
        .await
        .expect("handler result");
        assert!(!result.is_error);
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text result");
        };
        let body: serde_json::Value = serde_json::from_str(text).unwrap();
        assert_eq!(body["resolution"]["source"]["object"]["kiid"], "sym-c10");
        assert_eq!(
            body["resolution"]["destination"]["object"]["kiid"],
            "fp-c10"
        );
        assert_eq!(
            body["resolution"]["evidence"]["source"],
            "saved_kicad_footprint_symbol_path"
        );
        assert_eq!(body["destination_navigation"]["attempted"], false);
        let commands = commands.lock().unwrap();
        assert_eq!(commands.len(), 2);
        assert!(commands
            .iter()
            .all(|command| command.ends_with("GetOpenDocuments")));
    }

    #[test]
    fn unresolved_cross_probe_has_a_public_structured_refusal() {
        let result =
            cross_probe_error_result(anyhow::Error::new(CrossProbeError::UnresolvedDestination {
                source_kiid: "sym-c10".to_string(),
                reason: "duplicate linkage".to_string(),
                candidates: vec!["fp-a".to_string(), "fp-b".to_string()],
            }));
        assert_eq!(
            extract_error_kind(&result).as_deref(),
            Some("unresolved_cross_probe_destination")
        );
    }
}

#[cfg(test)]
mod dialog_tool_tests {
    use super::*;
    use crate::mcp::protocol::ToolContent;
    use crate::router::ToolRouter;
    use crate::test_support::MockIpcServer;
    use crate::tools::ServerConfig;
    use konnect_ipc::gen::kiapi;
    use prost::Message;
    use std::sync::{Arc, Mutex};

    fn context(ipc_address: String) -> ToolContext {
        ToolContext::new(
            ServerConfig {
                kicad_cli: String::new(),
                kicad_binary: String::new(),
                ipc_address,
                project_dir: None,
                jlcpcb_db_path: None,
                auto_load_toolsets: false,
                eager_toolsets: false,
            },
            Arc::new(ToolRouter::new()),
        )
    }

    fn body(result: &CallToolResult) -> serde_json::Value {
        let ToolContent::Text { text } = &result.content[0] else {
            panic!("expected text");
        };
        serde_json::from_str(text).expect("json result")
    }

    fn ok(message: Option<prost_types::Any>) -> kiapi::common::ApiResponse {
        kiapi::common::ApiResponse {
            status: Some(kiapi::common::ApiResponseStatus {
                status: kiapi::common::ApiStatusCode::AsOk as i32,
                error_message: String::new(),
            }),
            header: None,
            message,
        }
    }

    fn pcb_document() -> kiapi::common::types::DocumentSpecifier {
        kiapi::common::types::DocumentSpecifier {
            r#type: kiapi::common::types::DocumentType::DoctypePcb as i32,
            project: None,
            identifier: Some(
                kiapi::common::types::document_specifier::Identifier::BoardFilename(
                    "/p/b.kicad_pcb".to_string(),
                ),
            ),
        }
    }

    /// A fredo-fork KiCad holding one board, recording every request it is sent
    fn spawn_fork(requests: Arc<Mutex<Vec<prost_types::Any>>>) -> MockIpcServer {
        MockIpcServer::spawn("dialog-fork", move |request| {
            let command = request.message.expect("a command");
            requests.lock().unwrap().push(command.clone());
            let name = command.type_url.rsplit('.').next().unwrap().to_string();
            match name.as_str() {
                "GetOpenDocuments" => ok(Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::GetOpenDocumentsResponse {
                        documents: vec![pcb_document()],
                    },
                    "kiapi.common.commands.GetOpenDocumentsResponse",
                ))),
                "RunAction" => ok(Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::RunActionResponse {
                        status: kiapi::common::commands::RunActionStatus::RasOk as i32,
                    },
                    "kiapi.common.commands.RunActionResponse",
                ))),
                "RunChecker" => ok(Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::RunCheckerResponse { started: true },
                    "kiapi.common.commands.RunCheckerResponse",
                ))),
                "CloseDialog" => ok(Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::CloseDialogResponse {
                        found: true,
                        title: "Board Setup".to_string(),
                    },
                    "kiapi.common.commands.CloseDialogResponse",
                ))),
                "GetItems" => {
                    let footprint = kiapi::board::types::FootprintInstance {
                        id: Some(kiapi::common::types::Kiid {
                            value: "fp-r101".to_string(),
                        }),
                        reference_field: Some(kiapi::board::types::Field {
                            text: Some(kiapi::board::types::BoardText {
                                text: Some(kiapi::common::types::Text {
                                    text: "R101".to_string(),
                                    ..Default::default()
                                }),
                                ..Default::default()
                            }),
                            ..Default::default()
                        }),
                        ..Default::default()
                    };
                    ok(Some(konnect_ipc::builders::pack_any(
                        &kiapi::common::commands::GetItemsResponse {
                            status: kiapi::common::types::ItemRequestStatus::IrsOk as i32,
                            items: vec![konnect_ipc::builders::pack_any(
                                &footprint,
                                "kiapi.board.types.FootprintInstance",
                            )],
                            ..Default::default()
                        },
                        "kiapi.common.commands.GetItemsResponse",
                    )))
                }
                "Capture3DView" => ok(Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::Capture3DViewResponse {
                        width: 1280,
                        height: 720,
                    },
                    "kiapi.common.commands.Capture3DViewResponse",
                ))),
                "OpenEditor" | "OpenFootprint" => ok(None),
                "EditItemProperties" => ok(Some(konnect_ipc::builders::pack_any(
                    &kiapi::common::commands::EditItemPropertiesResponse { started: true },
                    "kiapi.common.commands.EditItemPropertiesResponse",
                ))),
                other => panic!("unexpected request {other}"),
            }
        })
    }

    fn sent<M: Message + Default>(requests: &Arc<Mutex<Vec<prost_types::Any>>>, suffix: &str) -> M {
        let any = requests
            .lock()
            .unwrap()
            .iter()
            .find(|any| any.type_url.ends_with(suffix))
            .cloned()
            .unwrap_or_else(|| panic!("{suffix} was not sent"));
        M::decode(any.value.as_slice()).expect("decodes")
    }

    #[tokio::test]
    async fn a_deferred_action_names_its_editor_and_is_marked_deferred() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server = spawn_fork(requests.clone());
        let result = handle_run_editor_action(
            &json!({ "editor": "pcb", "action": "pcbnew.EditorControl.boardSetup", "deferred": true }),
            &context(server.address().to_string()),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", body(&result));
        assert_eq!(body(&result)["deferred"], true);
        let sent: kiapi::common::commands::RunAction = sent(&requests, "RunAction");
        assert_eq!(sent.action, "pcbnew.EditorControl.boardSetup");
        assert!(sent.deferred);
        assert_eq!(
            sent.frame,
            kiapi::common::types::FrameType::FtPcbEditor as i32
        );
    }

    #[tokio::test]
    async fn the_checker_runs_against_the_open_document() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server = spawn_fork(requests.clone());
        let result = handle_run_checker(
            &json!({ "editor": "pcb" }),
            &context(server.address().to_string()),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", body(&result));
        assert_eq!(body(&result)["checker"], "DRC");
        assert_eq!(body(&result)["started"], true);
        let sent: kiapi::common::commands::RunChecker = sent(&requests, "RunChecker");
        assert_eq!(sent.document, Some(pcb_document()));
    }

    #[tokio::test]
    async fn closing_a_dialog_reports_its_title() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server = spawn_fork(requests.clone());
        let result = handle_close_dialog(
            &json!({ "button": "cancel" }),
            &context(server.address().to_string()),
        )
        .await
        .unwrap();
        assert_eq!(
            body(&result),
            json!({ "closed_with": "cancel", "dialog": "Board Setup" })
        );
        let sent: kiapi::common::commands::CloseDialog = sent(&requests, "CloseDialog");
        assert_eq!(sent.button, "cancel");
    }

    #[tokio::test]
    async fn a_footprint_is_edited_by_reference_through_its_kiid() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server = spawn_fork(requests.clone());
        let result = handle_edit_item_properties(
            &json!({ "editor": "pcb", "reference": "R101", "fields": { "Datasheet": "https://example.com/r.pdf" } }),
            &context(server.address().to_string()),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", body(&result));
        assert_eq!(body(&result)["started"], true);
        let sent: kiapi::common::commands::EditItemProperties =
            sent(&requests, "EditItemProperties");
        assert_eq!(sent.item.map(|id| id.value).as_deref(), Some("fp-r101"));
        assert_eq!(sent.fields.len(), 1);
        assert_eq!(sent.fields[0].name, "Datasheet");
        assert_eq!(sent.fields[0].value, "https://example.com/r.pdf");
    }

    #[tokio::test]
    async fn the_3d_view_is_saved_where_asked() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server = spawn_fork(requests.clone());
        let result = handle_capture_3d_view(
            &json!({ "path": "/tmp/view.png" }),
            &context(server.address().to_string()),
        )
        .await
        .unwrap();
        assert_eq!(
            body(&result),
            json!({ "path": "/tmp/view.png", "width": 1280, "height": 720 })
        );
        let sent: kiapi::common::commands::Capture3DView = sent(&requests, "Capture3DView");
        assert_eq!(sent.path, "/tmp/view.png");
        let relative =
            handle_capture_3d_view(&json!({ "path": "view.png" }), &context(String::new()))
                .await
                .unwrap();
        assert_eq!(body(&relative)["error"]["kind"], "invalid_argument");
    }

    #[tokio::test]
    async fn a_footprint_is_opened_after_its_editor() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server = spawn_fork(requests.clone());
        let result = handle_open_footprint_editor(
            &json!({ "library": "Resistor_SMD", "name": "R_0603_1608Metric" }),
            &context(server.address().to_string()),
        )
        .await
        .unwrap();
        assert_eq!(body(&result)["opened"], "Resistor_SMD:R_0603_1608Metric");
        let order: Vec<String> = requests
            .lock()
            .unwrap()
            .iter()
            .map(|any| any.type_url.rsplit('.').next().unwrap().to_string())
            .collect();
        assert_eq!(order, ["OpenEditor", "OpenFootprint"]);
        let sent: kiapi::common::commands::OpenFootprint = sent(&requests, "OpenFootprint");
        let id = sent.lib_id.unwrap();
        assert_eq!(
            (id.library_nickname.as_str(), id.entry_name.as_str()),
            ("Resistor_SMD", "R_0603_1608Metric")
        );
    }

    #[tokio::test]
    async fn an_action_can_target_the_3d_viewer() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server = spawn_fork(requests.clone());
        let result = handle_run_editor_action(
            &json!({ "editor": "3d", "action": "3DViewer.Control.viewTop" }),
            &context(server.address().to_string()),
        )
        .await
        .unwrap();
        assert!(!result.is_error, "{:?}", body(&result));
        let sent: kiapi::common::commands::RunAction = sent(&requests, "RunAction");
        assert_eq!(
            sent.frame,
            kiapi::common::types::FrameType::Ft3dViewer as i32
        );
    }

    #[tokio::test]
    async fn bad_arguments_are_refused_before_any_request() {
        let ctx = context(String::new());
        let editor = handle_run_editor_action(&json!({ "editor": "gerber", "action": "x" }), &ctx)
            .await
            .unwrap();
        let button = handle_close_dialog(&json!({ "button": "yes" }), &ctx)
            .await
            .unwrap();
        let fields = handle_edit_item_properties(
            &json!({ "editor": "pcb", "reference": "R1", "fields": {} }),
            &ctx,
        )
        .await
        .unwrap();
        let sheet = handle_edit_item_properties(
            &json!({ "editor": "schematic", "reference": "R1", "fields": { "Value": "1k" } }),
            &ctx,
        )
        .await
        .unwrap();
        for result in [editor, button, fields, sheet] {
            assert!(result.is_error);
            assert_eq!(body(&result)["error"]["kind"], "invalid_argument");
        }
    }
}
