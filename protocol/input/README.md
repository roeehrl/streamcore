# Input model contract

`crates/streamcore-input` implements Pad's 1.3 input additions (Pad's input protocol v2 and its profile schema) once,
for PadMac, PadPad, PadDroid and PadWin today and StreamCore later. The fixtures
in [`fixtures/`](fixtures) pin its behaviour. The Rust test
`crates/streamcore-input/tests/conformance.rs` runs every file through the JSON method
table; the Swift, Kotlin and C# test suites run the same files through their
bindings. Each file's `description` defines its own format.
[docs/INPUT-MODEL.md](../../docs/INPUT-MODEL.md) describes the API.

## Decoding rules

| Data | Rule |
| --- | --- |
| Capability-gated messages (`keyShortcut`, `gesture` twoFinger/click, `KeyPacket`, `PencilPacket`) | Strict: unknown enum values, literal modifier bits above 15 (`platformKey` never crosses the wire), unknown named keys, phases that do not fit the kind, missing companions (click button, action), non-finite numbers and positions outside 0...1 are errors. Binary payloads must have their exact length. |
| Other JSON bodies | Unknown members ignored, missing optional members default. Notice parameters that are not strings become their JSON text, and `params` that is not an object reads as none; a `ServerState.notices` entry without a string code is skipped and notices that are not a list read as none; a snapshot entry that is not a valid profile (even a bare string) is skipped; `replaceBefore` below 0 reads as 0. A `ServerState.source` that is not an object and a snapshot whose `profiles` is not a list are errors. `wire-messages.json` pins each (`tolerant: …` cases and `invalid`). |
| Profiles | Open: unknown members at every level and unknown enum strings survive a round trip. A host refuses a profile whose schema is not 1, whose appKey is blank, whose `updatedAt` is not RFC 3339, or whose keys, modifiers (0...31) or actions are malformed. Strict validation also refuses unknown values. |
| Shape | A JSON array is never read as an object, at any depth (serde would otherwise fill a struct's members by position). Where a body or a member is an object, an array is an error; where tolerance skips malformed entries (notices, snapshot profiles), an array entry is skipped. A key repeated in one object keeps its last value, at any depth. |
| Encoding | Canonical: absent optional members stay absent; defaulted members are written, except `keyShortcut.repeat`, written only when true. Integral floats are written without a fraction. |

| Modifier bit | Pad stored | Pad literal (wire) | StreamCore |
| --- | --- | --- | --- |
| 1 | shift | shift | capsLock |
| 2 | control | control | shift |
| 4 | option | option | control |
| 8 | primary (⌘ Mac, Ctrl Windows) | ⌘ Mac, Windows key | option |
| 16 | literal ⌘ / Windows key | invalid | command |
| 32 | invalid | invalid | function |

`modifiers_to_stream_core` and `modifiers_from_stream_core` map literal Pad bits to
StreamCore bits and back (capsLock and function have no Pad bit and are dropped).

## Fixtures

| File | What it pins | API |
| --- | --- | --- |
| `wire-messages.json` | Every 1.3 JSON body: canonical re-encoding and strict rejection | `decode_*` / `encode_*`, `wire.decode` |
| `binary-payloads.json` | `KeyPacket` (4 bytes) and `PencilPacket` (1 + 39 × n bytes, cancel = 5 with penV2) | `encode/decode_key_packet`, `encode/decode_pencil_packet` |
| `modifiers.json` | Stored → literal per host, StreamCore mapping, shortcut modifiers, labels | `resolve_modifiers`, `modifiers_to/from_stream_core`, `makes_shortcuts`, `modifier_label` |
| `latch-merge.json` | Latch merging, text vs. shortcut routing, fallbacks for older hosts, LatchState sequences and release-all | `key_latch_merge`, `key_latch_route`, `route_text`, `route_key`, `LatchState` |
| `shortcut-guard.json` | Window/app-closing fallbacks (⌘W, ⌘Q, Ctrl+W, Alt+F4) and the full guard decision | `is_closing_combination`, `guard_shortcut` |
| `redo.json` | Undo and per-app redo on each host | `undo_shortcut`, `redo_shortcut` |
| `profile-roundtrip.json` | Profiles round-trip exactly; schema defaults | `decode_profile`, `encode_profile`, `profile_defaults` |
| `profile-invalid.json` | Profiles a host refuses; values only strict validation refuses | `validate_profile`, `profile_issues` |
| `preset-formats.json` | Preset files: one profile, an array, or `{"profiles": [...]}` | `parse_preset_file` |
| `preset-lookup.json` | Built-in presets (the files in `presets/builtin/`), lookup by key/alias/path, resolution, snapshot contents | `PresetLibrary`, `ProfileStore.resolve/snapshot`, `parse_preset_file` |
| `profile-merge.json` | Last writer wins, preset dates, the alias rule, delete/reveal, import | `ProfileStore` |
| `twofinger-fallback.json` | Host delivery (native, shortcuts, scroll) and fallback step arithmetic | `TwoFingerRouter`, `FallbackAccumulator` |
| `twofinger-tracker.json` | Client two-finger tracking: scale, rotation, pan, swapped touches, phases | `TwoFingerTracker` |
| `action-defaults.json` | Default actions for unset slots; Apple Pencil gestures and `pen.followSystem` | `effective_action`, `tap_action`, `finger_pen_action`, `pencil_gesture_response` |

`fixtures/presets/` holds test-only preset files used by `profile-merge.json`.
`fixtures/presets/builtin/` ships the built-in presets (one `<id>.json` per
preset) so app runners can round-trip them (`parse_preset_file(id, text)` gives
the profile `builtin_presets()` has). It is a copy, not a second source:
`crates/streamcore-input/presets` is the source, and `conformance.rs` fails unless the
two directories hold the same `.json` files byte for byte. After changing a
preset, refresh the copy with
`cp crates/streamcore-input/presets/*.json protocol/input/fixtures/presets/builtin/`
(and delete a removed preset in both).

Comparisons: JSON values with key order ignored and numbers equal within 1e-6
(`15` equals `15.0`). Where a description says "subset", only the members the
expected object lists are compared. Runners must also check that every file in
the directory is run (the Rust runner fails on an unknown file).

These fixtures merge the earlier sets (PadMac's fixtures, PadWin's
conformance tests, PadPad's sampler and Pencil tests, and the core's own); each case records its `source`
where that helps. Where the sets disagreed, Pad's input protocol v2 and the
profile schema decided; the conflicts are listed in docs/INPUT-MODEL.md.
