//! Host-owned logical sessions, independent of authenticated transport lifetime.
//! References are public identifiers, never bearer credentials: callers must pass
//! the fingerprint from the authenticated channel, not client-supplied metadata.
use rand::RngCore;
use std::{
    collections::BTreeMap,
    sync::Mutex,
    time::{Duration, Instant},
};

const DETACHED_TTL: Duration = Duration::from_secs(120);
const MAX_SESSIONS: usize = 256;
const MAX_PER_PEER: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct ResumeReference {
    pub daemon_id: String,
    pub session_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, uniffi::Record)]
pub struct SessionLease {
    pub reference: ResumeReference,
    pub generation: u64,
    pub resumed: bool,
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SessionError {
    #[error("invalid authenticated peer fingerprint or session reference")]
    InvalidIdentity,
    #[error("session belongs to another authenticated peer")]
    WrongPeer,
    #[error("session capacity exhausted")]
    Capacity,
    #[error("session generation exhausted")]
    GenerationExhausted,
    #[error("session state unavailable")]
    Unavailable,
}

struct Entry {
    peer: String,
    generation: u64,
    detached: Option<Instant>,
}
struct State {
    daemon_id: String,
    generation: u64,
    sessions: BTreeMap<String, Entry>,
}

#[derive(uniffi::Object)]
pub struct SessionDaemon {
    state: Mutex<State>,
}

impl Default for SessionDaemon {
    fn default() -> Self {
        Self::new()
    }
}

#[uniffi::export]
impl SessionDaemon {
    #[uniffi::constructor]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State {
                daemon_id: new_id(),
                generation: 0,
                sessions: BTreeMap::new(),
            }),
        }
    }

    /// Supersedes the prior lease atomically. No commands or input are replayed.
    pub fn attach(
        &self,
        peer_fingerprint: String,
        resume: Option<ResumeReference>,
    ) -> Result<SessionLease, SessionError> {
        self.attach_at(peer_fingerprint, resume, Instant::now())
    }

    /// Stale detach callbacks cannot disconnect a replacement transport.
    pub fn detach(
        &self,
        peer_fingerprint: String,
        lease: SessionLease,
    ) -> Result<bool, SessionError> {
        self.detach_at(&peer_fingerprint, &lease, Instant::now())
    }

    pub fn is_current(&self, peer_fingerprint: String, lease: SessionLease) -> bool {
        self.state
            .lock()
            .is_ok_and(|state| current(&state, &peer_fingerprint, &lease))
    }

    /// Runtime stop/restart and trust revocation invalidate every retained lease.
    pub fn reset(&self) -> Result<(), SessionError> {
        let mut state = self.state.lock().map_err(|_| SessionError::Unavailable)?;
        state.sessions.clear();
        state.daemon_id = new_id();
        Ok(())
    }
}

impl SessionDaemon {
    fn attach_at(
        &self,
        peer: String,
        resume: Option<ResumeReference>,
        now: Instant,
    ) -> Result<SessionLease, SessionError> {
        if peer.len() != 64
            || !peer
                .bytes()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(SessionError::InvalidIdentity);
        }
        if let Some(reference) = &resume {
            for value in [&reference.daemon_id, &reference.session_id] {
                if !uuid::Uuid::parse_str(value).is_ok_and(|id| !id.is_nil()) {
                    return Err(SessionError::InvalidIdentity);
                }
            }
        }
        let mut state = self.state.lock().map_err(|_| SessionError::Unavailable)?;
        state.sessions.retain(|_, entry| {
            entry
                .detached
                .is_none_or(|at| now.saturating_duration_since(at) < DETACHED_TTL)
        });
        let existing = resume
            .as_ref()
            .filter(|r| r.daemon_id.eq_ignore_ascii_case(&state.daemon_id))
            .map(|r| r.session_id.to_ascii_lowercase())
            .filter(|id| state.sessions.contains_key(id));
        if let Some(id) = &existing {
            if state.sessions[id].peer != peer {
                return Err(SessionError::WrongPeer);
            }
        } else if state.sessions.len() >= MAX_SESSIONS
            || state.sessions.values().filter(|e| e.peer == peer).count() >= MAX_PER_PEER
        {
            return Err(SessionError::Capacity);
        }
        let generation = state
            .generation
            .checked_add(1)
            .ok_or(SessionError::GenerationExhausted)?;
        state.generation = generation;
        let resumed = existing.is_some();
        let session_id = existing.unwrap_or_else(new_id);
        state.sessions.insert(
            session_id.clone(),
            Entry {
                peer,
                generation,
                detached: None,
            },
        );
        Ok(SessionLease {
            reference: ResumeReference {
                daemon_id: state.daemon_id.clone(),
                session_id,
            },
            generation,
            resumed,
        })
    }

    fn detach_at(
        &self,
        peer: &str,
        lease: &SessionLease,
        now: Instant,
    ) -> Result<bool, SessionError> {
        let mut state = self.state.lock().map_err(|_| SessionError::Unavailable)?;
        if !current(&state, peer, lease) {
            return Ok(false);
        }
        if let Some(entry) = state.sessions.get_mut(&lease.reference.session_id) {
            entry.detached = Some(now);
        }
        Ok(true)
    }
}

fn current(state: &State, peer: &str, lease: &SessionLease) -> bool {
    state.daemon_id == lease.reference.daemon_id
        && state
            .sessions
            .get(&lease.reference.session_id)
            .is_some_and(|entry| {
                entry.peer == peer
                    && entry.generation == lease.generation
                    && entry.detached.is_none()
            })
}

/// A random version-4 UUID string for session, request and attempt ids.
pub fn new_id() -> String {
    let mut bytes = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    uuid::Uuid::from_bytes(bytes).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    fn peer(n: u64) -> String {
        format!("{n:064x}")
    }
    #[test]
    fn resume_is_peer_bound_and_old_detach_cannot_revoke_replacement() {
        let daemon = SessionDaemon::new();
        let first = daemon.attach(peer(1), None).unwrap();
        assert!(!first.resumed);
        assert!(matches!(
            daemon.attach(peer(2), Some(first.reference.clone())),
            Err(SessionError::WrongPeer)
        ));
        let next = daemon
            .attach(peer(1), Some(first.reference.clone()))
            .unwrap();
        assert!(next.resumed && next.generation > first.generation);
        assert_eq!(next.reference, first.reference);
        assert!(!daemon.is_current(peer(1), first.clone()));
        assert!(!daemon.detach(peer(1), first).unwrap());
        assert!(daemon.is_current(peer(1), next.clone()));
        assert!(!daemon.is_current(peer(2), next));
    }
    #[test]
    fn detached_expiry_and_restart_produce_fresh_sessions() {
        let daemon = SessionDaemon::new();
        let now = Instant::now();
        let first = daemon.attach_at(peer(1), None, now).unwrap();
        daemon.detach_at(&peer(1), &first, now).unwrap();
        let resumed = daemon
            .attach_at(
                peer(1),
                Some(first.reference),
                now + DETACHED_TTL - Duration::from_millis(1),
            )
            .unwrap();
        assert!(resumed.resumed);
        daemon.detach_at(&peer(1), &resumed, now).unwrap();
        let expired = daemon
            .attach_at(peer(1), Some(resumed.reference), now + DETACHED_TTL)
            .unwrap();
        assert!(!expired.resumed);
        daemon.reset().unwrap();
        assert!(!daemon.is_current(peer(1), expired.clone()));
        let fresh = daemon
            .attach(peer(1), Some(expired.reference.clone()))
            .unwrap();
        assert!(!fresh.resumed);
        assert_ne!(fresh.reference.daemon_id, expired.reference.daemon_id);
    }
    #[test]
    fn capacity_is_bounded_per_peer_and_globally() {
        let daemon = SessionDaemon::new();
        let now = Instant::now();
        let mut detached = Vec::new();
        for n in 0..MAX_SESSIONS {
            if n == MAX_PER_PEER {
                assert!(matches!(
                    daemon.attach_at(peer(0), None, now),
                    Err(SessionError::Capacity)
                ));
            }
            detached.push((
                peer((n / MAX_PER_PEER) as u64),
                daemon
                    .attach_at(peer((n / MAX_PER_PEER) as u64), None, now)
                    .unwrap(),
            ));
        }
        assert!(matches!(
            daemon.attach(peer(0), None),
            Err(SessionError::Capacity)
        ));
        assert!(matches!(
            daemon.attach(peer(999), None),
            Err(SessionError::Capacity)
        ));
        for (peer, lease) in detached {
            daemon.detach_at(&peer, &lease, now).unwrap();
        }
        assert!(
            daemon
                .attach_at(peer(999), None, now + DETACHED_TTL)
                .is_ok()
        );
        assert!(daemon.attach("claimed device UUID".into(), None).is_err());
    }
    #[test]
    fn concurrent_resumes_have_exactly_one_current_generation() {
        let daemon = std::sync::Arc::new(SessionDaemon::new());
        let first = daemon.attach(peer(1), None).unwrap();
        let workers: Vec<_> = (0..20)
            .map(|_| {
                let daemon = daemon.clone();
                let reference = first.reference.clone();
                std::thread::spawn(move || daemon.attach(peer(1), Some(reference)).unwrap())
            })
            .collect();
        let leases: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(
            leases
                .iter()
                .filter(|lease| daemon.is_current(peer(1), (*lease).clone()))
                .count(),
            1
        );
        for lease in &leases {
            if lease.generation != leases.iter().map(|l| l.generation).max().unwrap() {
                assert!(!daemon.detach(peer(1), lease.clone()).unwrap());
            }
        }
        assert!(!daemon.detach(peer(1), first).unwrap());
    }
}

uniffi::setup_scaffolding!();
