//! The profile store: the host's merge rules (Pad's input protocol v2, App profiles)
//! and, for clients, a mirror of the host's profiles.
//!
//! Rules:
//! - Resolution for an app key: the user's profile whose `appKey` or `aliases`
//!   match, then a preset that matches, then the user's `"*"`, then the `"*"`
//!   preset. A preset still applies to any of its keys no user profile covers.
//! - Upsert, last writer wins by `updatedAt`. Against a stored profile that shares
//!   a key or alias: only a strictly newer one replaces it (an identical one is
//!   accepted without a change). Against a preset when nothing is stored: only a
//!   profile newer than the preset is stored, so an unedited preset copy is not
//!   (presets are dated 2000-01-01, migrated profiles 2000-01-02).
//! - Every stored change bumps the revision by one. An upsert or delete that
//!   changes nothing still yields the host's current version at the current
//!   revision, for the sender to converge on.
//! - A delete that brings a preset back asks for a fresh `profilesSnapshot`.
//!
//! Native code does the file IO ([`ProfileStore::load_json`] /
//! [`ProfileStore::save_json`], the `profiles.json` format
//! `{"schema": 1, "revision": N, "profiles": [...]}`) and the sending.

use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::presets::PresetLibrary;
use crate::profile::{self, PROFILE_SCHEMA, Profile, profiles_overlap};
use crate::serde_util::{self, same_json};
use crate::wire::{ProfileChangedMessage, ProfilesSnapshotMessage};
use crate::{Result, StreamInputError};

/// How an upsert went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, uniffi::Enum)]
#[serde(rename_all = "camelCase")]
pub enum UpsertResult {
    /// Stored, or identical to what is stored (then `changed` is false).
    Accepted,
    /// Not newer than the stored profile or the preset it would replace.
    Stale,
    /// Refused: fails validation (schema, appKey, updatedAt, keys, modifiers).
    Invalid,
}

/// The result of [`ProfileStore::upsert`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct UpsertOutcome {
    pub result: UpsertResult,
    /// The store changed and the revision went up: send `message` to every client
    /// and save. Otherwise send `message` back to the sender only.
    pub changed: bool,
    pub revision: u64,
    /// What applies to the app now (the stored profile or preset), or `deleted`.
    pub message: ProfileChangedMessage,
    /// Why the profile was refused or kept out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The result of [`ProfileStore::delete`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct DeleteOutcome {
    /// A user profile was removed and the revision went up: broadcast `message`.
    pub removed: bool,
    pub revision: u64,
    /// The app's preset if it has one, else `deleted`.
    pub message: ProfileChangedMessage,
    /// The id of the preset that applies again after the removal.
    #[serde(default)]
    pub revealed_preset: Option<String>,
    /// Follow with a fresh `profilesSnapshot` (a preset came back).
    pub send_snapshot: bool,
}

/// The result of [`ProfileStore::load_json`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, uniffi::Record)]
#[serde(rename_all = "camelCase")]
pub struct LoadReport {
    pub loaded: u32,
    pub revision: u64,
    /// Entries left out, with the reason.
    pub skipped: Vec<String>,
}

#[derive(Debug, Default)]
struct State {
    revision: u64,
    user: Vec<Profile>,
}

#[derive(Serialize, Deserialize)]
struct StoreFile {
    #[serde(default = "schema_one")]
    schema: u32,
    #[serde(default)]
    revision: u64,
    #[serde(default)]
    profiles: Vec<serde_json::Value>,
}

fn schema_one() -> u32 {
    PROFILE_SCHEMA
}

/// The user's profiles over a preset library, with a revision. Thread-safe; one
/// short internal lock, no callbacks.
#[derive(Debug, uniffi::Object)]
pub struct ProfileStore {
    presets: Arc<PresetLibrary>,
    state: Mutex<State>,
}

impl State {
    fn user_for(&self, app_key: &str) -> Option<&Profile> {
        if app_key.trim() == profile::EVERY_OTHER_APP {
            return self.user.iter().find(|p| p.is_every_other_app());
        }
        self.user
            .iter()
            .find(|p| !p.is_every_other_app() && p.applies_to(app_key))
    }

    fn resolve<'a>(&'a self, presets: &'a PresetLibrary, app_key: &str) -> Option<&'a Profile> {
        if app_key.trim() != profile::EVERY_OTHER_APP
            && let Some(own) = self.user_for(app_key).or_else(|| presets.for_app(app_key))
        {
            return Some(own);
        }
        self.user
            .iter()
            .find(|p| p.is_every_other_app())
            .or_else(|| presets.every_other())
    }

    /// The app's own profile (stored or preset, `"*"` only for `"*"`).
    fn own<'a>(&'a self, presets: &'a PresetLibrary, app_key: &str) -> Option<&'a Profile> {
        if app_key.trim() == profile::EVERY_OTHER_APP {
            return self.user_for(app_key).or_else(|| presets.every_other());
        }
        self.user_for(app_key).or_else(|| presets.for_app(app_key))
    }

    fn current(&self, presets: &PresetLibrary, app_key: &str) -> ProfileChangedMessage {
        match self.own(presets, app_key) {
            Some(p) => ProfileChangedMessage {
                revision: self.revision,
                profile: Some(p.clone()),
                deleted: None,
            },
            None => ProfileChangedMessage {
                revision: self.revision,
                profile: None,
                deleted: Some(app_key.to_owned()),
            },
        }
    }

    fn covered(&self, preset: &Profile) -> bool {
        if preset.is_every_other_app() {
            return self.user.iter().any(Profile::is_every_other_app);
        }
        let keys = preset.normalized_keys();
        !keys.is_empty()
            && keys.iter().all(|k| {
                self.user
                    .iter()
                    .any(|u| !u.is_every_other_app() && u.normalized_keys().contains(k))
            })
    }

    fn effective(&self, presets: &PresetLibrary) -> Vec<Profile> {
        let mut out = self.user.clone();
        out.extend(presets.all().iter().filter(|p| !self.covered(p)).cloned());
        out
    }

    fn matching_preset<'a>(
        &self,
        presets: &'a PresetLibrary,
        incoming: &Profile,
    ) -> Option<&'a Profile> {
        if incoming.is_every_other_app() {
            return presets.every_other();
        }
        presets
            .all()
            .iter()
            .find(|p| !p.is_every_other_app() && profiles_overlap(p, incoming))
    }

    fn upsert(&mut self, presets: &PresetLibrary, incoming: Profile) -> UpsertOutcome {
        let key = incoming.app_key.clone();
        let kept = |state: &Self, result, reason: String| UpsertOutcome {
            result,
            changed: false,
            revision: state.revision,
            message: state.current(presets, &key),
            reason: Some(reason),
        };
        if let Err(e) = profile::validate_profile(&incoming, false) {
            return kept(self, UpsertResult::Invalid, e.to_string());
        }
        let overlapping: Vec<usize> = (0..self.user.len())
            .filter(|&i| profiles_overlap(&self.user[i], &incoming))
            .collect();
        if overlapping
            .iter()
            .any(|&i| same_json(&self.user[i], &incoming))
        {
            return UpsertOutcome {
                result: UpsertResult::Accepted,
                changed: false,
                revision: self.revision,
                message: self.current(presets, &key),
                reason: None,
            };
        }
        if let Some(&first) = overlapping.first() {
            if let Some(&newer) = overlapping
                .iter()
                .find(|&&i| !incoming.is_newer_than(&self.user[i]))
            {
                let reason = format!(
                    "the host's profile {:?} ({}) is as new or newer",
                    self.user[newer].name, self.user[newer].updated_at
                );
                return kept(self, UpsertResult::Stale, reason);
            }
            for &i in overlapping.iter().rev() {
                self.user.remove(i);
            }
            self.user.insert(first, incoming.clone());
        } else {
            if let Some(preset) = self.matching_preset(presets, &incoming)
                && !incoming.is_newer_than(preset)
            {
                let reason = format!(
                    "not newer than the preset {:?}; the preset keeps applying",
                    preset.preset.clone().unwrap_or_default()
                );
                return kept(self, UpsertResult::Stale, reason);
            }
            self.user.push(incoming.clone());
        }
        self.revision += 1;
        UpsertOutcome {
            result: UpsertResult::Accepted,
            changed: true,
            revision: self.revision,
            message: ProfileChangedMessage {
                revision: self.revision,
                profile: Some(incoming),
                deleted: None,
            },
            reason: None,
        }
    }

    fn delete(&mut self, presets: &PresetLibrary, app_key: &str) -> DeleteOutcome {
        let before = self.user.len();
        let every = app_key.trim() == profile::EVERY_OTHER_APP;
        self.user.retain(|p| {
            if every {
                !p.is_every_other_app()
            } else {
                p.is_every_other_app() || !p.applies_to(app_key)
            }
        });
        let removed = self.user.len() != before;
        if removed {
            self.revision += 1;
        }
        let message = self.current(presets, app_key);
        let revealed_preset = if removed && self.user_for(app_key).is_none() {
            let preset = if every {
                presets.every_other()
            } else {
                presets.for_app(app_key)
            };
            preset.and_then(|p| p.preset.clone())
        } else {
            None
        };
        DeleteOutcome {
            removed,
            revision: self.revision,
            send_snapshot: revealed_preset.is_some(),
            revealed_preset,
            message,
        }
    }
}

impl ProfileStore {
    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic while holding the lock cannot leave State half-updated in a way
        // that matters more than refusing every later call would.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A store over these presets.
    pub fn with_library(presets: Arc<PresetLibrary>) -> Self {
        Self {
            presets,
            state: Mutex::new(State::default()),
        }
    }
}

#[uniffi::export]
impl ProfileStore {
    /// An empty store over the built-in presets.
    #[uniffi::constructor]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::with_library(PresetLibrary::shared_builtin()))
    }

    /// An empty store over another preset library.
    #[uniffi::constructor]
    pub fn with_presets(presets: Arc<PresetLibrary>) -> Arc<Self> {
        Arc::new(Self::with_library(presets))
    }

    /// Replaces the store with a saved `profiles.json` (or a bare array of
    /// profiles). Invalid or duplicate entries are skipped and reported; fails
    /// only when the text is not JSON of that shape.
    pub fn load_json(&self, json: String) -> Result<LoadReport> {
        let value: serde_json::Value =
            serde_json::from_str(&json).map_err(StreamInputError::json)?;
        let file: StoreFile = match value {
            serde_json::Value::Array(profiles) => StoreFile {
                schema: PROFILE_SCHEMA,
                revision: 0,
                profiles,
            },
            other => serde_util::strict::from_value(other)
                .map_err(|e| StreamInputError::json(format!("profiles file: {e}")))?,
        };
        let mut user: Vec<Profile> = Vec::new();
        let mut skipped = Vec::new();
        for (index, item) in file.profiles.into_iter().enumerate() {
            let p: Profile = match serde_util::strict::from_value(item) {
                Ok(p) => p,
                Err(e) => {
                    skipped.push(format!("profiles[{index}]: {e}"));
                    continue;
                }
            };
            if let Err(e) = profile::validate_profile(&p, false) {
                skipped.push(format!("profiles[{index}]: {e}"));
            } else if user.iter().any(|u| profiles_overlap(u, &p)) {
                skipped.push(format!(
                    "profiles[{index}]: {:?} duplicates an earlier profile",
                    p.app_key
                ));
            } else {
                user.push(p);
            }
        }
        let mut state = self.lock();
        state.revision = file.revision;
        state.user = user;
        Ok(LoadReport {
            loaded: u32::try_from(state.user.len()).unwrap_or(u32::MAX),
            revision: state.revision,
            skipped,
        })
    }

    /// The user's profiles and the revision as `profiles.json` (pretty-printed).
    pub fn save_json(&self) -> String {
        let state = self.lock();
        let file = serde_json::json!({
            "schema": PROFILE_SCHEMA,
            "revision": state.revision,
            "profiles": state.user,
        });
        serde_json::to_string_pretty(&file).unwrap_or_default()
    }

    /// The current revision.
    pub fn revision(&self) -> u64 {
        self.lock().revision
    }

    /// The user's (stored) profiles, in order.
    pub fn user_profiles(&self) -> Vec<Profile> {
        self.lock().user.clone()
    }

    /// The preset library.
    pub fn presets(&self) -> Arc<PresetLibrary> {
        self.presets.clone()
    }

    /// Every profile in effect, for `profilesSnapshot`: the user's, then each
    /// preset with a key no user profile covers.
    pub fn snapshot(&self) -> ProfilesSnapshotMessage {
        let state = self.lock();
        ProfilesSnapshotMessage {
            schema: PROFILE_SCHEMA,
            revision: state.revision,
            profiles: state.effective(&self.presets),
        }
    }

    /// The profile that applies to an app (see the module rules); `None` only
    /// when there is no `"*"` profile or preset either.
    pub fn resolve(&self, app_key: String) -> Option<Profile> {
        self.lock().resolve(&self.presets, &app_key).cloned()
    }

    /// The app's own profile (stored, else its preset) at the current revision,
    /// or `deleted` when it has neither; what a host answers for that app.
    pub fn current(&self, app_key: String) -> ProfileChangedMessage {
        self.lock().current(&self.presets, &app_key)
    }

    /// Stores a profile from a client (`profileUpsert`), by the merge rules.
    pub fn upsert(&self, profile: Profile) -> UpsertOutcome {
        self.lock().upsert(&self.presets, profile)
    }

    /// Forgets the user's profile for an app (`profileDelete`); its preset, if
    /// any, applies again.
    pub fn delete(&self, app_key: String) -> DeleteOutcome {
        self.lock().delete(&self.presets, &app_key)
    }

    /// First-connect migration: stores each profile for an app the store has no
    /// user profile for (newer than the app's preset, as for an upsert). Never
    /// replaces a stored profile. Returns the change messages, in order.
    pub fn import_if_missing(&self, profiles: Vec<Profile>) -> Vec<ProfileChangedMessage> {
        let mut state = self.lock();
        let mut out = Vec::new();
        for p in profiles {
            if state.user.iter().any(|u| profiles_overlap(u, &p)) {
                continue;
            }
            let outcome = state.upsert(&self.presets, p);
            if outcome.changed {
                out.push(outcome.message);
            }
        }
        out
    }

    /// Client mirror: replaces everything with a host snapshot (its profiles,
    /// presets included, become this mirror's stored list). Invalid entries are
    /// dropped.
    pub fn apply_snapshot(&self, snapshot: ProfilesSnapshotMessage) {
        let mut state = self.lock();
        state.revision = snapshot.revision;
        state.user = snapshot
            .profiles
            .into_iter()
            .filter(|p| profile::validate_profile(p, false).is_ok())
            .collect();
    }

    /// Client mirror: applies a `profileChanged`. Ignored (false) when it is
    /// older than the mirror's revision or carries an invalid profile.
    pub fn apply_changed(&self, message: ProfileChangedMessage) -> bool {
        let mut state = self.lock();
        if message.revision < state.revision {
            return false;
        }
        match (message.profile, message.deleted) {
            (Some(p), _) => {
                if profile::validate_profile(&p, false).is_err() {
                    return false;
                }
                match state.user.iter().position(|u| profiles_overlap(u, &p)) {
                    Some(i) => {
                        state.user.retain(|u| !profiles_overlap(u, &p));
                        let at = i.min(state.user.len());
                        state.user.insert(at, p);
                    }
                    None => state.user.push(p),
                }
            }
            (None, Some(key)) => {
                let every = key.trim() == profile::EVERY_OTHER_APP;
                state.user.retain(|p| {
                    if every {
                        !p.is_every_other_app()
                    } else {
                        p.is_every_other_app() || !p.applies_to(&key)
                    }
                });
            }
            (None, None) => return false,
        }
        state.revision = message.revision;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(key: &str, name: &str, at: &str) -> Profile {
        crate::profile::decode_profile(format!(
            r#"{{"schema":1,"appKey":"{key}","name":"{name}","updatedAt":"{at}","groups":[]}}"#
        ))
        .unwrap()
    }

    #[test]
    fn last_writer_wins_and_presets() {
        let store = ProfileStore::new();
        let o = store.upsert(p("org.krita", "Mine", "2026-05-01T00:00:00Z"));
        assert!(o.changed);
        assert_eq!(o.revision, 1);
        let o = store.upsert(p("ORG.KRITA", "Old", "2026-04-01T00:00:00Z"));
        assert_eq!(o.result, UpsertResult::Stale);
        assert_eq!(o.message.profile.unwrap().name, "Mine");
        // krita.exe is an alias of the preset the user's org.krita doesn't cover.
        assert_eq!(
            store.resolve("krita.exe".into()).unwrap().preset.as_deref(),
            Some("krita")
        );
        assert_eq!(store.resolve("org.kde.krita".into()).unwrap().name, "Krita");
        assert_eq!(store.resolve("org.krita".into()).unwrap().name, "Mine");
        let snap = store.snapshot();
        assert!(
            snap.profiles
                .iter()
                .any(|p| p.preset.as_deref() == Some("krita"))
        );
        // Unedited preset copy is not stored.
        let preset = store.presets().with_id("photoshop").unwrap().clone();
        let o = store.upsert(preset);
        assert_eq!(o.result, UpsertResult::Stale);
        assert!(!o.changed);
        // Migrated (2000-01-02) replaces a preset.
        let o = store.upsert(p(
            "com.adobe.Photoshop",
            "Imported",
            crate::profile::MIGRATED_UPDATED_AT,
        ));
        assert!(o.changed);
        let d = store.delete("photoshop.exe".into());
        assert!(!d.removed, "the import only covers com.adobe.Photoshop");
        let d = store.delete("com.adobe.photoshop".into());
        assert!(d.removed);
        assert_eq!(d.revealed_preset.as_deref(), Some("photoshop"));
        assert!(d.send_snapshot);
    }

    #[test]
    fn save_and_load() {
        let store = ProfileStore::new();
        store.upsert(p("a.exe", "A", "2026-05-01T00:00:00Z"));
        store.upsert(p("*", "Any", "2026-05-01T00:00:00Z"));
        let saved = store.save_json();
        let other = ProfileStore::new();
        let report = other
            .load_json(saved.replace("\"revision\": 2", "\"revision\": 9"))
            .unwrap();
        assert_eq!(report.loaded, 2);
        assert_eq!(other.revision(), 9);
        assert_eq!(other.resolve("x".into()).unwrap().name, "Any");
        let report = other
            .load_json(
                r#"[{"schema":1,"appKey":"","name":"","updatedAt":"x","groups":[]},{"appKey":3}]"#
                    .into(),
            )
            .unwrap();
        assert_eq!(report.loaded, 0);
        assert_eq!(report.skipped.len(), 2);
        assert!(other.load_json("{".into()).is_err());
    }

    #[test]
    fn import_never_overwrites() {
        let store = ProfileStore::new();
        store.upsert(p("org.krita", "Edited", "2026-05-01T00:00:00Z"));
        let msgs = store.import_if_missing(vec![
            p("org.krita", "Imported", "2030-01-01T00:00:00Z"),
            p(
                "com.apple.TextEdit",
                "TE",
                crate::profile::MIGRATED_UPDATED_AT,
            ),
        ]);
        assert_eq!(msgs.len(), 1);
        assert_eq!(store.resolve("org.krita".into()).unwrap().name, "Edited");
    }

    #[test]
    fn mirror_applies_changes() {
        let host = ProfileStore::new();
        let client = ProfileStore::new();
        client.apply_snapshot(host.snapshot());
        let o = host.upsert(p("org.krita", "Mine", "2026-05-01T00:00:00Z"));
        assert!(client.apply_changed(o.message));
        assert_eq!(client.resolve("org.krita".into()).unwrap().name, "Mine");
        let d = host.delete("org.krita".into());
        assert!(client.apply_changed(d.message));
        assert_eq!(client.resolve("org.krita".into()).unwrap().name, "Krita");
        assert!(!client.apply_changed(ProfileChangedMessage {
            revision: 0,
            profile: None,
            deleted: Some("x".into())
        }));
        assert_eq!(client.revision(), 2);
    }
}
