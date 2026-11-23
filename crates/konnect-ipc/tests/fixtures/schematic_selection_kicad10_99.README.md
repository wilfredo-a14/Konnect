# `schematic_selection_kicad10_99.ipc.bin`

KiCad 10.99's answer to `GetSelection` in the schematic editor with nine kinds of item selected.
It is the evidence that `konnect_ipc::selected` reads the protocol KiCad 10.99 really sends, whose
schematic messages differ from the vendored KiCad 10.0 `.proto` files.

Used by `mock_server_test.rs`, `kicad_10_99s_captured_schematic_selection_is_read_in_full`: the mock
server replays the capture, and every item must come back under its kind with the KIID in the table
below.

## The capture

| | |
|---|---|
| KiCad | 10.99.0-6883-gf694197ec3 (a fork of upstream master, the KiCad 11 development branch; its schematic `Serialize` code and `schematic_types.proto` are upstream's), Windows |
| Message | `kiapi.common.commands.SelectionResponse`; the file is its `Any.value` |
| Transport | `GetSelection` over the running schematic editor's IPC socket, after Konnect's `mutate_editor_selection` had added the items |
| Size | 2931 bytes |

The response was parsed and written back by protobuf's Python runtime, loaded with a descriptor set
compiled from that build's own `api/proto`. The items are `google.protobuf.Any` values, which that
round trip keeps byte for byte.

## What was selected

A small schematic (a connector, an LED and a resistor with their wires and labels) to which a T
junction, a no-connect, a global and a hierarchical label, a text and a hierarchical sheet were
added. In the order KiCad returned them:

| Message (`kiapi.schematic.types.`) | Item | KIID | Bytes |
|---|---|---|---|
| `LocalLabel` | label `LED_A` | `0cc999dc-a82a-492e-b7dc-f2ddfc0c4446` | 110 |
| `SchematicText` | text `selection test` | `724e8a92-9414-4c2d-a4db-0a05f803aa76` | 105 |
| `SchematicSymbolInstance` | symbol `R1` (`Device:R`), with its library definition | `f4a4c65b-674b-4d0f-a935-3abb456ee78d` | 1361 |
| `NoConnectMarker` | no-connect | `b93eff96-b310-4077-a407-0326f43c6d37` | 60 |
| `Junction` | junction | `bd264233-12da-4bff-b641-56d25eeb10aa` | 56 |
| `GlobalLabel` | global label `GL_TEST` | `16dbdde3-7721-4e11-ad75-ff8819f36707` | 201 |
| `HierarchicalLabel` | hierarchical label `HL_TEST` | `038860db-fcfb-42e7-9554-36fe52fb6779` | 114 |
| `SheetSymbol` | sheet `child` | `871ff605-89eb-4a58-9a5b-f6e44d75f9e2` | 287 |
| `SchematicLine` | wire | `08527288-3601-4b95-b2e8-c2d72ccef881` | 74 |

The KIIDs in the table are the ones KiCad's own message classes read from each item's `id` field
(field 1 in all nine), and the same ones the schematic file holds for those items.

## Regenerating it

Any schematic with these item kinds will do; the test's expected KIIDs then change with it. Open it
in a KiCad built from upstream master with the API enabled, select the items, send `GetSelection`
for the open sheet, and save the `SelectionResponse` body.
