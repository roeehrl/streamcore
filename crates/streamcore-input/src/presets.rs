//! Built-in app profile presets (`presets/*.json`, embedded at build time) and the
//! preset file format.
//!
//! A preset applies to its app (its `appKey` or an alias) until the user has a
//! profile covering that key. The `"*"` preset applies to every other app. Every
//! preset is dated [`PRESET_UPDATED_AT`] so any edit wins over it.
//!
//! [`PRESET_UPDATED_AT`]: crate::profile::PRESET_UPDATED_AT

use std::sync::{Arc, OnceLock};

use serde::{Deserialize, Serialize};

use crate::profile::{self, Profile};
use crate::serde_util;
use crate::{Result, StreamInputError};

/// The embedded preset files: (preset id = file name without `.json`, contents).
/// Sorted by id, which is also the library's order.
pub const BUILTIN_PRESET_FILES: [(&str, &str); 11] = [
    ("affinity", include_str!("../presets/affinity.json")),
    (
        "affinity-designer",
        include_str!("../presets/affinity-designer.json"),
    ),
    (
        "affinity-photo",
        include_str!("../presets/affinity-photo.json"),
    ),
    (
        "clipstudiopaint",
        include_str!("../presets/clipstudiopaint.json"),
    ),
    ("default", include_str!("../presets/default.json")),
    ("goodnotes", include_str!("../presets/goodnotes.json")),
    ("illustrator", include_str!("../presets/illustrator.json")),
    ("krita", include_str!("../presets/krita.json")),
    ("notability", include_str!("../presets/notability.json")),
    ("photoshop", include_str!("../presets/photoshop.json")),
    ("rebelle", include_str!("../presets/rebelle.json")),
];

/// The profiles read from one preset file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct PresetFile {
    /// Valid profiles, in file order; each without a `preset` id got the file's id.
    pub profiles: Vec<Profile>,
    /// Why entries were skipped (one line each).
    pub skipped: Vec<String>,
}

/// Reads a preset file: one Profile, an array of them, or `{"profiles": [...]}`.
/// Each profile without `preset` gets `id`; invalid entries are skipped (a broken
/// preset never takes the others down). Fails only when the text is not JSON.
#[uniffi::export]
pub fn parse_preset_file(id: String, json: String) -> Result<PresetFile> {
    let value: serde_json::Value = serde_json::from_str(&json).map_err(StreamInputError::json)?;
    let items = match value {
        serde_json::Value::Array(items) => items,
        serde_json::Value::Object(mut map) => match map.remove("profiles") {
            Some(serde_json::Value::Array(items)) if !map.contains_key("appKey") => items,
            Some(other) => {
                map.insert("profiles".to_owned(), other);
                vec![serde_json::Value::Object(map)]
            }
            None => vec![serde_json::Value::Object(map)],
        },
        _ => Vec::new(),
    };
    let mut file = PresetFile {
        profiles: Vec::new(),
        skipped: Vec::new(),
    };
    for (index, item) in items.into_iter().enumerate() {
        let decoded: std::result::Result<Profile, String> =
            serde_util::strict::from_value(item).map_err(|e| e.to_string());
        match decoded.and_then(|p| {
            profile::validate_profile(&p, false)
                .map(|()| p)
                .map_err(|e| e.to_string())
        }) {
            Ok(mut p) => {
                if p.preset.is_none() {
                    p.preset = Some(id.clone());
                }
                file.profiles.push(p);
            }
            Err(why) => file.skipped.push(format!("{id}[{index}]: {why}")),
        }
    }
    Ok(file)
}

/// A set of presets with lookup by app key and id.
#[derive(Debug, uniffi::Object)]
pub struct PresetLibrary {
    presets: Vec<Profile>,
    failures: Vec<String>,
}

impl PresetLibrary {
    /// A library over these profiles, in order.
    pub fn with_profiles(presets: Vec<Profile>) -> Self {
        Self {
            presets,
            failures: Vec::new(),
        }
    }

    /// A library from preset files (id, contents), in order.
    pub fn from_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        let mut presets = Vec::new();
        let mut failures = Vec::new();
        for (id, json) in files {
            match parse_preset_file(id.to_owned(), json.to_owned()) {
                Ok(file) => {
                    presets.extend(file.profiles);
                    failures.extend(file.skipped);
                }
                Err(e) => failures.push(format!("{id}: {e}")),
            }
        }
        Self { presets, failures }
    }

    /// The built-in presets, parsed once.
    pub fn shared_builtin() -> Arc<PresetLibrary> {
        static BUILTIN: OnceLock<Arc<PresetLibrary>> = OnceLock::new();
        BUILTIN
            .get_or_init(|| Arc::new(Self::from_files(BUILTIN_PRESET_FILES)))
            .clone()
    }

    /// Every preset, in library order.
    pub fn all(&self) -> &[Profile] {
        &self.presets
    }

    /// The preset made for this app (its key or an alias), never the `"*"` one.
    pub fn for_app(&self, app_key: &str) -> Option<&Profile> {
        self.presets
            .iter()
            .find(|p| !p.is_every_other_app() && p.applies_to(app_key))
    }

    /// The preset for every app without its own.
    pub fn every_other(&self) -> Option<&Profile> {
        self.presets.iter().find(|p| p.is_every_other_app())
    }

    /// The preset with this id.
    pub fn with_id(&self, id: &str) -> Option<&Profile> {
        self.presets
            .iter()
            .find(|p| p.preset.as_deref() == Some(id))
    }
}

#[uniffi::export]
impl PresetLibrary {
    /// The built-in presets.
    #[uniffi::constructor]
    pub fn builtin() -> Arc<Self> {
        Self::shared_builtin()
    }

    /// A library over these profiles (tests, custom preset packs).
    #[uniffi::constructor]
    pub fn from_profiles(presets: Vec<Profile>) -> Arc<Self> {
        Arc::new(Self::with_profiles(presets))
    }

    /// Every preset, in library order.
    pub fn presets(&self) -> Vec<Profile> {
        self.presets.clone()
    }

    /// Entries that did not load, with the reason.
    pub fn failures(&self) -> Vec<String> {
        self.failures.clone()
    }

    /// The preset made for this app (its key or an alias); never the `"*"` one.
    pub fn preset_for(&self, app_key: String) -> Option<Profile> {
        self.for_app(&app_key).cloned()
    }

    /// The `"*"` preset.
    pub fn every_other_app(&self) -> Option<Profile> {
        self.every_other().cloned()
    }

    /// The preset with this id.
    pub fn preset_with_id(&self, id: String) -> Option<Profile> {
        self.with_id(&id).cloned()
    }
}

/// Every built-in preset.
#[uniffi::export]
pub fn builtin_presets() -> Vec<Profile> {
    PresetLibrary::shared_builtin().presets.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_builtin_preset_loads_and_validates() {
        let lib = PresetLibrary::shared_builtin();
        assert!(lib.failures.is_empty(), "{:?}", lib.failures);
        assert_eq!(lib.all().len(), BUILTIN_PRESET_FILES.len());
        for (id, _) in BUILTIN_PRESET_FILES {
            let p = lib.with_id(id).unwrap_or_else(|| panic!("{id}"));
            assert_eq!(p.updated_at, profile::PRESET_UPDATED_AT, "{id}");
            profile::validate_profile(p, true).unwrap_or_else(|e| panic!("{id}: {e}"));
            assert!(
                p.fallbacks.as_ref().and_then(|f| f.redo.as_ref()).is_some(),
                "{id} has no redo"
            );
        }
        assert_eq!(
            lib.every_other().unwrap().preset.as_deref(),
            Some("default")
        );
    }

    #[test]
    fn builtin_presets_round_trip_exactly() {
        for (id, json) in BUILTIN_PRESET_FILES {
            let p: Profile = serde_json::from_str(json).unwrap();
            let back: serde_json::Value = serde_json::to_value(&p).unwrap();
            let orig: serde_json::Value = serde_json::from_str(json).unwrap();
            assert_eq!(back, orig, "{id}");
        }
    }

    #[test]
    fn lookups() {
        let lib = PresetLibrary::shared_builtin();
        let id = |k: &str| lib.for_app(k).and_then(|p| p.preset.clone());
        assert_eq!(id("com.adobe.photoshop").as_deref(), Some("photoshop"));
        assert_eq!(
            id("C:\\Program Files\\Krita (x64)\\bin\\KRITA.EXE").as_deref(),
            Some("krita")
        );
        assert_eq!(id("*"), None);
        assert_eq!(id("com.apple.TextEdit"), None);
    }

    #[test]
    fn file_shapes() {
        let one = r#"{"schema":1,"appKey":"a.exe","name":"A","updatedAt":"2026-01-01T00:00:00Z","groups":[]}"#;
        let f = parse_preset_file("one".into(), one.into()).unwrap();
        assert_eq!(f.profiles[0].preset.as_deref(), Some("one"));
        let wrapped = format!(r#"{{"profiles":[{one},{{"appKey":""}}]}}"#);
        let f = parse_preset_file("w".into(), wrapped).unwrap();
        assert_eq!(f.profiles.len(), 1);
        assert_eq!(f.skipped.len(), 1);
        assert!(parse_preset_file("x".into(), "nope".into()).is_err());
        assert!(
            parse_preset_file("x".into(), "3".into())
                .unwrap()
                .profiles
                .is_empty()
        );
    }
}
