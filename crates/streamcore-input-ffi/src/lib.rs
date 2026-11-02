//! A JSON C ABI over `streamcore-input`, for PadWin's C# (P/Invoke).
//!
//! uniffi-bindgen-cs generates C# only for UniFFI 0.31; the core pins 0.32. Until
//! it supports 0.32, C# calls the same logic through three functions:
//!
//! ```c
//! char *streamcore_input_call(const char *method, const char *args_json);
//! void streamcore_input_free(char *result);
//! uint32_t streamcore_input_abi_version(void);
//! ```
//!
//! `streamcore_input_call` takes a method name ("profileStore.upsert") and a JSON
//! object of arguments, both NUL-terminated UTF-8, and always returns a
//! NUL-terminated UTF-8 JSON object the caller must free with `streamcore_input_free`:
//! `{"ok": <result>}` or `{"error": {"code": "...", "message": "..."}}`. It never
//! unwinds into the caller. Objects (`profileStore.new` and the like) return a
//! `handle`, shared by the whole process; release them with `handle.release`.
//! The method table is the one in `streamcore_input::json_api` (docs/INPUT-MODEL.md).
//!
//! This crate is replaced by UniFFI-generated C# once uniffi-bindgen-cs supports
//! UniFFI 0.32; the JSON method table stays for fixture runners.

use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::LazyLock;

use serde_json::{Value, json};
use streamcore_input::JsonApi;

/// Version of this C ABI (the three functions and the result envelope).
pub const ABI_VERSION: u32 = 1;

static API: LazyLock<JsonApi> = LazyLock::new(JsonApi::create);

fn error(code: &str, message: &str) -> String {
    json!({"error": {"code": code, "message": message}}).to_string()
}

/// The envelope for one call, as text. Never panics outward.
pub fn call_json(method: Option<&[u8]>, args: Option<&[u8]>) -> String {
    call_json_with(&API, method, args)
}

/// [`call_json`] on a given method table (tests use their own, so their
/// handles cannot collide with the process-wide one).
fn call_json_with(api: &JsonApi, method: Option<&[u8]>, args: Option<&[u8]>) -> String {
    let Some(method) = method else {
        return error("invalidArguments", "method is NULL");
    };
    let Ok(method) = std::str::from_utf8(method) else {
        return error("invalidArguments", "method is not UTF-8");
    };
    let args = match args.map(std::str::from_utf8) {
        None => "",
        Some(Ok(text)) => text,
        Some(Err(_)) => return error("invalidJson", "arguments are not UTF-8"),
    };
    match catch_unwind(AssertUnwindSafe(|| api.invoke(method, args))) {
        Ok(Ok(result)) => {
            let value: Value = serde_json::from_str(&result).unwrap_or(Value::Null);
            json!({ "ok": value }).to_string()
        }
        Ok(Err(e)) => error(e.code(), &e.to_string()),
        Err(_) => error("internal", "the call panicked"),
    }
}

fn into_c(text: String) -> *mut c_char {
    // JSON text escapes U+0000, so it never contains an interior NUL.
    CString::new(text)
        .or_else(|_| CString::new(error("internal", "result contained NUL")))
        .map_or(std::ptr::null_mut(), CString::into_raw)
}

/// Calls a method of the input model. See the crate documentation.
///
/// # Safety
///
/// `method` and `args_json` must each be NULL or point to a NUL-terminated
/// string that stays valid and unchanged for the duration of the call. The
/// result must be released with [`streamcore_input_free`] exactly once.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn streamcore_input_call(
    method: *const c_char,
    args_json: *const c_char,
) -> *mut c_char {
    let read = |ptr: *const c_char| {
        if ptr.is_null() {
            None
        } else {
            // SAFETY: the caller guarantees a non-NULL pointer is a valid,
            // NUL-terminated string for the duration of this call; the borrow
            // does not outlive the call.
            Some(unsafe { CStr::from_ptr(ptr) }.to_bytes())
        }
    };
    into_c(call_json(read(method), read(args_json)))
}

/// Releases a result of [`streamcore_input_call`]. NULL is ignored.
///
/// # Safety
///
/// `result` must be NULL or a pointer returned by [`streamcore_input_call`] that has
/// not been released yet.
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn streamcore_input_free(result: *mut c_char) {
    if !result.is_null() {
        // SAFETY: the caller guarantees `result` came from `CString::into_raw` in
        // `streamcore_input_call` and is released only once.
        drop(unsafe { CString::from_raw(result) });
    }
}

/// The C ABI version ([`ABI_VERSION`]).
#[allow(unsafe_code)]
#[unsafe(no_mangle)]
pub extern "C" fn streamcore_input_abi_version() -> u32 {
    ABI_VERSION
}

#[cfg(test)]
#[allow(unsafe_code)]
mod tests {
    use super::*;

    fn call(method: &str, args: &str) -> Value {
        let m = CString::new(method).unwrap();
        let a = CString::new(args).unwrap();
        // SAFETY: both pointers are valid NUL-terminated strings for the call,
        // and the result is freed exactly once below.
        let result = unsafe { streamcore_input_call(m.as_ptr(), a.as_ptr()) };
        assert!(!result.is_null());
        // SAFETY: `result` is a live NUL-terminated string from the call.
        let text = unsafe { CStr::from_ptr(result) }
            .to_str()
            .unwrap()
            .to_owned();
        // SAFETY: released once.
        unsafe { streamcore_input_free(result) };
        serde_json::from_str(&text).unwrap()
    }

    fn raw(method: *const c_char, args: *const c_char) -> String {
        // SAFETY: callers pass NULL or valid NUL-terminated strings.
        let result = unsafe { streamcore_input_call(method, args) };
        // SAFETY: `result` is a live NUL-terminated string from the call.
        let text = unsafe { CStr::from_ptr(result) }
            .to_str()
            .unwrap()
            .to_owned();
        // SAFETY: released once.
        unsafe { streamcore_input_free(result) };
        text
    }

    #[test]
    fn calls_through_the_c_abi() {
        assert_eq!(streamcore_input_abi_version(), 1);
        assert_eq!(
            call("modifiers.resolve", r#"{"stored": 8, "host": "windows"}"#),
            json!({"ok": 2})
        );
        let handle = call("profileStore.new", "{}")["ok"]["handle"].clone();
        let upsert = call(
            "profileStore.upsert",
            &json!({"handle": handle, "profile": {"schema": 1, "appKey": "krita.exe", "name": "Mine", "updatedAt": "2026-10-10T10:00:00Z", "groups": []}}).to_string(),
        );
        assert_eq!(upsert["ok"]["result"], "accepted");
        assert_eq!(
            call(
                "profileStore.resolve",
                &json!({"handle": handle, "appKey": "org.krita"}).to_string()
            )["ok"]["preset"],
            "krita"
        );
        assert_eq!(
            call("handle.release", &json!({"handle": handle}).to_string()),
            json!({"ok": true})
        );
        assert_eq!(
            call(
                "profileStore.revision",
                &json!({"handle": handle}).to_string()
            )["error"]["code"],
            "unknownHandle"
        );
    }

    #[test]
    fn null_and_bad_text_are_errors_not_crashes() {
        assert!(raw(std::ptr::null(), std::ptr::null()).contains("invalidArguments"));
        // SAFETY: NULL is ignored.
        unsafe { streamcore_input_free(std::ptr::null_mut()) };
        let m = CString::new("version").unwrap();
        assert!(raw(m.as_ptr(), std::ptr::null()).starts_with("{\"ok\""));
        let bad = [0xffu8, 0xfe, 0];
        assert!(raw(bad.as_ptr().cast(), std::ptr::null()).contains("not UTF-8"));
        assert!(raw(m.as_ptr(), bad.as_ptr().cast()).contains("invalidJson"));
        assert_eq!(
            call("version", "[1,2]")["error"]["code"],
            "invalidArguments"
        );
        assert_eq!(call("nope", "{}")["error"]["code"], "unknownMethod");
        assert_eq!(
            call(
                "wire.decode",
                r#"{"type":"keyShortcut","json":"{\"key\":{}}"}"#
            )["error"]["code"],
            "invalidMessage"
        );
    }

    /// Deterministic fuzzing: random bytes, truncated and mutated valid calls,
    /// and every listed method with garbage arguments must all produce an
    /// envelope, never a panic.
    #[test]
    fn malformed_input_always_yields_an_envelope() {
        // Its own table: the fuzzed `handle.release` calls must not release
        // handles other tests hold in the process-wide one.
        let api = JsonApi::create();
        let mut seed: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let methods = [
            "version",
            "constants",
            "profile.vocabularies",
            "keys.hardwareKeyForHost",
            "keys.usKeyForHid",
            "keys.namedKeyForHid",
            "wire.decode",
            "wire.encode",
            "keyPacket.decode",
            "pencil.decode",
            "modifiers.resolve",
            "modifiers.toStreamCore",
            "typing.routeText",
            "typing.routeKey",
            "latch.route",
            "shortcuts.guard",
            "shortcuts.redo",
            "profile.validate",
            "presets.parseFile",
            "profileStore.new",
            "profileStore.upsert",
            "profileStore.loadJson",
            "latchState.new",
            "latchState.routeText",
            "twoFingerTracker.new",
            "twoFingerTracker.touches",
            "fallbackAccumulator.feed",
            "twoFingerRouter.handle",
            "handle.release",
        ];
        let seeds = [
            r#"{"type":"gesture","json":{"kind":"twoFinger","phase":"began","x":0.5,"y":0.5}}"#,
            r#"{"handle":1,"profile":{"schema":1,"appKey":"a","name":"A","updatedAt":"2026-01-01T00:00:00Z","groups":[]}}"#,
            r#"{"text":"zS\n","latched":8,"host":"mac","hex":"011d0008"}"#,
            r#"{"handle":2,"contacts":[0.1,0.2,0.3,0.4],"width":100,"height":100}"#,
        ];
        for round in 0..4000u64 {
            let method = methods[(next() % methods.len() as u64) as usize];
            let base = seeds[(round % seeds.len() as u64) as usize]
                .as_bytes()
                .to_vec();
            let args: Vec<u8> = match round % 4 {
                0 => (0..(next() % 64)).map(|_| (next() & 0xff) as u8).collect(),
                1 => base[..(next() as usize % base.len())].to_vec(),
                2 => {
                    let mut b = base.clone();
                    for _ in 0..3 {
                        let i = next() as usize % b.len();
                        b[i] = (next() & 0x7f) as u8;
                    }
                    b
                }
                _ => base,
            };
            let out = call_json_with(&api, Some(method.as_bytes()), Some(&args));
            let v: Value = serde_json::from_str(&out).expect("envelope is JSON");
            assert!(
                v.get("ok").is_some() || v["error"]["code"].is_string(),
                "{out}"
            );
            assert_ne!(
                v["error"]["code"], "internal",
                "{method} panicked on {args:?}"
            );
        }
    }

    /// "capabilityPenV2" / "zoomViewReset" → "CAPABILITY_PEN_V2" / "ZOOM_VIEW_RESET".
    fn screaming(camel: &str) -> String {
        let mut out = String::new();
        let mut previous_lower = false;
        for c in camel.chars() {
            if c.is_ascii_uppercase() && previous_lower {
                out.push('_');
            }
            previous_lower = c.is_ascii_lowercase() || c.is_ascii_digit();
            out.push(c.to_ascii_uppercase());
        }
        out
    }

    /// Every `#define STREAMCORE_INPUT_*` in the header, in order, with its value as
    /// JSON (strings unquoted, `0x` numbers read as hex).
    fn header_defines() -> Vec<(String, Value)> {
        let header = include_str!("../include/streamcore_input.h");
        header
            .lines()
            .filter_map(|line| line.strip_prefix("#define STREAMCORE_INPUT_"))
            .filter_map(|rest| rest.split_once(' '))
            .map(|(name, value)| {
                let value = value.trim();
                let value = if let Some(text) = value.strip_prefix('"') {
                    json!(text.strip_suffix('"').unwrap())
                } else if let Some(hex) = value.strip_prefix("0x") {
                    json!(u64::from_str_radix(hex, 16).unwrap())
                } else {
                    json!(value.parse::<u64>().unwrap())
                };
                (name.to_owned(), value)
            })
            .collect()
    }

    /// The header's constants and vocabularies are exactly the library's, in
    /// the library's order, so C# copies them from one checked place.
    #[test]
    fn header_matches_the_library() {
        // The JSON results are objects (unordered); the order comes from the
        // library's lists behind them.
        let mut expected = vec![("ABI_VERSION".to_owned(), json!(ABI_VERSION))];
        let constants = call("constants", "{}")["ok"].clone();
        let listed = streamcore_input::constants::constants();
        assert_eq!(constants.as_object().unwrap().len(), listed.len());
        for (name, value) in listed {
            let camel = streamcore_input::constants::camel_case(name);
            assert_eq!(constants[&camel], value, "{camel}");
            expected.push((screaming(&camel), value));
        }
        let vocabularies = call("profile.vocabularies", "{}")["ok"].clone();
        let listed = streamcore_input::vocabulary::vocabularies();
        assert_eq!(vocabularies.as_object().unwrap().len(), listed.len());
        for (vocabulary, names) in listed {
            assert_eq!(vocabularies[vocabulary], json!(names), "{vocabulary}");
            for name in names {
                expected.push((
                    format!("{}_{}", screaming(vocabulary), screaming(name)),
                    json!(name),
                ));
            }
        }
        assert_eq!(header_defines(), expected);
    }

    #[test]
    fn large_input() {
        let text = "a".repeat(1 << 20);
        let args = json!({"text": text, "host": "mac"}).to_string();
        assert_eq!(
            call("latch.route", &args)["ok"]["text"]
                .as_str()
                .map(str::len),
            Some(1 << 20)
        );
    }
}
