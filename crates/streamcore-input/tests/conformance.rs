//! Runs protocol/input/fixtures through the JSON method table, the same way the
//! Swift, Kotlin and C# runners do through their bindings. Each fixture file's
//! `description` defines its format; protocol/input/README.md lists them.

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::{Value, json};
use streamcore_input::JsonApi;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../protocol/input/fixtures")
}

fn load(name: &str) -> Value {
    let path = fixtures_dir().join(name);
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn call(api: &JsonApi, method: &str, args: Value) -> Result<Value, String> {
    api.invoke(method, &args.to_string())
        .map(|text| serde_json::from_str(&text).unwrap())
        .map_err(|e| format!("{}: {e}", e.code()))
}

fn ok(api: &JsonApi, method: &str, args: Value) -> Value {
    call(api, method, args.clone()).unwrap_or_else(|e| panic!("{method} {args}: {e}"))
}

/// JSON equality with numbers compared within 1e-6; with `subset`, objects in
/// `expected` need only their own members to match.
fn same(expected: &Value, actual: &Value, subset: bool) -> bool {
    match (expected, actual) {
        (Value::Number(a), Value::Number(b)) => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            (a - b).abs() <= 1e-6 * a.abs().max(1.0)
        }
        (Value::Object(a), Value::Object(b)) => {
            (subset || a.len() == b.len())
                && a.iter().all(|(k, v)| {
                    b.get(k)
                        .map_or(subset && v.is_null(), |w| same(v, w, subset))
                })
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same(x, y, subset))
        }
        (a, b) => a == b,
    }
}

#[track_caller]
fn expect_same(context: &str, expected: &Value, actual: &Value, subset: bool) {
    assert!(
        same(expected, actual, subset),
        "{context}\n  expected: {expected}\n    actual: {actual}"
    );
}

fn name(case: &Value) -> String {
    case["name"].as_str().unwrap_or("?").to_owned()
}

#[test]
fn every_fixture_file_is_run() {
    let run: BTreeSet<&str> = [
        "action-defaults.json",
        "wire-messages.json",
        "binary-payloads.json",
        "modifiers.json",
        "latch-merge.json",
        "shortcut-guard.json",
        "redo.json",
        "profile-roundtrip.json",
        "profile-invalid.json",
        "preset-formats.json",
        "preset-lookup.json",
        "profile-merge.json",
        "twofinger-fallback.json",
        "twofinger-tracker.json",
    ]
    .into_iter()
    .collect();
    let present: BTreeSet<String> = std::fs::read_dir(fixtures_dir())
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.ends_with(".json").then_some(name)
        })
        .collect();
    let present: BTreeSet<&str> = present.iter().map(String::as_str).collect();
    assert_eq!(present, run);
}

#[test]
fn wire_messages() {
    let api = JsonApi::create();
    let doc = load("wire-messages.json");
    let mut tolerant = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for case in doc["cases"].as_array().unwrap() {
        let kind = case["type"].as_str().unwrap();
        // Only the duplicate-key cases give the body as text: a parsed fixture
        // file cannot keep a repeated key. wire.decode reads a string as text.
        if let Some(text) = case["json"].as_str() {
            assert!(
                name(case).starts_with("duplicate key: "),
                "{}: json is a string",
                name(case)
            );
            let parsed: Value = serde_json::from_str(text).unwrap();
            let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
            assert!(
                parsed.to_string().len() < compact.len(),
                "{}: no key is repeated",
                name(case)
            );
            duplicates.insert(kind.to_owned());
        }
        let actual = ok(
            &api,
            "wire.decode",
            json!({"type": kind, "json": case["json"]}),
        );
        let expected = case.get("canonical").unwrap_or(&case["json"]);
        expect_same(&name(case), expected, &actual, false);
        if name(case).starts_with("tolerant: ") {
            // A tolerant case drops something the decoder could not read.
            assert!(
                !same(&case["json"], &actual, false),
                "{} drops nothing",
                name(case)
            );
            tolerant.insert(kind.to_owned());
        }
    }
    let covered: BTreeSet<String> = ["serverNotices", "notice", "profilesSnapshot"]
        .into_iter()
        .map(str::to_owned)
        .collect();
    assert_eq!(tolerant, covered, "tolerant cases");
    // A repeated key keeps its last value, at the top level and nested.
    assert_eq!(duplicates.len(), 2, "duplicate-key cases: {duplicates:?}");
    for case in doc["invalid"].as_array().unwrap() {
        let kind = case["type"].as_str().unwrap();
        let result = call(
            &api,
            "wire.decode",
            json!({"type": kind, "json": case["json"]}),
        );
        assert!(result.is_err(), "{} decoded: {result:?}", name(case));
    }
    let types = ok(&api, "wire.messageTypes", json!({}));
    expect_same("message types", &doc["messageTypes"], &types, false);
}

#[test]
fn binary_payloads() {
    let api = JsonApi::create();
    let doc = load("binary-payloads.json");
    for case in doc["keyPacket"]["valid"].as_array().unwrap() {
        let packet = ok(&api, "keyPacket.decode", json!({"hex": case["hex"]}));
        expect_same(&name(case), &case["packet"], &packet, false);
        let hex = ok(&api, "keyPacket.encode", json!({"packet": case["packet"]}));
        assert_eq!(hex, case["hex"], "{}", name(case));
    }
    for case in doc["keyPacket"]["invalid"].as_array().unwrap() {
        assert!(
            call(&api, "keyPacket.decode", json!({"hex": case["hex"]})).is_err(),
            "{}",
            name(case)
        );
    }
    for case in doc["pencil"]["valid"].as_array().unwrap() {
        let args = json!({"hex": case["hex"], "penV2": case["penV2"]});
        let samples = ok(&api, "pencil.decode", args);
        expect_same(&name(case), &case["samples"], &samples, false);
        let hex = ok(
            &api,
            "pencil.encode",
            json!({"samples": case["samples"], "penV2": case["penV2"]}),
        );
        assert_eq!(hex, case["hex"], "{}", name(case));
    }
    for case in doc["pencil"]["invalid"].as_array().unwrap() {
        let args = json!({"hex": case["hex"], "penV2": case["penV2"]});
        assert!(call(&api, "pencil.decode", args).is_err(), "{}", name(case));
    }
}

#[test]
fn modifiers() {
    let api = JsonApi::create();
    let doc = load("modifiers.json");
    for case in doc["resolve"].as_array().unwrap() {
        for host in ["mac", "windows"] {
            let got = ok(
                &api,
                "modifiers.resolve",
                json!({"stored": case["stored"], "host": host}),
            );
            assert_eq!(got, case[host], "resolve {case} on {host}");
        }
    }
    for case in doc["streamCore"].as_array().unwrap() {
        assert_eq!(
            ok(
                &api,
                "modifiers.toStreamCore",
                json!({"literal": case["literal"]})
            ),
            case["bits"],
            "{case}"
        );
        assert_eq!(
            ok(
                &api,
                "modifiers.fromStreamCore",
                json!({"bits": case["bits"]})
            ),
            case["literal"],
            "{case}"
        );
    }
    for case in doc["fromStreamCore"].as_array().unwrap() {
        assert_eq!(
            ok(
                &api,
                "modifiers.fromStreamCore",
                json!({"bits": case["bits"]})
            ),
            case["literal"],
            "{case}"
        );
    }
    for literal in doc["streamCoreInvalid"]["literal"].as_array().unwrap() {
        assert!(
            call(&api, "modifiers.toStreamCore", json!({"literal": literal})).is_err(),
            "{literal}"
        );
    }
    for bits in doc["streamCoreInvalid"]["bits"].as_array().unwrap() {
        assert!(
            call(&api, "modifiers.fromStreamCore", json!({"bits": bits})).is_err(),
            "{bits}"
        );
    }
    for case in doc["makesShortcuts"].as_array().unwrap() {
        for host in ["mac", "windows"] {
            let got = ok(
                &api,
                "modifiers.makesShortcuts",
                json!({"literal": case["literal"], "host": host}),
            );
            assert_eq!(got, case[host], "makesShortcuts {case} on {host}");
        }
    }
    for case in doc["labels"].as_array().unwrap() {
        for host in ["mac", "windows"] {
            let got = ok(
                &api,
                "modifiers.label",
                json!({"stored": case["stored"], "host": host}),
            );
            assert_eq!(got, case[host], "label {case} on {host}");
        }
    }
}

#[test]
fn latch_merge_and_typing() {
    let api = JsonApi::create();
    let doc = load("latch-merge.json");
    for case in doc["merge"].as_array().unwrap() {
        assert_eq!(
            ok(&api, "latch.merge", case.clone()),
            case["expect"],
            "{case}"
        );
    }
    for case in doc["route"].as_array().unwrap() {
        let got = ok(&api, "latch.route", case.clone());
        expect_same(&name(case), &case["expect"], &got, false);
    }
    for case in doc["routeText"].as_array().unwrap() {
        let got = ok(&api, "typing.routeText", case.clone());
        expect_same(&name(case), &case["expect"], &got, false);
    }
    for case in doc["routeKey"].as_array().unwrap() {
        let got = ok(&api, "typing.routeKey", case.clone());
        expect_same(&name(case), &case["expect"], &got, false);
    }
    for case in doc["latchState"].as_array().unwrap() {
        let handle = ok(&api, "latchState.new", json!({}))["handle"].clone();
        for step in case["steps"].as_array().unwrap() {
            let mut args = step["args"].clone();
            args["handle"] = handle.clone();
            let method = format!("latchState.{}", step["method"].as_str().unwrap());
            let got = ok(&api, &method, args);
            expect_same(
                &format!("{} / {step}", name(case)),
                &step["expect"],
                &got,
                false,
            );
        }
    }
}

#[test]
fn shortcut_guard_and_redo() {
    let api = JsonApi::create();
    let doc = load("shortcut-guard.json");
    for case in doc["cases"].as_array().unwrap() {
        let got = ok(&api, "shortcuts.isClosing", case.clone());
        assert_eq!(got, case["closing"], "{case}");
    }
    for case in doc["decisions"].as_array().unwrap() {
        let got = ok(&api, "shortcuts.guard", case.clone());
        expect_same(&name(case), &case["expect"], &got, false);
    }
    for case in doc["forHost"].as_array().unwrap() {
        let got = ok(&api, "shortcuts.forHost", case.clone());
        expect_same(&name(case), &case["expect"], &got, false);
        let resolved = ok(&api, "shortcuts.resolve", case.clone());
        expect_same(&name(case), &case["resolve"], &resolved, false);
    }
    for case in doc["withHostEdit"].as_array().unwrap() {
        let got = ok(&api, "shortcuts.withHostEdit", case.clone());
        expect_same(&name(case), &case["expect"], &got, false);
    }
    let doc = load("redo.json");
    for case in doc["undo"].as_array().unwrap() {
        expect_same(
            "undo",
            &case["expect"],
            &ok(&api, "shortcuts.undo", case.clone()),
            false,
        );
    }
    for case in doc["redo"].as_array().unwrap() {
        let got = ok(&api, "shortcuts.redo", case.clone());
        expect_same(&case.to_string(), &case["expect"], &got, false);
    }
}

#[test]
fn profile_round_trips_and_defaults() {
    let api = JsonApi::create();
    let doc = load("profile-roundtrip.json");
    for case in doc["cases"].as_array().unwrap() {
        let got = ok(&api, "profile.decode", json!({"profile": case["profile"]}));
        expect_same(&name(case), &case["profile"], &got, false);
        let valid = ok(
            &api,
            "profile.validate",
            json!({"profile": case["profile"]}),
        );
        assert_eq!(valid["valid"], true, "{}: {valid}", name(case));
        if let Some(defaults) = case.get("defaults") {
            let got = ok(
                &api,
                "profile.defaults",
                json!({"profile": case["profile"]}),
            );
            expect_same(&format!("{} defaults", name(case)), defaults, &got, true);
        }
    }
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("presets");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "json") {
            let original: Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            let got = ok(&api, "profile.decode", json!({"profile": original}));
            expect_same(&format!("{path:?}"), &original, &got, false);
        }
    }
}

#[test]
fn invalid_profiles() {
    let api = JsonApi::create();
    let doc = load("profile-invalid.json");
    for case in doc["cases"].as_array().unwrap() {
        let hint = case["hint"].as_str().unwrap().to_lowercase();
        let message = match call(
            &api,
            "profile.validate",
            json!({"profile": case["profile"]}),
        ) {
            Err(e) => e,
            Ok(v) => {
                assert_eq!(v["valid"], false, "{} was accepted", name(case));
                v["error"].as_str().unwrap().to_owned()
            }
        };
        assert!(
            message.to_lowercase().contains(&hint),
            "{}: {message:?} lacks {hint:?}",
            name(case)
        );
    }
    for case in doc["strictOnly"].as_array().unwrap() {
        let lenient = ok(
            &api,
            "profile.validate",
            json!({"profile": case["profile"]}),
        );
        assert_eq!(lenient["valid"], true, "{}", name(case));
        let strict = ok(
            &api,
            "profile.validate",
            json!({"profile": case["profile"], "strict": true}),
        );
        assert_eq!(strict["valid"], false, "{}", name(case));
    }
}

#[test]
fn preset_formats_and_lookup() {
    let api = JsonApi::create();
    let doc = load("preset-formats.json");
    for case in doc["cases"].as_array().unwrap() {
        let file = ok(
            &api,
            "presets.parseFile",
            json!({"id": case["id"], "json": case["file"]}),
        );
        let got: Vec<Value> = file["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| json!({"appKey": p["appKey"], "preset": p["preset"]}))
            .collect();
        expect_same(&name(case), &case["expect"], &Value::Array(got), false);
    }

    let doc = load("preset-lookup.json");
    let builtin = ok(&api, "presets.builtin", json!({}));
    let builtin = builtin.as_array().unwrap();
    for id in doc["presets"].as_array().unwrap() {
        let p = builtin
            .iter()
            .find(|p| p["preset"] == *id)
            .unwrap_or_else(|| panic!("{id}"));
        assert_eq!(p["updatedAt"], "2000-01-01T00:00:00Z", "{id}");
        assert!(p["fallbacks"]["redo"].is_object(), "{id} has no redo");
    }
    assert_eq!(builtin.len(), doc["presets"].as_array().unwrap().len());
    for case in doc["lookups"].as_array().unwrap() {
        let got = ok(&api, "presets.lookup", json!({"appKey": case["appKey"]}));
        assert_eq!(got, case["preset"], "{case}");
    }
    let store_with = |stored: &Value| {
        let handle = ok(&api, "profileStore.new", json!({}))["handle"].clone();
        for p in stored.as_array().unwrap() {
            let outcome = ok(
                &api,
                "profileStore.upsert",
                json!({"handle": handle, "profile": p}),
            );
            assert_eq!(outcome["changed"], true, "{outcome}");
        }
        handle
    };
    for case in doc["resolve"].as_array().unwrap() {
        let handle = store_with(&case["stored"]);
        let got = ok(
            &api,
            "profileStore.resolve",
            json!({"handle": handle, "appKey": case["appKey"]}),
        );
        let is_stored = case["stored"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["name"] == got["name"]);
        let label = if is_stored {
            format!("stored:{}", got["name"].as_str().unwrap())
        } else {
            format!("preset:{}", got["preset"].as_str().unwrap())
        };
        assert_eq!(label, case["expect"].as_str().unwrap(), "{}", name(case));
    }
    for case in doc["effective"].as_array().unwrap() {
        let handle = store_with(&case["stored"]);
        let snapshot = ok(&api, "profileStore.snapshot", json!({"handle": handle}));
        let profiles = snapshot["profiles"].as_array().unwrap();
        assert_eq!(
            profiles.len() as u64,
            case["expectCount"].as_u64().unwrap(),
            "{}",
            name(case)
        );
        for id in case["expectAbsentPresets"].as_array().unwrap() {
            let preset = builtin.iter().find(|p| p["preset"] == *id).unwrap();
            assert!(!profiles.contains(preset), "{}: {id} present", name(case));
        }
        for stored in case["expectStored"].as_array().unwrap() {
            assert!(
                profiles.iter().any(|p| p["name"] == *stored),
                "{}: {stored} missing",
                name(case)
            );
        }
    }
}

/// `fixtures/presets/builtin` ships the built-in presets for app runners. The
/// crate's `presets/` stays the source: the shipped copy must hold exactly the
/// same `.json` files, byte for byte (refresh it with
/// `cp crates/streamcore-input/presets/*.json protocol/input/fixtures/presets/builtin/`),
/// and each must read back as the embedded preset.
#[test]
fn shipped_presets_are_the_builtin_ones() {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("presets");
    let shipped = fixtures_dir().join("presets").join("builtin");
    let json_files = |dir: &PathBuf| -> BTreeSet<String> {
        std::fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("{dir:?}: {e}"))
            .filter_map(|e| {
                let name = e.unwrap().file_name().into_string().unwrap();
                name.ends_with(".json").then_some(name)
            })
            .collect()
    };
    let files = json_files(&source);
    assert_eq!(
        json_files(&shipped),
        files,
        "{shipped:?} differs from {source:?}"
    );
    let other: Vec<String> = std::fs::read_dir(&shipped)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|name| !name.ends_with(".json"))
        .collect();
    assert!(
        other.is_empty(),
        "unexpected files in {shipped:?}: {other:?}"
    );

    let api = JsonApi::create();
    let builtin = ok(&api, "presets.builtin", json!({}));
    let builtin = builtin.as_array().unwrap();
    assert_eq!(builtin.len(), files.len());
    for file in &files {
        let original = std::fs::read(source.join(file)).unwrap();
        let copy = std::fs::read(shipped.join(file)).unwrap();
        assert!(
            original == copy,
            "{file}: the shipped copy differs from crates/streamcore-input/presets"
        );
        let id = file.trim_end_matches(".json");
        let parsed = ok(
            &api,
            "presets.parseFile",
            json!({"id": id, "json": String::from_utf8(copy).unwrap()}),
        );
        assert_eq!(parsed["skipped"], json!([]), "{file}");
        let embedded = builtin
            .iter()
            .find(|p| p["preset"] == id)
            .unwrap_or_else(|| panic!("{id} is not built in"));
        expect_same(file, embedded, &parsed["profiles"][0], false);
        assert_eq!(parsed["profiles"].as_array().unwrap().len(), 1, "{file}");
    }
}

#[test]
fn profile_merge_scenarios() {
    let api = JsonApi::create();
    let doc = load("profile-merge.json");
    let builtin = ok(&api, "presets.builtin", json!({}));
    for scenario in doc["scenarios"].as_array().unwrap() {
        let handle = if scenario["presets"] == "builtin" {
            ok(&api, "profileStore.new", json!({}))["handle"].clone()
        } else {
            let mut profiles = Vec::new();
            for file in scenario["presets"].as_array().unwrap() {
                let file = file.as_str().unwrap();
                let text =
                    std::fs::read_to_string(fixtures_dir().join("presets").join(file)).unwrap();
                let id = file.trim_end_matches(".json");
                let parsed = ok(&api, "presets.parseFile", json!({"id": id, "json": text}));
                profiles.extend(parsed["profiles"].as_array().unwrap().iter().cloned());
            }
            let presets = ok(&api, "presets.new", json!({"profiles": profiles}))["handle"].clone();
            ok(&api, "profileStore.new", json!({"presets": presets}))["handle"].clone()
        };
        let reply_matches = |expect: &Value, message: &Value, context: &str| {
            if let Some(n) = expect.get("name") {
                assert_eq!(&message["profile"]["name"], n, "{context}: {message}");
            }
            if let Some(d) = expect.get("deleted") {
                assert_eq!(&message["deleted"], d, "{context}: {message}");
            }
        };
        for step in scenario["steps"].as_array().unwrap() {
            let context = format!("{} / {step}", name(scenario));
            let expect = &step["expect"];
            match step["op"].as_str().unwrap() {
                "forApp" => {
                    let got = ok(
                        &api,
                        "profileStore.resolve",
                        json!({"handle": handle, "appKey": step["appKey"]}),
                    );
                    if expect.is_null() {
                        assert!(got.is_null(), "{context}: {got}");
                    } else {
                        expect_same(&context, expect, &got, true);
                    }
                }
                op @ ("upsert" | "upsertPreset") => {
                    let profile = if op == "upsert" {
                        step["profile"].clone()
                    } else {
                        builtin
                            .as_array()
                            .unwrap()
                            .iter()
                            .find(|p| p["preset"] == step["preset"])
                            .unwrap()
                            .clone()
                    };
                    let got = ok(
                        &api,
                        "profileStore.upsert",
                        json!({"handle": handle, "profile": profile}),
                    );
                    assert_eq!(got["result"], expect["result"], "{context}: {got}");
                    assert_eq!(got["changed"], expect["changed"], "{context}: {got}");
                    assert_eq!(got["revision"], expect["revision"], "{context}: {got}");
                    assert_eq!(
                        got["message"]["revision"], expect["revision"],
                        "{context}: {got}"
                    );
                    reply_matches(expect, &got["message"], &context);
                }
                "delete" => {
                    let got = ok(
                        &api,
                        "profileStore.delete",
                        json!({"handle": handle, "appKey": step["appKey"]}),
                    );
                    for key in ["removed", "revision", "revealedPreset", "sendSnapshot"] {
                        assert_eq!(got[key], expect[key], "{context}: {key} in {got}");
                    }
                    reply_matches(expect, &got["message"], &context);
                }
                "import" => {
                    let got = ok(
                        &api,
                        "profileStore.importIfMissing",
                        json!({"handle": handle, "profiles": step["profiles"]}),
                    );
                    let names: Vec<Value> = got
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|m| m["profile"]["name"].clone())
                        .collect();
                    assert_eq!(Value::Array(names), expect["names"], "{context}");
                    let revision = ok(&api, "profileStore.revision", json!({"handle": handle}));
                    assert_eq!(revision, expect["revision"], "{context}");
                }
                "snapshot" => {
                    let got = ok(&api, "profileStore.snapshot", json!({"handle": handle}));
                    assert_eq!(got["revision"], expect["revision"], "{context}");
                    let names: Vec<Value> = got["profiles"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|p| p["name"].clone())
                        .collect();
                    if let Some(expected) = expect.get("names") {
                        assert_eq!(&Value::Array(names.clone()), expected, "{context}");
                    }
                    if let Some(count) = expect.get("count") {
                        assert_eq!(names.len() as u64, count.as_u64().unwrap(), "{context}");
                    }
                }
                other => panic!("unknown op {other}"),
            }
        }
        if let Some(stored) = scenario.get("storedNames") {
            let got = ok(&api, "profileStore.userProfiles", json!({"handle": handle}));
            let names: Vec<Value> = got
                .as_array()
                .unwrap()
                .iter()
                .map(|p| p["name"].clone())
                .collect();
            assert_eq!(&Value::Array(names), stored, "{} stored", name(scenario));
        }
    }
}

#[test]
fn two_finger_fallbacks() {
    let api = JsonApi::create();
    let doc = load("twofinger-fallback.json");
    for case in doc["cases"].as_array().unwrap() {
        let router = ok(&api, "twoFingerRouter.new", json!({}))["handle"].clone();
        let fallbacks = &doc["fallbacks"][case["fallbacks"].as_str().unwrap()];
        for message in case["messages"].as_array().unwrap() {
            let mut gesture = json!({"kind": "twoFinger"});
            for key in ["phase", "scale", "rotation", "dx", "dy"] {
                if let Some(v) = message.get(key) {
                    gesture[key] = v.clone();
                }
            }
            let got = ok(
                &api,
                "twoFingerRouter.handle",
                json!({
                    "handle": router,
                    "message": gesture,
                    "delivery": case["delivery"],
                    "pan": case.get("pan").cloned().unwrap_or(Value::Null),
                    "fallbacks": fallbacks,
                    "panScale": case["panScale"],
                    "host": case.get("host").cloned().unwrap_or(json!("mac")),
                }),
            );
            expect_same(
                &format!("{} / {message}", name(case)),
                &message["expect"],
                &got,
                true,
            );
        }
    }
    for case in doc["steps"].as_array().unwrap() {
        let acc = ok(&api, "fallbackAccumulator.new", json!({}))["handle"].clone();
        for feed in case["feeds"].as_array().unwrap() {
            if feed.get("reset").is_some() {
                ok(&api, "fallbackAccumulator.reset", json!({"handle": acc}));
                continue;
            }
            let got = ok(
                &api,
                "fallbackAccumulator.feedDeltas",
                json!({"handle": acc, "scale": feed["scale"], "rotation": feed["rotation"],
                       "zoomStep": case["zoomStep"], "rotateStepDegrees": case["rotateStepDegrees"]}),
            );
            let pair = json!([got["zoom"], got["rotate"]]);
            assert_eq!(pair, feed["expect"], "{} / {feed}", name(case));
        }
    }
}

#[test]
fn action_defaults() {
    let api = JsonApi::create();
    let doc = load("action-defaults.json");
    for (slot, expect) in doc["defaults"].as_object().unwrap() {
        let got = ok(&api, "actions.default", json!({"slot": slot}));
        expect_same(slot, expect, &got, false);
    }
    assert_eq!(doc["defaults"].as_object().unwrap().len(), 11);
    for case in doc["effective"].as_array().unwrap() {
        let got = ok(&api, "actions.effective", case.clone());
        expect_same(&name(case), &case["expect"], &got, false);
    }
    for (key, method) in [("taps", "actions.tap"), ("fingerPen", "actions.fingerPen")] {
        for case in doc[key].as_array().unwrap() {
            let got = ok(&api, method, case.clone());
            expect_same(&case.to_string(), &case["expect"], &got, false);
        }
    }
    for case in doc["pencil"].as_array().unwrap() {
        let got = ok(&api, "actions.pencil", case.clone());
        expect_same(&name(case), &case["expect"], &got, false);
    }
}

#[test]
fn two_finger_tracker() {
    let api = JsonApi::create();
    let doc = load("twofinger-tracker.json");
    for case in doc["cases"].as_array().unwrap() {
        let t = ok(&api, "twoFingerTracker.new", json!({}))["handle"].clone();
        for frame in case["frames"].as_array().unwrap() {
            let got = if frame.get("end").is_some() {
                ok(&api, "twoFingerTracker.end", json!({"handle": t}))
            } else if frame.get("cancel").is_some() {
                ok(&api, "twoFingerTracker.cancel", json!({"handle": t}))
            } else {
                ok(
                    &api,
                    "twoFingerTracker.touches",
                    json!({"handle": t, "contacts": frame["touches"], "width": frame["size"][0], "height": frame["size"][1]}),
                )
            };
            expect_same(
                &format!("{} / {frame}", name(case)),
                &frame["expect"],
                &got,
                false,
            );
        }
    }
}
