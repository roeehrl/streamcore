//! A JSON-in/JSON-out interface over the whole crate, for the C ABI (PadWin's
//! P/Invoke, until uniffi-bindgen-cs supports UniFFI 0.32) and for fixture
//! runners in any language.
//!
//! `call(method, args)` takes a method name ("namespace.method") and a JSON
//! object of arguments and returns the result as JSON text. Objects (profile
//! stores, latch states, gesture trackers) live in this `JsonApi` instance and are
//! addressed by the integer `handle` their `*.new` method returns; release them
//! with `handle.release`. The method table is documented in docs/INPUT-MODEL.md.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::gestures::{FallbackAccumulator, TwoFingerOutput, TwoFingerRouter, TwoFingerTracker};
use crate::latch::LatchState;
use crate::modifiers::{self, HostOs};
use crate::presets::{self, PresetLibrary};
use crate::profile::{self, Profile};
use crate::store::ProfileStore;
use crate::typing::{self, HostCapabilities, LatchRoute};
use crate::wire::{self, GesturePhase, KeyPhase, PadMessageType};
use crate::{Result, StreamInputError, actions, keys, pencil, serde_util, shortcuts, time};

#[derive(Clone)]
enum Object {
    Presets(Arc<PresetLibrary>),
    Store(Arc<ProfileStore>),
    Latch(Arc<LatchState>),
    Tracker(Arc<TwoFingerTracker>),
    Accumulator(Arc<FallbackAccumulator>),
    Router(Arc<TwoFingerRouter>),
}

/// The JSON method table with its own object handles. Thread-safe.
#[derive(uniffi::Object)]
pub struct JsonApi {
    objects: Mutex<HashMap<u64, Object>>,
    next: AtomicU64,
}

impl Default for JsonApi {
    fn default() -> Self {
        Self {
            objects: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }
    }
}

impl std::fmt::Debug for JsonApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonApi").finish_non_exhaustive()
    }
}

fn arg<T: DeserializeOwned>(args: &Value, name: &str) -> Result<T> {
    let value = args
        .get(name)
        .ok_or_else(|| StreamInputError::arguments(format!("missing argument {name:?}")))?;
    serde_util::strict::from_value(value.clone())
        .map_err(|e| StreamInputError::arguments(format!("argument {name:?}: {e}")))
}

fn opt<T: DeserializeOwned>(args: &Value, name: &str) -> Result<Option<T>> {
    match args.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(_) => arg(args, name).map(Some),
    }
}

fn or<T: DeserializeOwned>(args: &Value, name: &str, default: T) -> Result<T> {
    Ok(opt(args, name)?.unwrap_or(default))
}

/// A message body given either as JSON text or as a JSON value.
fn body(args: &Value, name: &str) -> Result<String> {
    match args.get(name) {
        Some(Value::String(s)) => Ok(s.clone()),
        Some(v) => Ok(v.to_string()),
        None => Err(StreamInputError::arguments(format!(
            "missing argument {name:?}"
        ))),
    }
}

fn caps(args: &Value) -> Result<HostCapabilities> {
    match args.get("capabilities") {
        None | Some(Value::Null) => Ok(HostCapabilities::all()),
        Some(_) => Ok(typing::host_capabilities(arg(args, "capabilities")?)),
    }
}

fn to_value<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Result<Vec<u8>> {
    let clean: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if !clean.len().is_multiple_of(2) || !clean.is_ascii() {
        return Err(StreamInputError::arguments("hex has an odd length"));
    }
    (0..clean.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&clean[i..i + 2], 16)
                .map_err(|_| StreamInputError::arguments("hex has a non-hex digit"))
        })
        .collect()
}

/// `KeyLatch.route` in the fixtures' shape: `{"text": "..."}` or `{"shortcuts": [...]}`.
fn latch_route_json(route: LatchRoute) -> Value {
    match route {
        LatchRoute::Text { message } => json!({"text": message.text}),
        LatchRoute::Shortcuts { messages } => json!({"shortcuts": messages}),
    }
}

/// Router outputs in the fixtures' shape (`{"magnify": 0.1, "phase": "changed"}`).
pub fn router_output_json(output: &TwoFingerOutput) -> Value {
    use TwoFingerOutput::*;
    match output {
        GestureBegin { series } => json!({"gestureBegin": series}),
        GestureEnd { series } => json!({"gestureEnd": series}),
        Magnify { amount, phase } => json!({"magnify": amount, "phase": phase}),
        Rotate { degrees, phase } => json!({"rotate": degrees, "phase": phase}),
        Scroll { dx, dy, phase } => json!({"scroll": [dx, dy], "phase": phase}),
        ZoomScroll { dy, phase } => json!({"zoomScroll": dy, "phase": phase}),
        PanDrag { via, dx, dy, phase } => {
            json!({"panDrag": [dx, dy], "via": via, "phase": phase})
        }
        Shortcut { kind, message } => json!({"shortcut": kind, "message": message}),
        FallbackUnavailable { kind } => json!({"fallbackUnavailable": kind}),
    }
}

/// Decodes a body of `kind` and encodes it again with the production encoder.
fn reencode_wire(kind: &str, text: String) -> Result<String> {
    Ok(match kind {
        "text" => wire::encode_text(&wire::decode_text(text)?),
        "openSetup" => wire::encode_open_setup(&wire::decode_open_setup(text)?),
        "keyShortcut" => wire::encode_key_shortcut(&wire::decode_key_shortcut(text)?),
        "layoutInfo" => wire::encode_layout_info(&wire::decode_layout_info(text)?),
        "hostStats" => wire::encode_host_stats(&wire::decode_host_stats(text)?),
        "gesture" => wire::encode_gesture(&wire::decode_gesture(text)?),
        "notice" => wire::encode_notice(&wire::decode_notice(text)?),
        "serverNotices" => wire::encode_server_notices(wire::decode_server_notices(text)?),
        "source" => wire::encode_source(&wire::decode_source(text)?),
        "frontApp" => wire::encode_front_app(&wire::decode_front_app(text)?),
        "appList" => wire::encode_app_list(&wire::decode_app_list(text)?),
        "profilesSnapshot" => {
            wire::encode_profiles_snapshot(&wire::decode_profiles_snapshot(text)?)
        }
        "profileUpsert" => wire::encode_profile_upsert(&wire::decode_profile_upsert(text)?),
        "profileDelete" => wire::encode_profile_delete(&wire::decode_profile_delete(text)?),
        "profileChanged" => wire::encode_profile_changed(&wire::decode_profile_changed(text)?),
        "profile" => profile::encode_profile(&profile::decode_profile(text)?),
        other => {
            return Err(StreamInputError::arguments(format!(
                "unknown message type {other:?}"
            )));
        }
    })
}

impl JsonApi {
    /// A fresh method table with no objects.
    pub fn create() -> Self {
        Self::default()
    }

    fn insert(&self, object: Object) -> Value {
        let handle = self.next.fetch_add(1, Ordering::Relaxed);
        self.objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(handle, object);
        json!({ "handle": handle })
    }

    fn object(&self, args: &Value) -> Result<Object> {
        let handle: u64 = arg(args, "handle")?;
        self.objects
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&handle)
            .cloned()
            .ok_or(StreamInputError::UnknownHandle { handle })
    }

    fn handle_of<T>(
        &self,
        args: &Value,
        pick: impl Fn(Object) -> Option<Arc<T>>,
    ) -> Result<Arc<T>> {
        let handle: u64 = arg(args, "handle")?;
        pick(self.object(args)?).ok_or(StreamInputError::UnknownHandle { handle })
    }

    /// Calls `method` with `args` (a JSON object as text; empty means `{}`) and
    /// returns the result as JSON text.
    pub fn invoke(&self, method: &str, args: &str) -> Result<String> {
        let args: Value = if args.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(args).map_err(StreamInputError::json)?
        };
        if !args.is_object() {
            return Err(StreamInputError::arguments(
                "arguments must be a JSON object",
            ));
        }
        let result = self.dispatch(method, &args)?;
        Ok(result.to_string())
    }

    #[allow(clippy::too_many_lines)]
    fn dispatch(&self, method: &str, a: &Value) -> Result<Value> {
        let store = || {
            self.handle_of(a, |o| match o {
                Object::Store(s) => Some(s),
                _ => None,
            })
        };
        let latch = || {
            self.handle_of(a, |o| match o {
                Object::Latch(s) => Some(s),
                _ => None,
            })
        };
        let tracker = || {
            self.handle_of(a, |o| match o {
                Object::Tracker(s) => Some(s),
                _ => None,
            })
        };
        let accumulator = || {
            self.handle_of(a, |o| match o {
                Object::Accumulator(s) => Some(s),
                _ => None,
            })
        };
        let router = || {
            self.handle_of(a, |o| match o {
                Object::Router(s) => Some(s),
                _ => None,
            })
        };
        let host = || -> Result<HostOs> { arg(a, "host") };
        Ok(match method {
            "version" => json!({
                "inputModel": crate::INPUT_MODEL_VERSION,
                "crate": env!("CARGO_PKG_VERSION"),
            }),
            "constants" => Value::Object(
                crate::constants::constants()
                    .into_iter()
                    .map(|(name, value)| (crate::constants::camel_case(name), value))
                    .collect(),
            ),
            "handle.release" => {
                let handle: u64 = arg(a, "handle")?;
                let removed = self
                    .objects
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&handle)
                    .is_some();
                json!(removed)
            }

            // Wire
            "wire.decode" => {
                let text = reencode_wire(&arg::<String>(a, "type")?, body(a, "json")?)?;
                serde_json::from_str(&text).map_err(StreamInputError::json)?
            }
            "wire.encode" => json!(reencode_wire(&arg::<String>(a, "type")?, body(a, "json")?)?),
            "wire.messageTypes" => Value::Object(
                PadMessageType::ALL
                    .iter()
                    .map(|t| {
                        (
                            to_value(t).as_str().unwrap_or_default().to_owned(),
                            json!(t.code()),
                        )
                    })
                    .collect(),
            ),
            "wire.capabilities" => json!(wire::capability_names()),
            "wire.noticeCodes" => json!(wire::notice_codes()),
            "wire.isSnapshotRequest" => json!(wire::is_profiles_snapshot_request(body(a, "body")?)),
            "keyPacket.encode" => json!(hex(&wire::encode_key_packet(arg(a, "packet")?)?)),
            "keyPacket.decode" => {
                to_value(&wire::decode_key_packet(unhex(&arg::<String>(a, "hex")?)?)?)
            }
            "pencil.encode" => json!(hex(&pencil::encode_pencil_packet(
                arg(a, "samples")?,
                or(a, "penV2", false)?
            )?)),
            "pencil.decode" => to_value(&pencil::decode_pencil_packet(
                unhex(&arg::<String>(a, "hex")?)?,
                or(a, "penV2", false)?,
            )?),

            // Modifiers and keys
            "modifiers.resolve" => json!(modifiers::resolve_modifiers(arg(a, "stored")?, host()?)),
            "modifiers.makesShortcuts" => {
                json!(modifiers::makes_shortcuts(arg(a, "literal")?, host()?))
            }
            "modifiers.toStreamCore" => {
                json!(modifiers::modifiers_to_stream_core(arg(a, "literal")?)?)
            }
            "modifiers.fromStreamCore" => {
                json!(modifiers::modifiers_from_stream_core(arg(a, "bits")?)?)
            }
            "modifiers.label" => json!(modifiers::modifier_label(arg(a, "stored")?, host()?)),
            "keys.names" => json!(keys::named_key_names()),
            "keys.namedHid" => json!(
                keys::NamedKey::from_name(&arg::<String>(a, "named")?).map(keys::NamedKey::hid)
            ),
            "keys.usKey" => to_value(&keys::us_key_for_char(arg(a, "char")?)),
            "keys.usKeyForHid" => {
                json!(keys::us_key_for_hid(arg(a, "hid")?, or(a, "shift", false)?))
            }
            "keys.namedKeyForHid" => json!(keys::named_key_for_hid(arg(a, "hid")?)),
            "keys.isModifierHid" => json!(keys::is_modifier_hid(arg(a, "hid")?)),
            "keys.hardwareKeyForHost" => to_value(&keys::hardware_key_for_host(
                arg(a, "hid")?,
                or(a, "modifiers", 0)?,
                host()?,
            )),
            "keys.validateShortcut" => {
                let message = serde_util::strict::from_str(&body(a, "message")?)
                    .map_err(|e| StreamInputError::message(e.to_string()))?;
                wire::validate_key_shortcut(&message)?;
                Value::Null
            }

            // Typing
            "latch.merge" => json!(typing::key_latch_merge(
                or(a, "requested", 0)?,
                or(a, "latched", 0)?,
                or(a, "held", 0)?,
                host()?
            )),
            "latch.route" => latch_route_json(typing::key_latch_route(
                arg(a, "text")?,
                or(a, "latched", 0)?,
                or(a, "held", 0)?,
                host()?,
            )),
            "typing.routeText" => to_value(&typing::route_text(
                arg(a, "text")?,
                or(a, "replaceBefore", 0)?,
                or(a, "latched", 0)?,
                or(a, "held", 0)?,
                host()?,
                caps(a)?,
            )),
            "typing.routeKey" => to_value(&typing::route_key(
                arg(a, "key")?,
                or(a, "phase", KeyPhase::Tap)?,
                or(a, "repeat", false)?,
                or(a, "requested", 0)?,
                or(a, "latched", 0)?,
                or(a, "held", 0)?,
                host()?,
                caps(a)?,
            )),

            // Shortcuts
            "shortcuts.undo" => to_value(&shortcuts::undo_shortcut(host()?)),
            "shortcuts.redo" => to_value(&shortcuts::redo_shortcut(
                or(a, "appKey", String::new())?,
                host()?,
                opt(a, "profile")?,
            )),
            "shortcuts.forHost" => to_value(&crate::profile::shortcut_for_host(
                arg(a, "shortcut")?,
                host()?,
            )),
            "shortcuts.withHostEdit" => to_value(&crate::profile::shortcut_with_host_edit(
                arg(a, "shortcut")?,
                arg(a, "edited")?,
                host()?,
            )),
            "shortcuts.hasHostOverrides" => json!(crate::profile::shortcut_has_host_overrides(
                &arg(a, "shortcut")?
            )),
            "shortcuts.resolve" => {
                to_value(&shortcuts::resolve_shortcut(arg(a, "shortcut")?, host()?))
            }
            "shortcuts.isClosing" => json!(shortcuts::is_closing_combination(
                opt(a, "character")?,
                arg(a, "hid")?,
                arg(a, "modifiers")?,
                host()?,
            )),
            "shortcuts.guard" => to_value(&shortcuts::guard_shortcut(
                arg(a, "message")?,
                arg(a, "resolution")?,
                host()?,
                or(a, "layout", String::new())?,
            )),

            // Profiles and presets
            "profile.decode" => to_value(&profile::decode_profile(body(a, "profile")?)?),
            "profile.validate" => {
                let p = profile::decode_profile(body(a, "profile")?)?;
                let strict = or(a, "strict", false)?;
                let issues = profile::profile_issues(&p);
                let error = profile::validate_profile(&p, strict)
                    .err()
                    .map(|e| e.to_string());
                json!({"valid": error.is_none(), "error": error, "issues": issues})
            }
            "profile.defaults" => to_value(&profile::profile_defaults(&arg(a, "profile")?)),
            "profile.isNewer" => json!(profile::profile_is_newer(&arg(a, "a")?, &arg(a, "b")?)),
            "profile.appliesTo" => json!(profile::profile_applies_to(
                &arg(a, "profile")?,
                &arg::<String>(a, "appKey")?
            )),
            "profile.vocabularies" => Value::Object(
                crate::vocabulary::vocabularies()
                    .into_iter()
                    .map(|(name, values)| (name.to_owned(), json!(values)))
                    .collect(),
            ),
            "profile.normalizeAppKey" => {
                json!(profile::normalize_app_key(&arg::<String>(a, "appKey")?))
            }
            "presets.parseFile" => to_value(&presets::parse_preset_file(
                arg(a, "id")?,
                body(a, "json")?,
            )?),
            "presets.builtin" => to_value(&presets::builtin_presets()),
            "presets.lookup" => {
                let lib = PresetLibrary::shared_builtin();
                json!(
                    lib.for_app(&arg::<String>(a, "appKey")?)
                        .and_then(|p| p.preset.clone())
                )
            }
            "presets.new" => {
                let profiles: Vec<Profile> = or(a, "profiles", Vec::new())?;
                self.insert(Object::Presets(PresetLibrary::from_profiles(profiles)))
            }
            "actions.default" => to_value(&actions::default_action(arg(a, "slot")?)),
            "actions.effective" => to_value(&actions::effective_action(
                &arg(a, "profile")?,
                arg(a, "slot")?,
            )),
            "actions.tap" => to_value(&actions::tap_action(
                &arg(a, "profile")?,
                arg(a, "fingers")?,
            )),
            "actions.fingerPen" => to_value(&actions::finger_pen_action(
                &arg(a, "profile")?,
                arg(a, "fingers")?,
            )),
            "actions.followsSystem" => json!(actions::follows_system(&arg(a, "profile")?)),
            "actions.pencil" => to_value(&actions::pencil_gesture_response(
                &arg(a, "profile")?,
                arg(a, "gesture")?,
                arg(a, "system")?,
            )),
            "time.now" => json!(time::profile_timestamp_now()),
            "time.after" => json!(time::profile_timestamp_after(
                opt(a, "previous")?,
                opt(a, "now")?
            )?),
            "time.compare" => json!(time::compare_timestamps(arg(a, "a")?, arg(a, "b")?)),

            // ProfileStore
            "profileStore.new" => {
                let presets = match opt::<u64>(a, "presets")? {
                    None => PresetLibrary::shared_builtin(),
                    Some(handle) => match self.object(&json!({"handle": handle}))? {
                        Object::Presets(p) => p,
                        _ => return Err(StreamInputError::UnknownHandle { handle }),
                    },
                };
                self.insert(Object::Store(ProfileStore::with_presets(presets)))
            }
            "profileStore.loadJson" => to_value(&store()?.load_json(body(a, "json")?)?),
            "profileStore.saveJson" => json!(store()?.save_json()),
            "profileStore.revision" => json!(store()?.revision()),
            "profileStore.userProfiles" => to_value(&store()?.user_profiles()),
            "profileStore.snapshot" => to_value(&store()?.snapshot()),
            "profileStore.resolve" => to_value(&store()?.resolve(arg(a, "appKey")?)),
            "profileStore.current" => to_value(&store()?.current(arg(a, "appKey")?)),
            "profileStore.upsert" => to_value(&store()?.upsert(arg(a, "profile")?)),
            "profileStore.delete" => to_value(&store()?.delete(arg(a, "appKey")?)),
            "profileStore.importIfMissing" => {
                to_value(&store()?.import_if_missing(arg(a, "profiles")?))
            }
            "profileStore.applySnapshot" => {
                store()?.apply_snapshot(arg(a, "snapshot")?);
                Value::Null
            }
            "profileStore.applyChanged" => json!(store()?.apply_changed(arg(a, "message")?)),

            // LatchState
            "latchState.new" => self.insert(Object::Latch(LatchState::new())),
            "latchState.toggle" => json!(latch()?.toggle(arg(a, "modifiers")?)),
            "latchState.setLatched" => {
                latch()?.set_latched(arg(a, "modifiers")?);
                Value::Null
            }
            "latchState.setOneShot" => {
                latch()?.set_one_shot(arg(a, "oneShot")?);
                Value::Null
            }
            "latchState.hold" => json!(latch()?.hold(arg(a, "modifiers")?)),
            "latchState.release" => json!(latch()?.release(arg(a, "modifiers")?)),
            "latchState.state" => {
                let l = latch()?;
                json!({"latched": l.latched(), "held": l.held(), "active": l.active(), "oneShot": l.one_shot()})
            }
            "latchState.merge" => json!(latch()?.merge(or(a, "requested", 0)?, host()?)),
            "latchState.routeText" => to_value(&latch()?.route_text(
                arg(a, "text")?,
                or(a, "replaceBefore", 0)?,
                host()?,
                caps(a)?,
            )),
            "latchState.routeKey" => to_value(&latch()?.route_key(
                arg(a, "key")?,
                or(a, "phase", KeyPhase::Tap)?,
                or(a, "repeat", false)?,
                or(a, "requested", 0)?,
                host()?,
                caps(a)?,
            )),
            "latchState.keyPacket" => to_value(&latch()?.key_packet(arg(a, "packet")?, host()?)),
            "latchState.releaseAll" => to_value(&latch()?.release_all()),

            // Gestures
            "twoFingerTracker.new" => self.insert(Object::Tracker(TwoFingerTracker::new())),
            "twoFingerTracker.touches" => {
                let c: [f64; 4] = arg(a, "contacts")?;
                to_value(&tracker()?.touches(
                    c[0],
                    c[1],
                    c[2],
                    c[3],
                    arg(a, "width")?,
                    arg(a, "height")?,
                ))
            }
            "twoFingerTracker.end" => to_value(&tracker()?.end()),
            "twoFingerTracker.cancel" => to_value(&tracker()?.cancel()),
            "twoFingerTracker.isActive" => json!(tracker()?.is_active()),
            "fallbackAccumulator.new" => {
                self.insert(Object::Accumulator(FallbackAccumulator::new()))
            }
            "fallbackAccumulator.reset" => {
                accumulator()?.reset();
                Value::Null
            }
            "fallbackAccumulator.feedDeltas" => to_value(&accumulator()?.feed_deltas(
                or(a, "scale", 0.0)?,
                or(a, "rotation", 0.0)?,
                or(a, "zoomStep", 0.0)?,
                or(a, "rotateStepDegrees", 0.0)?,
            )),
            "fallbackAccumulator.feed" => {
                to_value(&accumulator()?.feed(arg(a, "message")?, opt(a, "fallbacks")?, host()?))
            }
            "twoFingerRouter.new" => self.insert(Object::Router(TwoFingerRouter::new())),
            "twoFingerRouter.handle" => Value::Array(
                router()?
                    .handle(
                        arg(a, "message")?,
                        opt(a, "delivery")?,
                        opt(a, "pan")?,
                        opt(a, "fallbacks")?,
                        or(a, "panScale", 1.0)?,
                        or(a, "host", HostOs::Mac)?,
                    )
                    .iter()
                    .map(router_output_json)
                    .collect(),
            ),
            "twoFingerRouter.finish" => Value::Array(
                router()?
                    .finish(or(a, "phase", GesturePhase::Cancelled)?)
                    .iter()
                    .map(router_output_json)
                    .collect(),
            ),
            other => {
                return Err(StreamInputError::UnknownMethod {
                    method: other.to_owned(),
                });
            }
        })
    }
}

#[uniffi::export]
impl JsonApi {
    /// A fresh method table with no objects.
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::create())
    }

    /// Calls a method; `args` is a JSON object as text. Returns JSON text.
    pub fn call(&self, method: String, args: String) -> Result<String> {
        self.invoke(&method, &args)
    }

    /// How many objects are alive.
    pub fn live_objects(&self) -> u64 {
        self.objects.lock().unwrap_or_else(|e| e.into_inner()).len() as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(api: &JsonApi, method: &str, args: Value) -> Value {
        serde_json::from_str(&api.invoke(method, &args.to_string()).unwrap()).unwrap()
    }

    #[test]
    fn stateless_calls() {
        let api = JsonApi::create();
        assert_eq!(
            call(
                &api,
                "modifiers.resolve",
                json!({"stored": 24, "host": "windows"})
            ),
            json!(10)
        );
        assert_eq!(
            call(
                &api,
                "wire.decode",
                json!({"type": "text", "json": {"text": "a"}})
            ),
            json!({"text": "a", "replaceBefore": 0})
        );
        assert_eq!(
            call(
                &api,
                "wire.encode",
                json!({"type": "text", "json": "{\"text\":\"a\"}"})
            ),
            json!("{\"text\":\"a\",\"replaceBefore\":0}")
        );
        assert_eq!(
            call(
                &api,
                "keyPacket.encode",
                json!({"packet": {"isDown": true, "hidUsage": 29, "modifiers": 8}})
            ),
            json!("011d0008")
        );
        assert_eq!(
            call(&api, "presets.lookup", json!({"appKey": "krita.exe"})),
            json!("krita")
        );
        assert_eq!(
            call(&api, "wire.messageTypes", json!({}))["keyShortcut"],
            json!(37)
        );
    }

    #[test]
    fn constants_and_vocabularies() {
        let api = JsonApi::create();
        let constants = call(&api, "constants", json!({}));
        assert_eq!(constants["migratedUpdatedAt"], "2000-01-02T00:00:00Z");
        assert_eq!(constants["presetUpdatedAt"], "2000-01-01T00:00:00Z");
        assert_eq!(constants["capabilityPenV2"], "penV2");
        assert_eq!(constants["noticeKeyUnavailable"], "key.unavailable");
        assert_eq!(constants["everyOtherAppKey"], "*");
        let vocabularies = call(&api, "profile.vocabularies", json!({}));
        assert_eq!(
            vocabularies["actionType"],
            json!(["shortcut", "text", "click", "hold", "pad", "none"])
        );
        assert_eq!(
            vocabularies["twoFingerMode"],
            json!(["sendToApp", "zoomView"])
        );
        assert_eq!(vocabularies.as_object().unwrap().len(), 7);
        assert_eq!(constants["hidLeftCommand"], 0xE3);
    }

    #[test]
    fn hid_helpers() {
        let api = JsonApi::create();
        assert_eq!(
            call(
                &api,
                "keys.hardwareKeyForHost",
                json!({"hid": 0xE3, "modifiers": 8, "host": "windows"})
            ),
            json!({"hidUsage": 0xE0, "modifiers": 2})
        );
        assert_eq!(
            call(
                &api,
                "keys.hardwareKeyForHost",
                json!({"hid": 6, "modifiers": 8, "host": "mac"})
            ),
            json!({"hidUsage": 6, "modifiers": 8})
        );
        assert_eq!(
            call(&api, "keys.usKeyForHid", json!({"hid": 0x1D})),
            json!("z")
        );
        assert_eq!(
            call(
                &api,
                "keys.usKeyForHid",
                json!({"hid": 0x1E, "shift": true})
            ),
            json!("!")
        );
        assert_eq!(
            call(&api, "keys.usKeyForHid", json!({"hid": 0x28})),
            Value::Null
        );
        assert_eq!(
            call(&api, "keys.namedKeyForHid", json!({"hid": 0x4B})),
            json!("pageUp")
        );
        assert_eq!(
            call(&api, "keys.namedKeyForHid", json!({"hid": 4})),
            Value::Null
        );
        assert_eq!(
            call(&api, "keys.isModifierHid", json!({"hid": 0xE7})),
            json!(true)
        );
    }

    #[test]
    fn handles_and_errors() {
        let api = JsonApi::create();
        let h = call(&api, "profileStore.new", json!({}))["handle"].clone();
        assert_eq!(
            call(&api, "profileStore.revision", json!({"handle": h})),
            json!(0)
        );
        let err = api.invoke(
            "latchState.toggle",
            &json!({"handle": h, "modifiers": 8}).to_string(),
        );
        assert!(matches!(err, Err(StreamInputError::UnknownHandle { .. })));
        assert_eq!(
            call(&api, "handle.release", json!({"handle": h})),
            json!(true)
        );
        assert_eq!(api.live_objects(), 0);
        assert!(matches!(
            api.invoke("nope", "{}"),
            Err(StreamInputError::UnknownMethod { .. })
        ));
        assert!(matches!(
            api.invoke("version", "[]"),
            Err(StreamInputError::InvalidArguments { .. })
        ));
        assert!(matches!(
            api.invoke("version", "{"),
            Err(StreamInputError::InvalidJson { .. })
        ));
        assert!(matches!(
            api.invoke("modifiers.resolve", "{}"),
            Err(StreamInputError::InvalidArguments { .. })
        ));
        assert!(api.invoke("version", "").is_ok());
    }
}
