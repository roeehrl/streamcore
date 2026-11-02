# Input model (`streamcore-input`)

`crates/streamcore-input` is the platform-independent input logic shared by Pad's four
apps (PadMac and PadPad in Swift, PadDroid in Kotlin, PadWin in C#) and, after
MyCNC launches, by StreamCore. It implements Pad's 1.3 input additions (Pad's
input protocol v2) once; the apps keep only what touches their OS. The
behaviour is pinned by [protocol/input](../protocol/input/README.md) fixtures that
every language runs.

## What the crate owns, what stays native

| The crate decides | Native code does |
| --- | --- |
| JSON bodies of `text`, `keyShortcut`, `gesture` (incl. `twoFinger`, `click`), `layoutInfo`, `hostStats`, `frontApp`/`appList` (`appKey`), notices, `profilesSnapshot`/`profileUpsert`/`profileDelete`/`profileChanged`; strict vs tolerant decoding | Framing, sockets, handshake, capability exchange |
| `KeyPacket` and `PencilPacket` byte layouts (cancel = 5 with `penV2`) | Reading touches, pencil and hardware keys |
| Modifier resolution (stored primary → ⌘ / Ctrl, platform key → ⌘ / Win), the StreamCore bit mapping, labels | Drawing the modifier bar and key labels |
| Latches and holds, text vs. per-character shortcuts, fallbacks for hosts without `textInput`/`keyShortcut`, key-ups to release on disconnect | Collecting committed text from the input method; sending |
| Default actions for unset slots, Apple Pencil double-tap/squeeze vs `pen.followSystem` | Recognizing taps, chords and Pencil gestures |
| Profile format, validation, built-in presets, merge rules (last writer wins, preset dates, alias rule), resolution per app, snapshot contents, client mirror | `profiles.json` file IO, iCloud/migration sources, editors |
| Undo/redo per front app, the closing-combination guard and its `key.unavailable` notice | Keyboard-layout lookups (`UCKeyTranslate`, `VkKeyScanExW`) and event injection |
| Two-finger tracking (scale, rotation, pan, phases), host delivery (native magnify/rotate brackets, fallback shortcut steps, 1.2 zoom-scroll), fallback step arithmetic | Posting trackpad gestures, touch contacts, scroll and keys |

Objects hold one short internal lock, return copies and never call back into
native code, as MyCNC's `HostRegistry` does.

Every JSON input (the typed `decode_*` functions, `decode_profile`, preset files,
`ProfileStore.load_json` and every argument of the JSON method table) is read into
a `serde_json::Value` and decoded through one deserializer, `serde_util::strict`,
which refuses a JSON array wherever the type expects an object, at any depth.
serde's derived structs would otherwise accept an array and fill the members by
position, which no other Pad implementation does.

A key repeated in one JSON object keeps its last value, at any depth. This is the
rule, not an accident of the parser: every decode on both ends of a connection
goes through this one core parser, so two parsers can never disagree about a
repeated key, and refusing repeats would fail snapshots that are valid today.
`wire-messages.json` pins it (`duplicate key: …` cases). The internally tagged
output enums (`OutgoingInput`, `RoutedInput`, `PencilResponse`) have no
`Deserialize` implementation, since serde decodes such enums past `strict`; a
test fails if one gains it.

## API

The same names in every language (UniFFI converts `snake_case` to `camelCase`).

**Objects**

| Object | Methods |
| --- | --- |
| `ProfileStore` (`new()`, `with_presets(PresetLibrary)`) | `load_json`, `save_json`, `revision`, `user_profiles`, `presets`, `snapshot`, `resolve(app_key)`, `current(app_key)`, `upsert(profile) -> UpsertOutcome{result, changed, revision, message, reason}`, `delete(app_key) -> DeleteOutcome{removed, revision, message, revealed_preset, send_snapshot}`, `import_if_missing(profiles)`, `apply_snapshot`, `apply_changed` (client mirror) |
| `PresetLibrary` (`builtin()`, `from_profiles`) | `presets`, `failures`, `preset_for(app_key)`, `every_other_app`, `preset_with_id` |
| `LatchState` | `toggle`, `set_latched`, `set_one_shot`, `hold`, `release`, `latched`, `held`, `active`, `merge(requested, host)`, `route_text`, `route_key`, `key_packet` (hardware keyboard), `release_all -> ReleaseAll` |
| `TwoFingerTracker` (client, PadPad's `TwoFingerSampler`) | `touches(x1, y1, x2, y2, width, height) -> GestureMessage?` (positions normalized to the content rect, its size in unzoomed view points; swapped touches matched back), `end`, `cancel` (bare `ended`/`cancelled`), `is_active` |
| `TwoFingerRouter` (host) | `handle(message, delivery, pan, fallbacks, pan_scale, host) -> [TwoFingerOutput]` (`pan` is `gestures.pan`: the pan as a scroll, or as a middle-button or Space drag, `PanDrag`), `finish(phase)` |
| `FallbackAccumulator` (host) | `feed(message, fallbacks, host) -> FallbackOutput`, `feed_deltas(scale, rotation, zoom_step, rotate_step_degrees) -> FallbackCounts`, `reset` |
| `JsonApi` | `call(method, args_json) -> json` (the method table below) |

**Functions:** `effective_action(profile, slot)`, `default_action(slot)`,
`tap_action(profile, fingers)`, `finger_pen_action(profile, fingers)`,
`follows_system`, `pencil_gesture_response(profile, gesture, system)` (client
actions and their defaults: tap2/3/4 undo, redo, toggle controls; fingerPen1/2/3
eraser, hover, right-button stroke; doubleTap eraser toggle; squeeze, lowerButton
right click; upperButton middle click; eraserEnd eraser), `encode_*`/`decode_*` for every message (`decode_key_shortcut` and
`decode_gesture` are strict; `validate_key_shortcut`, `validate_gesture`),
`encode/decode_key_packet`, `encode/decode_pencil_packet(bytes, pen_v2)`,
`decode_profile`, `encode_profile`, `validate_profile(profile, strict)`,
`profile_issues`, `profile_defaults`, `normalize_app_key`, `profiles_overlap`,
`parse_preset_file`, `builtin_presets`, `resolve_modifiers`, `makes_shortcuts`,
`modifiers_to_stream_core`, `modifiers_from_stream_core`, `modifier_label`,
`host_os_from_name`, `key_latch_merge`, `key_latch_route` (PadMac's
`KeyLatch.route`), `route_text`, `route_key`, `host_capabilities(names)`,
`undo_shortcut`, `redo_shortcut`, `resolve_shortcut`, `shortcut_for_host`,
`shortcut_with_host_edit`, `shortcut_has_host_overrides`, `is_closing_combination`,
`guard_shortcut`, `named_key_*` (incl. `named_key_for_hid`), `us_key_for_char`,
`us_key_for_hid`, `hardware_key_for_host`, `is_modifier_hid`, `capability_names`,
`notice_codes`, `pad_message_type_code`, `is_profiles_snapshot_request`,
`profile_timestamp_now`, `compare_timestamps`, the vocabulary and constant
functions below. Errors are one `StreamInputError`
(`InvalidJson`, `InvalidMessage`, `InvalidBinary`, `InvalidProfile`, and for the
JSON table `UnknownMethod`, `UnknownHandle`, `InvalidArguments`), each with a
`detail` string (not `message`, which Kotlin exceptions reserve).

The names follow PadMac's boundary (`Shared/CoreBridge`): `KeyLatch.merge/route`
→ `key_latch_merge/route`, `ProfileMerge.upsert/delete/current` and
`ProfileResolver.profile/effectiveProfiles` → `ProfileStore.upsert/delete/current/
resolve/snapshot`, `PresetLibrary.preset(for:)/everyOtherApp/preset(id:)` →
`preset_for/every_other_app/preset_with_id`, `TwoFingerRouter.handle/finish`,
`ShortcutGuard.isClosingCombination` → `is_closing_combination`,
`PadCapability`/`NoticeCode` → `capability_names`/`notice_codes` and the Rust
constants. PadWin's `ProfileSet.Upsert/Delete/ForApp/Snapshot`,
`PresetFormat.Parse`, `ModifierBits.FromProfile` and `TwoFingerSteps.Feed/Reset`
map to `profileStore.upsert/delete/resolve/snapshot`, `presets.parseFile`,
`modifiers.resolve` (host windows) and `fallbackAccumulator.feedDeltas/reset`.

### Vocabularies and constants

The profile schema's closed sets are enums, so Swift and Kotlin switch over them
exhaustively. Profiles stay open: `ProfileAction.action_type`, `.button`, `.pad`,
`QuickKey.color`, `ProfileGestures.two_finger`, `.delivery` and `.pan` remain the strings
that were stored or received, so a value from a newer version survives a round
trip and does nothing. The enums are a typed view of those strings:
`<vocabulary>_from_name(name)` returns `None`/`nil`/`null` for an unknown value
and `<vocabulary>_name(value)` gives the wire name back. In the C ABI the values
are their wire names; `profile.vocabularies` lists them and `streamcore_input.h`
defines each as `STREAMCORE_INPUT_<VOCABULARY>_<VALUE>`.

| Rust enum (wire names) | Swift | Kotlin | Parse / name functions |
| --- | --- | --- | --- |
| `ActionType` (shortcut, text, click, hold, pad, none) | `.shortcut` … `.noAction` | `SHORTCUT` … `NO_ACTION` | `action_type_from_name`, `action_type_name` |
| `PadAction` (eraserToggle … redo) | `.eraserToggle` … | `ERASER_TOGGLE` … | `pad_action_from_name`, `pad_action_name` |
| `MouseButton` (left, right, middle; also the `click` gesture's button) | `.left` … | `LEFT` … | `mouse_button_from_name`, `mouse_button_name` |
| `KeyColor` (neutral, red, blue, green, amber, purple) | `.neutral` … | `NEUTRAL` … | `key_color_from_name`, `key_color_name` |
| `TwoFingerMode` (sendToApp, zoomView) | `.sendToApp`, `.zoomView` | `SEND_TO_APP`, `ZOOM_VIEW` | `two_finger_mode_from_name`, `two_finger_mode_name` |
| `GestureDelivery` (native, shortcuts, scroll) | `.native` … | `NATIVE` … | `gesture_delivery_from_name`, `gesture_delivery_name` |
| `PanDelivery` (scroll, middleDrag, spaceDrag) | `.scroll` … | `SCROLL` … | `pan_delivery_from_name`, `pan_delivery_name` |

`ActionType`'s "none" is `NoAction` (Swift `.noAction`): a Swift case named
`none` would be confused with an absent optional (`actionTypeFromName(x) ==
.none` would test for `nil`).

Open sets stay strings and get one exported constant per known value. UniFFI 0.32
has no exported constants, so each is a function without arguments; the C ABI's
`constants` method returns all of them under the same camelCase names, and the
header defines each as `STREAMCORE_INPUT_<NAME>`. The existing lists
(`capability_names`, `notice_codes`, `wire.capabilities`, `wire.noticeCodes`) are
unchanged.

| Rust function (value) | Swift / Kotlin | C ABI |
| --- | --- | --- |
| `capability_host_aware_keys` … `capability_host_stats` (the nine 1.3 capability names) | `capabilityHostAwareKeys()` … | `constants.capabilityHostAwareKeys`, `STREAMCORE_INPUT_CAPABILITY_HOST_AWARE_KEYS` … |
| `notice_accessibility_missing` … `notice_computer_locked` (the ten notice codes) | `noticeAccessibilityMissing()` … | `constants.noticeAccessibilityMissing`, `STREAMCORE_INPUT_NOTICE_ACCESSIBILITY_MISSING` … |
| `hid_left_control`, `hid_left_shift`, `hid_left_option`, `hid_left_command`, `hid_right_control` … `hid_right_command` (0xE0…0xE7; option = Alt, command = ⌘ / the Windows key) | `hidLeftControl()` … | `constants.hidLeftControl`, `STREAMCORE_INPUT_HID_LEFT_CONTROL` … |
| `preset_updated_at` ("2000-01-01T00:00:00Z") | `presetUpdatedAt()` | `constants.presetUpdatedAt`, `STREAMCORE_INPUT_PRESET_UPDATED_AT` |
| `migrated_updated_at` ("2000-01-02T00:00:00Z") | `migratedUpdatedAt()` | `constants.migratedUpdatedAt`, `STREAMCORE_INPUT_MIGRATED_UPDATED_AT` |
| `every_other_app_key` ("*") | `everyOtherAppKey()` | `constants.everyOtherAppKey`, `STREAMCORE_INPUT_EVERY_OTHER_APP_KEY` |

`streamcore-input-ffi`'s tests fail if the header's definitions and the library's
lists differ in any name, value or order.

### Hardware keys (stateless)

`LatchState.key_packet` merges latched and held modifiers into a hardware key
and then applies `hardware_key_for_host`; apps that keep their own state can call
the same rule directly.

| Rust | Swift / Kotlin | C ABI | Does |
| --- | --- | --- | --- |
| `hardware_key_for_host(hid, stored, host) -> HostKey{hid_usage, modifiers}` | `hardwareKeyForHost(hid:stored:host:)` | `keys.hardwareKeyForHost {hid, modifiers, host}` → `{hidUsage, modifiers}` | A tablet keyboard's key (Mac convention, ⌘ = `command`) as the host receives it: modifiers resolved for the host; on Windows ⌘ is Ctrl, the ⌘ keys (0xE3/0xE7) included |
| `us_key_for_hid(hid, shift) -> String?` | `usKeyForHid(hid:shift:)` | `keys.usKeyForHid {hid, shift}` | The printing character (U+0020…U+007E) the US key types, including " " for Space (whose named key is `space`); `nil` for Return, Tab, the other named keys and modifier keys. Reverse of `us_key_for_char` |
| `named_key_for_hid(hid) -> NamedKey?` | `namedKeyForHid(hid:)` | `keys.namedKeyForHid {hid}` → wire name | Reverse of `named_key_hid` |
| `is_modifier_hid(hid)` | `isModifierHid(hid:)` | `keys.isModifierHid {hid}` | One of the eight modifier keys |

### JSON method table (C ABI)

`streamcore-input-ffi` exports `streamcore_input_call(method, args_json)`,
`streamcore_input_free(result)` and `streamcore_input_abi_version()`
([header](../crates/streamcore-input-ffi/include/streamcore_input.h)). Every call returns
`{"ok": result}` or `{"error": {"code", "message"}}`; `*.new` returns
`{"handle": n}`, released with `handle.release`. Arguments are camelCase JSON
named as in the typed API; `host` is `"mac"` or `"windows"`; message bodies may be
given as JSON or as text.

| Namespace | Methods |
| --- | --- |
| (none) | `version`, `constants` (every constant above, camelCase keys) |
| `wire` | `decode`, `encode` (`{type, json}`), `messageTypes`, `capabilities`, `noticeCodes`, `isSnapshotRequest` |
| `keyPacket`, `pencil` | `encode`, `decode` (hex) |
| `modifiers` | `resolve`, `makesShortcuts`, `toStreamCore`, `fromStreamCore`, `label` |
| `keys` | `names`, `namedHid`, `usKey`, `validateShortcut`, `hardwareKeyForHost`, `usKeyForHid`, `namedKeyForHid`, `isModifierHid` |
| `latch`, `typing` | `latch.merge`, `latch.route`, `typing.routeText`, `typing.routeKey` |
| `shortcuts` | `undo`, `redo`, `resolve`, `forHost`, `withHostEdit`, `hasHostOverrides`, `isClosing`, `guard` |
| `actions` | `default`, `effective`, `tap`, `fingerPen`, `followsSystem`, `pencil` |
| `profile`, `presets`, `time` | `profile.decode/validate/defaults/isNewer/appliesTo/normalizeAppKey/vocabularies`, `presets.parseFile/builtin/lookup/new`, `time.now/compare` |
| objects | `profileStore.*`, `latchState.*`, `twoFingerTracker.*`, `twoFingerRouter.*`, `fallbackAccumulator.*` (the object methods above, camelCase), `handle.release` |

The C ABI is the only `unsafe` in the workspace (three functions, documented
`SAFETY` blocks, fuzzed with malformed input). It is replaced by
UniFFI-generated C# when uniffi-bindgen-cs supports UniFFI 0.32 (today it targets
0.31); the method table stays for fixture runners.

## Builds

| Platform | Script | Output |
| --- | --- | --- |
| Apple | `scripts/build-streamcore-input-apple.sh` | `StreamCoreInput/Artifacts/StreamCoreInputFFI.xcframework` (macOS arm64+x86_64, iOS arm64, Simulator arm64+x86_64; macOS 15 / iOS 18), Swift bindings in `StreamCoreInput/Sources/StreamCoreInput/Generated`; runs the smoke program and Swift tests, compiles for iOS |
| Android | `scripts/build-streamcore-input-android.sh [abi…]` | `target/streamcore-input-android/jniLibs/{arm64-v8a,armeabi-v7a,x86_64}/libstreamcore_input.so` (API 28, 16 KB pages), `target/streamcore-input-android/kotlin/uniffi/streamcore_input/streamcore_input.kt` |
| Windows | `scripts/build-streamcore-input-windows.sh` | `target/streamcore-input-windows/{x64,arm64}/streamcore_input_ffi.dll` (+ `.dll.lib`, `.pdb`) and `streamcore_input.h`; built and tested natively on the Windows PC, cargo-xwin cross build as fallback |

StreamCore's local CI ([`ci/local/jobs.json`](../ci/local/jobs.json)) formats, lints and
tests both crates with the rest of the workspace, and runs the Apple and Android
scripts (`local/streamcore-input-apple`, `local/streamcore-input-android`). The
Windows script needs a Windows PC named by `STREAMCORE_WINDOWS_HOST` (or
`STREAMCORE_INPUT_WINDOWS_CROSS=1` for the cross build) and is run by hand.

## Wiring into Pad

- **PadMac / PadPad (XcodeGen):** add the package, `packages: StreamCoreInput: {path:
  ../streamcore/StreamCoreInput}` (or a copy of `StreamCoreInput/` with its `Artifacts` and
  `Generated`), and `dependencies: [{package: StreamCoreInput}]` on the Mac and iPad
  targets; `import StreamCoreInput`. Replace `Shared/CoreBridge/*` with the generated
  types, and Pad's built-in presets with `PresetLibrary.builtin()`.
- **PadDroid:** copy `jniLibs/*` to `android/app/src/main/jniLibs/` and
  `kotlin/uniffi/` to `android/app/src/main/kotlin/`; add
  `implementation("net.java.dev.jna:jna:5.19.1@aar")` (Pad uses 5.19.1, whose `libjnidispatch.so` is 16 KB aligned; 5.12 or newer works: UniFFI's Kotlin
  loads `libstreamcore_input.so` through JNA and uses its `Cleaner`) and keep `com.sun.jna` in R8
  (`-keep class com.sun.jna.** { *; }`, `-keep class uniffi.** { *; }`).
- **PadWin:** copy `x64/streamcore_input_ffi.dll` (and `arm64/` for an ARM64 build)
  next to `PadWin.exe` (`<None Include="..\native\x64\streamcore_input_ffi.dll"
  CopyToOutputDirectory="PreserveNewest" />`) and declare
  `[DllImport("streamcore_input_ffi", CallingConvention = CallingConvention.Cdecl)]
  static extern IntPtr streamcore_input_call(byte[] method, byte[] args);` (NUL-terminated
  UTF-8), `streamcore_input_free(IntPtr)` and `streamcore_input_abi_version()`; read the
  result with `Marshal.PtrToStringUTF8` before freeing it. `CoreBridge/*.cs` then
  becomes thin wrappers over `streamcore_input_call`.
- **Fixtures:** each app's test suite runs `protocol/input/fixtures/*.json`
  through its binding (Swift: typed calls or `JsonApi`; C#: `streamcore_input_call`);
  `StreamCoreInput/Tests` and `crates/streamcore-input/tests/conformance.rs` are references.
  `fixtures/presets/builtin/` carries the built-in presets for round-tripping;
  it is a checked copy of `crates/streamcore-input/presets` (see
  [protocol/input](../protocol/input/README.md)).

## Relation to MyCNC's input lane

MyCNC's current input lane ([BINARY.md](../protocol/BINARY.md), input payloads
`0x20`) carries fixed-size pointer, scroll, magnify and key-position events with
StreamCore modifier bits, plus committed `insertText`; the native injector owns
leases and posting ([INPUT-OWNERSHIP.md](INPUT-OWNERSHIP.md)). This crate does not
change that lane. It provides what StreamCore will need to carry Pad's input:

| Pad (this crate) | StreamCore today |
| --- | --- |
| `text` (committed only) | `insertText` (≤ 4096 UTF-16 units, split natively) |
| `KeyPacket` (HID position) | key payload (key code, down, modifiers) |
| `keyShortcut` (character resolved on the host's layout) | none yet; StreamCore expects to carry both |
| `twoFinger` began/changed/ended/cancelled | magnify phases began/changed/ended+cancelled |
| Pad modifier bits | `modifiers_to_stream_core` / `modifiers_from_stream_core` |
| Strict decoding of capability-gated values | StreamCore's rule: unknown values and bits rejected |

## Decisions where the earlier sets disagreed

The fixtures merge PadMac's fixtures, PadWin's conformance tests
and the core's own cases; Pad's input protocol v2 and the profile schema decided:

1. A user profile hides a preset only for the keys it covers (the alias rule); the
   preset stays in the snapshot. PadMac's and PadWin's sets hid the whole preset.
2. When nothing is stored for an app, an upsert must be newer than its preset
   (PadMac); PadWin stored any first profile. PadWin's sample preset now carries
   the preset date 2000-01-01, so its import case still passes.
3. A profile whose `updatedAt` is not RFC 3339 is refused (PadWin, schema
   `date-time`); PadMac stored it.
4. `keyShortcut.repeat` is written only when true (PadMac, and the spec's
   example); PadWin wrote `false`. Notice `params` is always written, `{}` when
   empty (PadMac); PadWin left it out.
5. Fallback zoom steps add up in log space with at most 3 presses per axis per
   message (PadWin); PadMac added linearly with up to 8. Scale is the relative
   change per message, so log space makes a spread and the same pinch back cancel.
6. `twoFinger` positions are optional (a `cancelled` may carry none) but must lie
   in 0...1 when present; `replaceBefore` below 0 reads as 0 (PadMac).
7. The `"*"` profile's `fallbacks.redo` is ignored on Windows: apps without their
   own profile use PadWin's per-app Ctrl+Y / Ctrl+Shift+Z list.
8. Default actions (integrating owner's list): tap4 is toggle controls (PadPad and
   PadDroid had none); fingerPen1/2/3 are eraser, hover and a right-button stroke
   (PadPad 1.2; PadDroid had none); pen buttons follow PadDroid.
9. The tracker follows PadPad's sampler: swapped touches are matched back, below
   1 point of spacing scale and rotation are 0, and `ended`/`cancelled` carry
   only kind and phase.

## Tablet setup controls and edit timestamps

`openSetup` (Pad type `0x54`) is an empty JSON object from an authenticated tablet.
The host advertises the `openSetup` capability before a client may send it, then
opens its own setup checklist on its UI thread. The body cannot name a path,
settings pane, process or OS command. Existing message numbers are unchanged.
`permission.localNetwork.missing` is a standing notice for a computer that can
serve USB while macOS blocks local-network discovery.

`profile_timestamp_after(previous, now)` returns a valid UTC timestamp at least
one whole millisecond after `previous` when clock resolution or clock skew would
otherwise cause the profile store to reject an edit. `now` is optional; omitting
it uses the core's clock. Invalid explicit clock values and a timestamp beyond
the RFC 3339 year range are errors. The C ABI exposes this as `time.after`.
Native apps should use this helper when stamping an edit, rather than duplicate
date arithmetic. Ordered `*_names` functions also expose the seven profile option
vocabularies to Swift and Kotlin.

## Per-host shortcuts and the pan as a drag

A stored `Shortcut` may carry `hosts: {mac?, windows?}`, each a `{key, modifiers}`
in the profile convention, for apps whose shortcut differs between the Mac and
Windows by more than ⌘ → Ctrl (Affinity zooms in with ⌘+ on a Mac and Ctrl+= on
Windows). `key` and `modifiers` stay the shortcut for a host without its own entry,
and are what versions before `hosts` use; they keep `hosts` as an unknown member,
so it survives their edits. `shortcut_for_host(shortcut, host)` is the shortcut a
host uses (no `hosts`); `resolve_shortcut`, and with it the fallbacks, the router
and `redo_shortcut`, apply it. A client sending a Quick Key, pen or gesture
shortcut calls it with the connected host's OS before routing the key. An editor
shows `shortcut_for_host` for the connected host and stores an edit with
`shortcut_with_host_edit(original, edited, host)`, which changes that host's own
entry when it has one and the shared key otherwise, keeping the other host's
entry. Validation checks each entry's key and modifiers like the shared ones.

`gestures.pan` (`PanDelivery`: scroll, middleDrag, spaceDrag; default scroll) says
how the router delivers the two-finger pan. `scroll` is a phased trackpad scroll
on a Mac and the wheel on Windows. Some apps zoom on the wheel (Krita's default
canvas profile), so `middleDrag` and `spaceDrag` deliver the pan as a drag:
`PanDrag{via, dx, dy, phase}` in screen points, `began` at the centroid once the
pan adds up to `PAN_DRAG_START_POINTS` (4) so a pinch's wobble does not press a
button, `changed` moves, `ended`/`cancelled` release. The drag is released before
any zoom or rotate shortcut or modifier+scroll the same message produces (Krita
reads Ctrl or Shift with the middle button or Space as zoom or rotate) and starts
again with the next pan. On Windows, native delivery is real touch and never
reaches the router, so `pan` applies to the shortcuts and scroll deliveries there.

