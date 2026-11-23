//! Identify the objects in a KiCad `SelectionResponse`.
//!
//! A selection readback needs two facts per object: what kind of thing it is
//! and its KIID. Both are available without decoding the object: the kind is
//! the protobuf type name in the `Any`'s type URL, and the KIID sits at a known
//! field number.
//!
//! That matters because Konnect talks to two generations of the protocol. The
//! vendored `.proto` files are KiCad 10.0's. KiCad 10.99 (the 11 development
//! branch) rewrote `schematic_types.proto`:
//!
//! - `Line` became `SchematicLine`;
//! - symbols, junctions, no-connects, sheets and the rest are now serialized
//!   (`SchematicSymbolInstance`, `Junction`, `NoConnectMarker`, `SheetSymbol`,
//!   ...), where 10.0 had no message for them;
//! - the four label messages kept their names but not their layout: field 3
//!   was the wrapper `kiapi.schematic.types.Text` and is now
//!   `kiapi.common.types.Text` itself, so the 10.0 message of the same name
//!   decodes a 10.99 label without an error and without its text.
//!
//! What both generations agree on is where the KIID is, so this module reads
//! only that. Decoding a selected object in full would tie every selection
//! call to one generation's layout of fields the readback never uses, and a
//! same-named message is no proof of the same layout.
//!
//! Nothing KiCad reports as selected is dropped or fails the readback. A type
//! missing from [`KNOWN_TYPES`] is listed as [`UNRECOGNIZED_OBJECT_TYPE`] under
//! its protobuf type name, with the KIID from its lowest-numbered field that
//! holds nothing but one, if it has such a field.

use crate::gen::kiapi;
use crate::types::{IpcSelectedKiidEvidence, IpcSelectedObject};
use prost::Message;

/// `object_type` of a selected object whose protobuf type is not in
/// [`KNOWN_TYPES`].
pub const UNRECOGNIZED_OBJECT_TYPE: &str = "unrecognized";

/// Protobuf type name, the `object_type` reported for it, and where its own
/// KIID is: a chain of embedded-message field numbers ending at a
/// `kiapi.common.types.KIID`.
///
/// Field numbers were read from `api/proto` of KiCad 10.0.4 and of upstream
/// master at df8ee89812 (10.99). A name both generations use has the same KIID
/// path in both.
const KNOWN_TYPES: &[(&str, &str, &[u32])] = &[
    // Board: the same names and KIID paths in 10.0 and 10.99.
    ("kiapi.board.types.FootprintInstance", "pcb_footprint", &[1]),
    ("kiapi.board.types.Pad", "pcb_pad", &[1]),
    ("kiapi.board.types.BoardGraphicShape", "pcb_shape", &[4]),
    ("kiapi.board.types.BoardText", "pcb_text", &[1]),
    ("kiapi.board.types.BoardTextBox", "pcb_text_box", &[1]),
    ("kiapi.board.types.Track", "pcb_trace", &[1]),
    ("kiapi.board.types.Via", "pcb_via", &[1]),
    ("kiapi.board.types.Arc", "pcb_arc", &[1]),
    ("kiapi.board.types.Dimension", "pcb_dimension", &[1]),
    ("kiapi.board.types.Zone", "pcb_zone", &[1]),
    ("kiapi.board.types.Group", "pcb_group", &[1]),
    // A field has no KIID of its own; its text does (`Field.text.id`).
    ("kiapi.board.types.Field", "pcb_field", &[3, 1]),
    // Schematic, 10.0 only.
    ("kiapi.schematic.types.Line", "schematic_line", &[1]),
    // Schematic, 10.99 only. `SchematicLine` is 10.0's `Line` renamed.
    (
        "kiapi.schematic.types.SchematicLine",
        "schematic_line",
        &[1],
    ),
    // Schematic, both generations (see the module note on their layout).
    ("kiapi.schematic.types.LocalLabel", "schematic_label", &[1]),
    (
        "kiapi.schematic.types.GlobalLabel",
        "schematic_global_label",
        &[1],
    ),
    (
        "kiapi.schematic.types.HierarchicalLabel",
        "schematic_hierarchical_label",
        &[1],
    ),
    (
        "kiapi.schematic.types.DirectiveLabel",
        "schematic_directive_label",
        &[1],
    ),
    // Schematic, 10.99 only.
    (
        "kiapi.schematic.types.SchematicSymbolInstance",
        "schematic_symbol",
        &[1],
    ),
    ("kiapi.schematic.types.Junction", "schematic_junction", &[1]),
    (
        "kiapi.schematic.types.NoConnectMarker",
        "schematic_no_connect",
        &[1],
    ),
    (
        "kiapi.schematic.types.BusEntry",
        "schematic_bus_entry",
        &[1],
    ),
    ("kiapi.schematic.types.SheetSymbol", "schematic_sheet", &[1]),
    (
        "kiapi.schematic.types.SheetPin",
        "schematic_sheet_pin",
        &[1],
    ),
    (
        "kiapi.schematic.types.SchematicText",
        "schematic_text",
        &[1],
    ),
    (
        "kiapi.schematic.types.SchematicTextBox",
        "schematic_text_box",
        &[1],
    ),
    // The shape comes first in this message; the KIID is field 2.
    (
        "kiapi.schematic.types.SchematicGraphicShape",
        "schematic_shape",
        &[2],
    ),
    (
        "kiapi.schematic.types.SchematicImage",
        "schematic_image",
        &[1],
    ),
    ("kiapi.schematic.types.SchematicPin", "schematic_pin", &[1]),
    ("kiapi.schematic.types.Group", "schematic_group", &[1]),
    (
        "kiapi.schematic.types.SchematicRuleArea",
        "schematic_rule_area",
        &[1],
    ),
    (
        "kiapi.schematic.types.SchematicTable",
        "schematic_table",
        &[1],
    ),
    // A table cell is a text box in a wrapper (`SchematicTableCell.text_box.id`).
    (
        "kiapi.schematic.types.SchematicTableCell",
        "schematic_table_cell",
        &[1, 1],
    ),
    // Fields have no KIID in 10.99; the proto reserves field 1 for one, so the
    // KIID is reported once KiCad starts sending it.
    (
        "kiapi.schematic.types.SchematicField",
        "schematic_field",
        &[1],
    ),
];

/// Describe one item of a `SelectionResponse`. Never fails: an object that
/// cannot be identified is still reported, by its protobuf type.
pub(crate) fn identify_selected_object(item: &prost_types::Any) -> IpcSelectedObject {
    let protocol_type = crate::builders::any_type_name(item);
    let known = KNOWN_TYPES
        .iter()
        .find(|(name, _, _)| *name == protocol_type);
    let (object_type, kiid, found) = match known {
        Some((_, object_type, id_path)) => (
            *object_type,
            kiid_at_path(&item.value, id_path),
            IpcSelectedKiidEvidence::KnownTypeField,
        ),
        None => (
            UNRECOGNIZED_OBJECT_TYPE,
            probe_own_kiid(&item.value),
            IpcSelectedKiidEvidence::ProbedField,
        ),
    };
    let kiid_evidence = if kiid.is_some() {
        found
    } else {
        IpcSelectedKiidEvidence::Unreadable
    };
    IpcSelectedObject {
        kiid,
        object_type: object_type.to_string(),
        protocol_type: protocol_type.to_string(),
        kiid_evidence,
    }
}

/// One top-level field of a serialized message: its number and, for a
/// length-delimited field, its payload.
type WireField<'a> = (u32, Option<&'a [u8]>);

fn read_varint(buf: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for index in 0..10 {
        let byte = *buf.get(index)?;
        value |= u64::from(byte & 0x7f) << (7 * index);
        if byte & 0x80 == 0 {
            *buf = &buf[index + 1..];
            return Some(value);
        }
    }
    None
}

/// Split a serialized message into its top-level fields, or `None` when the
/// bytes are not a well-formed message.
///
/// Written out rather than borrowed from prost because the helpers that walk
/// fields without a schema are not part of prost's documented interface.
fn top_level_fields(mut buf: &[u8]) -> Option<Vec<WireField<'_>>> {
    let mut fields = Vec::new();
    while !buf.is_empty() {
        let key = read_varint(&mut buf)?;
        let number = u32::try_from(key >> 3).ok().filter(|number| *number != 0)?;
        let payload = match key & 7 {
            0 => {
                read_varint(&mut buf)?;
                None
            }
            1 => {
                buf = buf.get(8..)?;
                None
            }
            2 => {
                let length = usize::try_from(read_varint(&mut buf)?).ok()?;
                let (payload, rest) = buf.split_at_checked(length)?;
                buf = rest;
                Some(payload)
            }
            5 => {
                buf = buf.get(4..)?;
                None
            }
            // Groups are proto2 only and KiCad's API is proto3; 6 and 7 are
            // not wire types.
            _ => return None,
        };
        fields.push((number, payload));
    }
    Some(fields)
}

/// The serialized embedded message in field `number`, or `None` when the field
/// is absent or is not an embedded message.
fn embedded_message(message: &[u8], number: u32) -> Option<Vec<u8>> {
    let mut merged: Option<Vec<u8>> = None;
    for (field, payload) in top_level_fields(message)? {
        if field == number {
            // A singular message that occurs more than once merges, which for
            // serialized messages is concatenation.
            merged
                .get_or_insert_with(Vec::new)
                .extend_from_slice(payload?);
        }
    }
    merged
}

/// The KIID at `path` in a serialized message, if the path leads to a
/// non-empty one.
fn kiid_at_path(message: &[u8], path: &[u32]) -> Option<String> {
    let (&first, rest) = path.split_first()?;
    let mut current = embedded_message(message, first)?;
    for &number in rest {
        current = embedded_message(&current, number)?;
    }
    let kiid = kiapi::common::types::Kiid::decode(current.as_slice()).ok()?;
    (!kiid.value.is_empty()).then_some(kiid.value)
}

/// Best-effort KIID of an object whose type is not in [`KNOWN_TYPES`].
///
/// Every item message in both protocol generations stores its own KIID in a
/// singular top-level field, ahead of any `parent` KIID. So the candidate is
/// the lowest-numbered field that occurs once and holds nothing but a KIID in
/// canonical UUID form. A field that occurs more than once is a list of other
/// objects' KIIDs (`Group.items`) and is passed over.
fn probe_own_kiid(message: &[u8]) -> Option<String> {
    let fields = top_level_fields(message)?;
    let mut candidates: Vec<(u32, &str)> = fields
        .iter()
        .filter_map(|(number, payload)| Some((*number, bare_kiid((*payload)?)?)))
        .filter(|(number, _)| fields.iter().filter(|(other, _)| other == number).count() == 1)
        .collect();
    candidates.sort_by_key(|(number, _)| *number);
    candidates.first().map(|(_, kiid)| (*kiid).to_string())
}

/// The UUID text of a payload that is exactly a `kiapi.common.types.KIID`:
/// one field, number 1, holding a canonical UUID.
fn bare_kiid(payload: &[u8]) -> Option<&str> {
    let fields = top_level_fields(payload)?;
    let [(1, Some(value))] = fields.as_slice() else {
        return None;
    };
    let text = std::str::from_utf8(value).ok()?;
    is_canonical_uuid(text).then_some(text)
}

fn is_canonical_uuid(text: &str) -> bool {
    text.len() == 36
        && text.bytes().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builders::pack_any;

    const UUID_A: &str = "08527288-3601-4b95-b2e8-c2d72ccef881";
    const UUID_B: &str = "f4a4c65b-674b-4d0f-a935-3abb456ee78d";

    fn kiid(value: &str) -> kiapi::common::types::Kiid {
        kiapi::common::types::Kiid {
            value: value.to_string(),
        }
    }

    /// KiCad 10.99's messages, reduced to the fields these tests fill, with
    /// upstream's field numbers (`api/proto/schematic/schematic_types.proto`
    /// and `common/types/base_types.proto` at df8ee89812).
    mod v1099 {
        use crate::gen::kiapi::common::types::{Kiid, Vector2};

        /// `kiapi.common.types.Text`, as much as a label needs. The same
        /// field numbers as in 10.0.
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct CommonText {
            #[prost(message, optional, tag = "2")]
            pub position: Option<Vector2>,
            #[prost(string, tag = "5")]
            pub text: String,
        }

        /// `LocalLabel`, `GlobalLabel`, `HierarchicalLabel`, `DirectiveLabel`:
        /// fields 1 to 4 are the same in all four.
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct Label {
            #[prost(message, optional, tag = "1")]
            pub id: Option<Kiid>,
            #[prost(message, optional, tag = "2")]
            pub position: Option<Vector2>,
            #[prost(message, optional, tag = "3")]
            pub text: Option<CommonText>,
            #[prost(int32, tag = "4")]
            pub spin_style: i32,
        }

        /// `SchematicLine`.
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct SchematicLine {
            #[prost(message, optional, tag = "1")]
            pub id: Option<Kiid>,
            #[prost(message, optional, tag = "2")]
            pub start: Option<Vector2>,
            #[prost(message, optional, tag = "3")]
            pub end: Option<Vector2>,
            #[prost(int32, tag = "4")]
            pub r#type: i32,
        }

        /// The shape of every 10.99 item whose KIID is field 1 and whose
        /// position is field 2: `Junction`, `NoConnectMarker`, `BusEntry`,
        /// `SchematicImage`, `SheetPin`, `SheetSymbol`.
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct IdThenPosition {
            #[prost(message, optional, tag = "1")]
            pub id: Option<Kiid>,
            #[prost(message, optional, tag = "2")]
            pub position: Option<Vector2>,
        }

        /// `kiapi.common.types.SheetPath`.
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct SheetPath {
            #[prost(message, repeated, tag = "1")]
            pub path: Vec<Kiid>,
            #[prost(string, tag = "2")]
            pub path_human_readable: String,
        }

        /// `SchematicField`: no KIID.
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct SchematicField {
            #[prost(string, tag = "2")]
            pub name: String,
            #[prost(bool, tag = "4")]
            pub visible: bool,
        }

        /// `SchematicSymbolInstance`: the KIID, then the sheet path the
        /// instance was read from, which is itself a list of KIIDs.
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct SchematicSymbolInstance {
            #[prost(message, optional, tag = "1")]
            pub id: Option<Kiid>,
            #[prost(message, optional, tag = "2")]
            pub path: Option<SheetPath>,
            #[prost(message, optional, tag = "3")]
            pub position: Option<Vector2>,
            #[prost(message, optional, tag = "7")]
            pub reference_field: Option<SchematicField>,
        }

        /// `SchematicGraphicShape`: the KIID is field 2. The shape is filled
        /// with a position here only to put a message in field 1.
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct SchematicGraphicShape {
            #[prost(message, optional, tag = "1")]
            pub shape: Option<Vector2>,
            #[prost(message, optional, tag = "2")]
            pub id: Option<Kiid>,
        }

        /// `SchematicTableCell`: a `SchematicTextBox` (KIID in field 1) in
        /// field 1.
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct SchematicTableCell {
            #[prost(message, optional, tag = "1")]
            pub text_box: Option<IdThenPosition>,
            #[prost(int32, tag = "2")]
            pub column_span: i32,
        }

        /// `kiapi.schematic.types.Group` and `kiapi.board.types.Group`.
        #[derive(Clone, PartialEq, prost::Message)]
        pub struct Group {
            #[prost(message, optional, tag = "1")]
            pub id: Option<Kiid>,
            #[prost(string, tag = "2")]
            pub name: String,
            #[prost(message, repeated, tag = "3")]
            pub items: Vec<Kiid>,
        }
    }

    fn position() -> Option<kiapi::common::types::Vector2> {
        Some(kiapi::common::types::Vector2 {
            x_nm: 127_000_000,
            y_nm: 63_500_000,
        })
    }

    fn assert_known(item: &prost_types::Any, object_type: &str, kiid: &str) {
        let object = identify_selected_object(item);
        assert_eq!(object.object_type, object_type, "{}", item.type_url);
        assert_eq!(object.kiid.as_deref(), Some(kiid), "{}", item.type_url);
        assert_eq!(
            object.kiid_evidence,
            IpcSelectedKiidEvidence::KnownTypeField,
            "{}",
            item.type_url
        );
        assert_eq!(
            format!("type.googleapis.com/{}", object.protocol_type),
            item.type_url
        );
    }

    #[test]
    fn the_10_0_schematic_messages_are_identified() {
        use kiapi::schematic::types as sch;
        let id = Some(kiid(UUID_A));
        let text = Some(sch::Text {
            text: Some(kiapi::common::types::Text {
                text: "VBUS".to_string(),
                ..Default::default()
            }),
        });
        assert_known(
            &pack_any(
                &sch::Line {
                    id: id.clone(),
                    start: position(),
                    end: position(),
                    layer: 0,
                },
                "kiapi.schematic.types.Line",
            ),
            "schematic_line",
            UUID_A,
        );
        assert_known(
            &pack_any(
                &sch::LocalLabel {
                    id: id.clone(),
                    position: position(),
                    text: text.clone(),
                },
                "kiapi.schematic.types.LocalLabel",
            ),
            "schematic_label",
            UUID_A,
        );
        assert_known(
            &pack_any(
                &sch::GlobalLabel {
                    id: id.clone(),
                    position: position(),
                    text: text.clone(),
                },
                "kiapi.schematic.types.GlobalLabel",
            ),
            "schematic_global_label",
            UUID_A,
        );
        assert_known(
            &pack_any(
                &sch::HierarchicalLabel {
                    id: id.clone(),
                    position: position(),
                    text: text.clone(),
                },
                "kiapi.schematic.types.HierarchicalLabel",
            ),
            "schematic_hierarchical_label",
            UUID_A,
        );
        assert_known(
            &pack_any(
                &sch::DirectiveLabel {
                    id,
                    position: position(),
                    text,
                },
                "kiapi.schematic.types.DirectiveLabel",
            ),
            "schematic_directive_label",
            UUID_A,
        );
    }

    #[test]
    fn the_10_99_schematic_messages_are_identified() {
        let id = Some(kiid(UUID_A));
        let label = v1099::Label {
            id: id.clone(),
            position: position(),
            text: Some(v1099::CommonText {
                position: position(),
                text: "VBUS".to_string(),
            }),
            spin_style: 3,
        };
        for (name, object_type) in [
            ("LocalLabel", "schematic_label"),
            ("GlobalLabel", "schematic_global_label"),
            ("HierarchicalLabel", "schematic_hierarchical_label"),
            ("DirectiveLabel", "schematic_directive_label"),
        ] {
            assert_known(
                &pack_any(&label, &format!("kiapi.schematic.types.{name}")),
                object_type,
                UUID_A,
            );
        }

        assert_known(
            &pack_any(
                &v1099::SchematicLine {
                    id: id.clone(),
                    start: position(),
                    end: position(),
                    r#type: 1,
                },
                "kiapi.schematic.types.SchematicLine",
            ),
            "schematic_line",
            UUID_A,
        );

        let placed = v1099::IdThenPosition {
            id: id.clone(),
            position: position(),
        };
        for (name, object_type) in [
            ("Junction", "schematic_junction"),
            ("NoConnectMarker", "schematic_no_connect"),
            ("BusEntry", "schematic_bus_entry"),
            ("SchematicImage", "schematic_image"),
            ("SheetPin", "schematic_sheet_pin"),
            ("SheetSymbol", "schematic_sheet"),
            // Their field 2 is not a position, but it is a message, which is
            // all this reader looks at.
            ("SchematicText", "schematic_text"),
            ("SchematicTextBox", "schematic_text_box"),
            ("SchematicRuleArea", "schematic_rule_area"),
        ] {
            assert_known(
                &pack_any(&placed, &format!("kiapi.schematic.types.{name}")),
                object_type,
                UUID_A,
            );
        }

        assert_known(
            &pack_any(
                &v1099::Group {
                    id: id.clone(),
                    name: "block".to_string(),
                    items: vec![kiid(UUID_B), kiid(UUID_B)],
                },
                "kiapi.schematic.types.Group",
            ),
            "schematic_group",
            UUID_A,
        );
        assert_known(
            &pack_any(
                &v1099::SchematicGraphicShape {
                    shape: position(),
                    id: id.clone(),
                },
                "kiapi.schematic.types.SchematicGraphicShape",
            ),
            "schematic_shape",
            UUID_A,
        );
        assert_known(
            &pack_any(
                &v1099::SchematicTableCell {
                    text_box: Some(placed),
                    column_span: 1,
                },
                "kiapi.schematic.types.SchematicTableCell",
            ),
            "schematic_table_cell",
            UUID_A,
        );
    }

    #[test]
    fn a_10_99_symbol_is_identified_by_its_own_kiid_not_its_sheet_path() {
        let symbol = v1099::SchematicSymbolInstance {
            id: Some(kiid(UUID_A)),
            path: Some(v1099::SheetPath {
                path: vec![kiid(UUID_B)],
                path_human_readable: "/".to_string(),
            }),
            position: position(),
            reference_field: Some(v1099::SchematicField {
                name: "Reference".to_string(),
                visible: true,
            }),
        };
        assert_known(
            &pack_any(&symbol, "kiapi.schematic.types.SchematicSymbolInstance"),
            "schematic_symbol",
            UUID_A,
        );
    }

    /// Why the labels are not decoded in full: the 10.0 message of the same
    /// name accepts a 10.99 label, whose field 3 has another layout, and
    /// comes back without the text. The KIID is where both agree.
    #[test]
    fn a_10_99_label_decoded_as_the_10_0_message_of_the_same_name_loses_its_text() {
        let label = v1099::Label {
            id: Some(kiid(UUID_A)),
            position: position(),
            text: Some(v1099::CommonText {
                position: position(),
                text: "VBUS".to_string(),
            }),
            spin_style: 1,
        };
        let item = pack_any(&label, "kiapi.schematic.types.LocalLabel");
        let as_10_0 = kiapi::schematic::types::LocalLabel::decode(item.value.as_slice())
            .expect("unknown fields are skipped, not refused");
        assert_eq!(as_10_0.id, Some(kiid(UUID_A)));
        assert_eq!(
            as_10_0.text.and_then(|wrapper| wrapper.text),
            None,
            "the label layouts were expected to differ between generations"
        );
        assert_known(&item, "schematic_label", UUID_A);
    }

    #[test]
    fn every_board_type_in_the_table_reads_the_kiid_the_10_0_message_carries() {
        use kiapi::board::types as board;
        let id = Some(kiid(UUID_A));
        let parent = Some(kiid(UUID_B));
        let items = [
            (
                pack_any(
                    &board::FootprintInstance {
                        id: id.clone(),
                        parent: parent.clone(),
                        ..Default::default()
                    },
                    "kiapi.board.types.FootprintInstance",
                ),
                "pcb_footprint",
            ),
            (
                pack_any(
                    &board::Pad {
                        id: id.clone(),
                        parent: parent.clone(),
                        ..Default::default()
                    },
                    "kiapi.board.types.Pad",
                ),
                "pcb_pad",
            ),
            (
                pack_any(
                    &board::BoardGraphicShape {
                        id: id.clone(),
                        parent: parent.clone(),
                        shape: Some(Default::default()),
                        ..Default::default()
                    },
                    "kiapi.board.types.BoardGraphicShape",
                ),
                "pcb_shape",
            ),
            (
                pack_any(
                    &board::BoardText {
                        id: id.clone(),
                        parent: parent.clone(),
                        ..Default::default()
                    },
                    "kiapi.board.types.BoardText",
                ),
                "pcb_text",
            ),
            (
                pack_any(
                    &board::BoardTextBox {
                        id: id.clone(),
                        parent: parent.clone(),
                        ..Default::default()
                    },
                    "kiapi.board.types.BoardTextBox",
                ),
                "pcb_text_box",
            ),
            (
                pack_any(
                    &board::Track {
                        id: id.clone(),
                        parent: parent.clone(),
                        start: position(),
                        ..Default::default()
                    },
                    "kiapi.board.types.Track",
                ),
                "pcb_trace",
            ),
            (
                pack_any(
                    &board::Via {
                        id: id.clone(),
                        parent: parent.clone(),
                        ..Default::default()
                    },
                    "kiapi.board.types.Via",
                ),
                "pcb_via",
            ),
            (
                pack_any(
                    &board::Arc {
                        id: id.clone(),
                        parent: parent.clone(),
                        ..Default::default()
                    },
                    "kiapi.board.types.Arc",
                ),
                "pcb_arc",
            ),
            (
                pack_any(
                    &board::Dimension {
                        id: id.clone(),
                        parent: parent.clone(),
                        ..Default::default()
                    },
                    "kiapi.board.types.Dimension",
                ),
                "pcb_dimension",
            ),
            (
                pack_any(
                    &board::Zone {
                        id: id.clone(),
                        parent: parent.clone(),
                        ..Default::default()
                    },
                    "kiapi.board.types.Zone",
                ),
                "pcb_zone",
            ),
            (
                pack_any(
                    &board::Group {
                        id: id.clone(),
                        parent: parent.clone(),
                        items: vec![kiid(UUID_B), kiid(UUID_B)],
                        ..Default::default()
                    },
                    "kiapi.board.types.Group",
                ),
                "pcb_group",
            ),
            (
                pack_any(
                    &board::Field {
                        name: "Reference".to_string(),
                        text: Some(board::BoardText {
                            id: id.clone(),
                            parent,
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                    "kiapi.board.types.Field",
                ),
                "pcb_field",
            ),
        ];
        let board_rows = KNOWN_TYPES
            .iter()
            .filter(|(name, _, _)| name.starts_with("kiapi.board."))
            .count();
        assert_eq!(items.len(), board_rows, "a board row has no test message");
        for (item, object_type) in &items {
            assert_known(item, object_type, UUID_A);
        }
    }

    #[test]
    fn a_known_type_accepts_any_non_empty_kiid_text() {
        // Only the probe for unknown types insists on UUID form.
        let footprint = kiapi::board::types::FootprintInstance {
            id: Some(kiid("footprint-kiid")),
            ..Default::default()
        };
        assert_known(
            &pack_any(&footprint, "kiapi.board.types.FootprintInstance"),
            "pcb_footprint",
            "footprint-kiid",
        );
    }

    #[test]
    fn a_known_type_without_its_kiid_is_listed_as_unreadable() {
        let no_id = identify_selected_object(&pack_any(
            &kiapi::board::types::FootprintInstance::default(),
            "kiapi.board.types.FootprintInstance",
        ));
        assert_eq!(no_id.object_type, "pcb_footprint");
        assert_eq!(no_id.kiid, None);
        assert_eq!(no_id.kiid_evidence, IpcSelectedKiidEvidence::Unreadable);

        // 10.99 serializes a selected field; it has no KIID.
        let field = identify_selected_object(&pack_any(
            &v1099::SchematicField {
                name: "Reference".to_string(),
                visible: true,
            },
            "kiapi.schematic.types.SchematicField",
        ));
        assert_eq!(field.object_type, "schematic_field");
        assert_eq!(field.kiid, None);
        assert_eq!(field.kiid_evidence, IpcSelectedKiidEvidence::Unreadable);

        let truncated = identify_selected_object(&prost_types::Any {
            type_url: "type.googleapis.com/kiapi.schematic.types.Junction".to_string(),
            value: vec![0x0a, 0x26, 0x0a],
        });
        assert_eq!(truncated.object_type, "schematic_junction");
        assert_eq!(truncated.kiid, None);
        assert_eq!(truncated.kiid_evidence, IpcSelectedKiidEvidence::Unreadable);
    }

    #[test]
    fn an_unknown_type_is_listed_with_the_kiid_it_carries() {
        // The layout of board items: KIID first, parent KIID later.
        let item = pack_any(
            &kiapi::board::types::ReferenceImage {
                id: Some(kiid(UUID_A)),
                parent: Some(kiid(UUID_B)),
                ..Default::default()
            },
            "kiapi.board.types.ReferenceImage",
        );
        let object = identify_selected_object(&item);
        assert_eq!(object.object_type, UNRECOGNIZED_OBJECT_TYPE);
        assert_eq!(object.protocol_type, "kiapi.board.types.ReferenceImage");
        assert_eq!(object.kiid.as_deref(), Some(UUID_A));
        assert_eq!(object.kiid_evidence, IpcSelectedKiidEvidence::ProbedField);

        // A KIID that is not the first field is still found.
        let shape = identify_selected_object(&pack_any(
            &v1099::SchematicGraphicShape {
                shape: position(),
                id: Some(kiid(UUID_A)),
            },
            "kiapi.schematic.types.SomethingNewer",
        ));
        assert_eq!(shape.object_type, UNRECOGNIZED_OBJECT_TYPE);
        assert_eq!(shape.kiid.as_deref(), Some(UUID_A));
    }

    #[test]
    fn an_unknown_type_with_no_kiid_of_its_own_is_listed_without_one() {
        let unreadable = |item: &prost_types::Any| {
            let object = identify_selected_object(item);
            assert_eq!(object.object_type, UNRECOGNIZED_OBJECT_TYPE);
            assert_eq!(object.kiid, None, "{}", item.type_url);
            assert_eq!(object.kiid_evidence, IpcSelectedKiidEvidence::Unreadable);
            object
        };

        let empty = unreadable(&prost_types::Any {
            type_url: "type.googleapis.com/kiapi.schematic.types.Future".to_string(),
            value: Vec::new(),
        });
        assert_eq!(empty.protocol_type, "kiapi.schematic.types.Future");

        // A list of KIIDs is other objects, not this one.
        unreadable(&pack_any(
            &v1099::SheetPath {
                path: vec![kiid(UUID_A), kiid(UUID_B)],
                path_human_readable: "/".to_string(),
            },
            "kiapi.common.types.SheetPath",
        ));
        // A string that happens to hold a UUID is not a KIID message.
        unreadable(&pack_any(
            &v1099::SchematicField {
                name: UUID_A.to_string(),
                visible: true,
            },
            "kiapi.schematic.types.Future",
        ));
        // A KIID-shaped message whose text is not a UUID is not trusted.
        unreadable(&pack_any(
            &v1099::IdThenPosition {
                id: Some(kiid("R1")),
                position: position(),
            },
            "kiapi.schematic.types.Future",
        ));
        // Not a protobuf message at all.
        unreadable(&prost_types::Any {
            type_url: "type.googleapis.com/kiapi.schematic.types.Future".to_string(),
            value: vec![0xff; 12],
        });
    }

    #[test]
    fn the_table_names_each_protobuf_type_once() {
        let mut names = KNOWN_TYPES
            .iter()
            .map(|(name, _, _)| *name)
            .collect::<Vec<_>>();
        names.sort_unstable();
        let total = names.len();
        names.dedup();
        assert_eq!(names.len(), total);
        assert!(KNOWN_TYPES.iter().all(|(_, object_type, path)| *object_type
            != UNRECOGNIZED_OBJECT_TYPE
            && !path.is_empty()));
    }

    #[test]
    fn a_repeated_embedded_message_is_read_as_protobuf_merges_it() {
        // Two occurrences of field 1: the later KIID wins, as in a full decode.
        let mut value = Vec::new();
        for id in [UUID_B, UUID_A] {
            v1099::IdThenPosition {
                id: Some(kiid(id)),
                position: None,
            }
            .encode(&mut value)
            .unwrap();
        }
        assert_eq!(kiid_at_path(&value, &[1]).as_deref(), Some(UUID_A));
        assert_eq!(
            kiapi::schematic::types::Line::decode(value.as_slice())
                .unwrap()
                .id
                .unwrap()
                .value,
            UUID_A
        );
    }
}
