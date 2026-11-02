/*
 * streamcore_input.h - JSON C ABI over the StreamCore input model (crates/streamcore-input).
 *
 * Hand-written to match crates/streamcore-input-ffi/src/lib.rs; ABI version 1.
 * Used by PadWin (C# P/Invoke) until uniffi-bindgen-cs supports UniFFI 0.32,
 * when UniFFI-generated C# replaces it.
 *
 * All strings are NUL-terminated UTF-8. streamcore_input_call always returns a JSON
 * object, {"ok": <result>} or {"error": {"code": "...", "message": "..."}},
 * which the caller releases with streamcore_input_free exactly once. It returns NULL
 * only if memory allocation fails. Handles returned by "*.new" methods are
 * process-wide; release them with the "handle.release" method. Every function is
 * thread-safe. The method table is in docs/INPUT-MODEL.md. The constants and
 * vocabularies below are checked against the library by the crate's tests.
 */
#ifndef STREAMCORE_INPUT_H
#define STREAMCORE_INPUT_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define STREAMCORE_INPUT_ABI_VERSION 1

/*
 * Constants: the "constants" method returns each under its camelCase name
 * (STREAMCORE_INPUT_CAPABILITY_PEN_V2 is "capabilityPenV2"), as do the Swift and
 * Kotlin functions of that name. Capability names and notice codes are open
 * sets: peers may send others.
 */
#define STREAMCORE_INPUT_CAPABILITY_HOST_AWARE_KEYS "hostAwareKeys"
#define STREAMCORE_INPUT_CAPABILITY_TEXT_INPUT "textInput"
#define STREAMCORE_INPUT_CAPABILITY_KEY_SHORTCUT "keyShortcut"
#define STREAMCORE_INPUT_CAPABILITY_LAYOUT_INFO "layoutInfo"
#define STREAMCORE_INPUT_CAPABILITY_TWO_FINGER "twoFinger"
#define STREAMCORE_INPUT_CAPABILITY_PEN_V2 "penV2"
#define STREAMCORE_INPUT_CAPABILITY_PROFILES "profiles"
#define STREAMCORE_INPUT_CAPABILITY_NOTICE_CODES "noticeCodes"
#define STREAMCORE_INPUT_CAPABILITY_HOST_STATS "hostStats"
#define STREAMCORE_INPUT_CAPABILITY_OPEN_SETUP "openSetup"

#define STREAMCORE_INPUT_NOTICE_ACCESSIBILITY_MISSING "permission.accessibility.missing"
#define STREAMCORE_INPUT_NOTICE_SCREEN_RECORDING_MISSING "permission.screenRecording.missing"
#define STREAMCORE_INPUT_NOTICE_NEEDS_RELAUNCH "permission.needsRelaunch"
#define STREAMCORE_INPUT_NOTICE_CAPTURE_WINDOW_CLOSED "capture.windowClosed"
#define STREAMCORE_INPUT_NOTICE_CAPTURE_FELL_BACK "capture.fellBack"
#define STREAMCORE_INPUT_NOTICE_CAPTURE_FAILED "capture.failed"
#define STREAMCORE_INPUT_NOTICE_KEY_UNAVAILABLE "key.unavailable"
#define STREAMCORE_INPUT_NOTICE_GESTURE_FALLBACK "gesture.fallback"
#define STREAMCORE_INPUT_NOTICE_DISPLAY_ASLEEP "display.asleep"
#define STREAMCORE_INPUT_NOTICE_COMPUTER_LOCKED "computer.locked"
#define STREAMCORE_INPUT_NOTICE_LOCAL_NETWORK_MISSING "permission.localNetwork.missing"

/* USB HID usages (page 0x07) of the modifier keys; OPTION is Alt, COMMAND is the Windows key on a PC. */
#define STREAMCORE_INPUT_HID_LEFT_CONTROL 0xE0
#define STREAMCORE_INPUT_HID_LEFT_SHIFT 0xE1
#define STREAMCORE_INPUT_HID_LEFT_OPTION 0xE2
#define STREAMCORE_INPUT_HID_LEFT_COMMAND 0xE3
#define STREAMCORE_INPUT_HID_RIGHT_CONTROL 0xE4
#define STREAMCORE_INPUT_HID_RIGHT_SHIFT 0xE5
#define STREAMCORE_INPUT_HID_RIGHT_OPTION 0xE6
#define STREAMCORE_INPUT_HID_RIGHT_COMMAND 0xE7

#define STREAMCORE_INPUT_PRESET_UPDATED_AT "2000-01-01T00:00:00Z"
#define STREAMCORE_INPUT_MIGRATED_UPDATED_AT "2000-01-02T00:00:00Z"
#define STREAMCORE_INPUT_EVERY_OTHER_APP_KEY "*"

/*
 * Profile vocabularies (closed sets of the profile schema), as their wire names;
 * the "profile.vocabularies" method lists them in this order. Profiles keep any
 * other value unchanged; it does nothing.
 */
#define STREAMCORE_INPUT_ACTION_TYPE_SHORTCUT "shortcut"
#define STREAMCORE_INPUT_ACTION_TYPE_TEXT "text"
#define STREAMCORE_INPUT_ACTION_TYPE_CLICK "click"
#define STREAMCORE_INPUT_ACTION_TYPE_HOLD "hold"
#define STREAMCORE_INPUT_ACTION_TYPE_PAD "pad"
#define STREAMCORE_INPUT_ACTION_TYPE_NONE "none"

#define STREAMCORE_INPUT_PAD_ACTION_ERASER_TOGGLE "eraserToggle"
#define STREAMCORE_INPUT_PAD_ACTION_PREVIOUS_TOOL "previousTool"
#define STREAMCORE_INPUT_PAD_ACTION_HOVER "hover"
#define STREAMCORE_INPUT_PAD_ACTION_TOGGLE_CONTROLS "toggleControls"
#define STREAMCORE_INPUT_PAD_ACTION_KEYBOARD "keyboard"
#define STREAMCORE_INPUT_PAD_ACTION_FIT "fit"
#define STREAMCORE_INPUT_PAD_ACTION_ZOOM_VIEW_RESET "zoomViewReset"
#define STREAMCORE_INPUT_PAD_ACTION_UNDO "undo"
#define STREAMCORE_INPUT_PAD_ACTION_REDO "redo"

#define STREAMCORE_INPUT_MOUSE_BUTTON_LEFT "left"
#define STREAMCORE_INPUT_MOUSE_BUTTON_RIGHT "right"
#define STREAMCORE_INPUT_MOUSE_BUTTON_MIDDLE "middle"

#define STREAMCORE_INPUT_KEY_COLOR_NEUTRAL "neutral"
#define STREAMCORE_INPUT_KEY_COLOR_RED "red"
#define STREAMCORE_INPUT_KEY_COLOR_BLUE "blue"
#define STREAMCORE_INPUT_KEY_COLOR_GREEN "green"
#define STREAMCORE_INPUT_KEY_COLOR_AMBER "amber"
#define STREAMCORE_INPUT_KEY_COLOR_PURPLE "purple"

#define STREAMCORE_INPUT_TWO_FINGER_MODE_SEND_TO_APP "sendToApp"
#define STREAMCORE_INPUT_TWO_FINGER_MODE_ZOOM_VIEW "zoomView"

#define STREAMCORE_INPUT_GESTURE_DELIVERY_NATIVE "native"
#define STREAMCORE_INPUT_GESTURE_DELIVERY_SHORTCUTS "shortcuts"
#define STREAMCORE_INPUT_GESTURE_DELIVERY_SCROLL "scroll"

#define STREAMCORE_INPUT_PAN_DELIVERY_SCROLL "scroll"
#define STREAMCORE_INPUT_PAN_DELIVERY_MIDDLE_DRAG "middleDrag"
#define STREAMCORE_INPUT_PAN_DELIVERY_SPACE_DRAG "spaceDrag"

/* method: e.g. "profileStore.upsert"; args_json: a JSON object (NULL = {}). */
char *streamcore_input_call(const char *method, const char *args_json);

/* Releases a result of streamcore_input_call. NULL is ignored. */
void streamcore_input_free(char *result);

/* STREAMCORE_INPUT_ABI_VERSION of the loaded library. */
uint32_t streamcore_input_abi_version(void);

#ifdef __cplusplus
}
#endif


#endif /* STREAMCORE_INPUT_H */
