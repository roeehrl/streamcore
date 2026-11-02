//! One-use, expiring PIN pairing. OPAQUE authenticates both certificate pins in
//! its context; a recording of the exchange does not expose a PIN verifier.
//! Registration happens entirely on the host when the user opens pairing.

use hmac::{Hmac, Mac};
use opaque_ke::{
    CipherSuite, ClientLogin, ClientLoginFinishParameters, ClientRegistration,
    ClientRegistrationFinishParameters, CredentialFinalization, CredentialRequest,
    CredentialResponse, ServerLogin, ServerLoginParameters, ServerRegistration, ServerSetup,
};
use rand::{Rng, rngs::OsRng};
use sha2::{Digest, Sha256, Sha512};
use std::{
    collections::BTreeMap,
    sync::Mutex,
    time::{Duration, Instant},
};
use zeroize::Zeroizing;

use crate::identity::validate_certificate;

const PAIRING_LIFETIME: Duration = Duration::from_secs(120);
const ATTEMPT_LIMIT: u32 = 5;
const MAX_MESSAGE: usize = 4096;
const CREDENTIAL: &[u8] = b"mycnc-pairing-pin-v1";
const CONFIRMATION: &[u8] = b"mycnc-pairing-persisted-v1";

struct PairingSuite;
impl CipherSuite for PairingSuite {
    type OprfCs = opaque_ke::Ristretto255;
    type KeyExchange = opaque_ke::TripleDh<opaque_ke::Ristretto255, Sha512>;
    type Ksf = argon2::Argon2<'static>;
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum PairingError {
    #[error("invalid pairing message or certificate")]
    InvalidMessage,
    #[error("pairing code must contain six digits")]
    InvalidCode,
    #[error("pairing expired; open a new pairing window on the host")]
    Expired,
    #[error("pairing attempt limit reached; open a new pairing window on the host")]
    Locked,
    #[error("pairing failed")]
    Authentication,
    #[error("pairing has already finished or been canceled")]
    Finished,
    #[error("pairing state is unavailable")]
    Unavailable,
}

struct HostAttempt {
    state: ServerLogin<PairingSuite>,
    certificate: Vec<u8>,
    context: Vec<u8>,
}

struct WindowState {
    used: bool,
    started: u32,
    attempts: BTreeMap<u32, HostAttempt>,
}

#[derive(Clone, uniffi::Record)]
pub struct PairingChallenge {
    pub attempt: u32,
    pub response: Vec<u8>,
}

#[derive(Clone, uniffi::Record)]
pub struct VerifiedPairing {
    pub peer_certificate: Vec<u8>,
    /// Return to the client only AFTER persisting peer_certificate successfully.
    pub persisted_confirmation: Vec<u8>,
}

#[derive(uniffi::Object)]
pub struct PairingWindow {
    code: Zeroizing<String>,
    certificate: Vec<u8>,
    setup: ServerSetup<PairingSuite>,
    record: ServerRegistration<PairingSuite>,
    expires: Instant,
    state: Mutex<WindowState>,
}

#[uniffi::export]
impl PairingWindow {
    #[uniffi::constructor]
    pub fn new(certificate: Vec<u8>) -> Result<Self, PairingError> {
        Self::with_lifetime(certificate, PAIRING_LIFETIME)
    }

    /// Display locally, or include in the user's QR/headless invitation. Never
    /// transmit the code over discovery or the pairing network connection.
    pub fn display_code(&self) -> Result<String, PairingError> {
        let state = self.state.lock().map_err(|_| PairingError::Unavailable)?;
        self.check(&state)?;
        Ok(self.code.to_string())
    }

    pub fn begin(
        &self,
        client_certificate: Vec<u8>,
        request: Vec<u8>,
    ) -> Result<PairingChallenge, PairingError> {
        let mut state = self.state.lock().map_err(|_| PairingError::Unavailable)?;
        self.check(&state)?;
        if state.started >= ATTEMPT_LIMIT {
            return Err(PairingError::Locked);
        }
        // Reserve BEFORE parsing or responding; disconnects and malformed
        // requests cannot reset the global budget of this pairing window.
        state.started += 1;
        let attempt = state.started;
        validate_certificate(&client_certificate.clone().into())
            .map_err(|_| PairingError::InvalidMessage)?;
        bounded(&request)?;
        let request =
            CredentialRequest::deserialize(&request).map_err(|_| PairingError::InvalidMessage)?;
        let context = context(&self.certificate, &client_certificate);
        let result = ServerLogin::start(
            &mut OsRng,
            &self.setup,
            Some(self.record.clone()),
            request,
            CREDENTIAL,
            ServerLoginParameters {
                context: Some(&context),
                ..Default::default()
            },
        )
        .map_err(|_| PairingError::Authentication)?;
        state.attempts.insert(
            attempt,
            HostAttempt {
                state: result.state,
                certificate: client_certificate,
                context,
            },
        );
        Ok(PairingChallenge {
            attempt,
            response: result.message.serialize().to_vec(),
        })
    }

    pub fn finish(
        &self,
        attempt: u32,
        finalization: Vec<u8>,
    ) -> Result<VerifiedPairing, PairingError> {
        let mut state = self.state.lock().map_err(|_| PairingError::Unavailable)?;
        self.check(&state)?;
        let pending = state
            .attempts
            .remove(&attempt)
            .ok_or(PairingError::Finished)?;
        bounded(&finalization)?;
        let finalization = CredentialFinalization::deserialize(&finalization)
            .map_err(|_| PairingError::InvalidMessage)?;
        let result = pending
            .state
            .finish(
                finalization,
                ServerLoginParameters {
                    context: Some(&pending.context),
                    ..Default::default()
                },
            )
            .map_err(|_| PairingError::Authentication)?;
        let confirmation = confirmation(&result.session_key, &pending.context)?;
        state.used = true;
        state.attempts.clear();
        Ok(VerifiedPairing {
            peer_certificate: pending.certificate,
            persisted_confirmation: confirmation,
        })
    }

    pub fn cancel(&self) -> Result<(), PairingError> {
        let mut state = self.state.lock().map_err(|_| PairingError::Unavailable)?;
        state.used = true;
        state.attempts.clear();
        Ok(())
    }
}

impl PairingWindow {
    pub(crate) fn host_certificate(&self) -> Vec<u8> {
        self.certificate.clone()
    }

    fn with_lifetime(certificate: Vec<u8>, lifetime: Duration) -> Result<Self, PairingError> {
        validate_certificate(&certificate.clone().into())
            .map_err(|_| PairingError::InvalidMessage)?;
        let code = Zeroizing::new(format!("{:06}", OsRng.gen_range(0..1_000_000_u32)));
        let setup = ServerSetup::new(&mut OsRng);
        let client = ClientRegistration::<PairingSuite>::start(&mut OsRng, code.as_bytes())
            .map_err(|_| PairingError::Authentication)?;
        let server = ServerRegistration::start(&setup, client.message, CREDENTIAL)
            .map_err(|_| PairingError::Authentication)?;
        let finish = client
            .state
            .finish(
                &mut OsRng,
                code.as_bytes(),
                server.message,
                ClientRegistrationFinishParameters::default(),
            )
            .map_err(|_| PairingError::Authentication)?;
        Ok(Self {
            code,
            certificate,
            setup,
            record: ServerRegistration::finish(finish.message),
            expires: Instant::now() + lifetime,
            state: Mutex::new(WindowState {
                used: false,
                started: 0,
                attempts: BTreeMap::new(),
            }),
        })
    }
    fn check(&self, state: &WindowState) -> Result<(), PairingError> {
        if state.used {
            return Err(PairingError::Finished);
        }
        if Instant::now() >= self.expires {
            return Err(PairingError::Expired);
        }
        Ok(())
    }
}

struct ClientState {
    login: Option<ClientLogin<PairingSuite>>,
    session_key: Option<Zeroizing<Vec<u8>>>,
}

#[derive(uniffi::Object)]
pub struct PairingClient {
    code: Zeroizing<String>,
    host_certificate: Vec<u8>,
    context: Vec<u8>,
    request: Vec<u8>,
    expires: Instant,
    state: Mutex<ClientState>,
}

#[uniffi::export]
impl PairingClient {
    #[uniffi::constructor]
    pub fn new(
        code: String,
        host_certificate: Vec<u8>,
        client_certificate: Vec<u8>,
    ) -> Result<Self, PairingError> {
        let code = Zeroizing::new(code);
        if code.len() != 6 || !code.bytes().all(|c| c.is_ascii_digit()) {
            return Err(PairingError::InvalidCode);
        }
        for cert in [&host_certificate, &client_certificate] {
            validate_certificate(&cert.clone().into()).map_err(|_| PairingError::InvalidMessage)?;
        }
        let start = ClientLogin::start(&mut OsRng, code.as_bytes())
            .map_err(|_| PairingError::Authentication)?;
        Ok(Self {
            code,
            context: context(&host_certificate, &client_certificate),
            host_certificate,
            request: start.message.serialize().to_vec(),
            expires: Instant::now() + PAIRING_LIFETIME,
            state: Mutex::new(ClientState {
                login: Some(start.state),
                session_key: None,
            }),
        })
    }
    pub fn request(&self) -> Vec<u8> {
        self.request.clone()
    }

    pub fn finish(&self, response: Vec<u8>) -> Result<Vec<u8>, PairingError> {
        let mut state = self.state.lock().map_err(|_| PairingError::Unavailable)?;
        let login = state.login.take().ok_or(PairingError::Finished)?;
        if Instant::now() >= self.expires {
            return Err(PairingError::Expired);
        }
        bounded(&response)?;
        let response =
            CredentialResponse::deserialize(&response).map_err(|_| PairingError::InvalidMessage)?;
        let result = login
            .finish(
                &mut OsRng,
                self.code.as_bytes(),
                response,
                ClientLoginFinishParameters {
                    context: Some(&self.context),
                    ..Default::default()
                },
            )
            .map_err(|_| PairingError::Authentication)?;
        state.session_key = Some(Zeroizing::new(result.session_key.to_vec()));
        Ok(result.message.serialize().to_vec())
    }

    /// Releases the authenticated host pin only after the host acknowledges
    /// persistence. The caller must then persist it before reporting paired.
    pub fn confirm_persisted(&self, confirmation: Vec<u8>) -> Result<Vec<u8>, PairingError> {
        let mut state = self.state.lock().map_err(|_| PairingError::Unavailable)?;
        let key = state.session_key.take().ok_or(PairingError::Finished)?;
        if Instant::now() >= self.expires {
            return Err(PairingError::Expired);
        }
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&key).map_err(|_| PairingError::Authentication)?;
        mac.update(CONFIRMATION);
        mac.update(&self.context);
        mac.verify_slice(&confirmation)
            .map_err(|_| PairingError::Authentication)?;
        Ok(self.host_certificate.clone())
    }
}

fn bounded(message: &[u8]) -> Result<(), PairingError> {
    if message.is_empty() || message.len() > MAX_MESSAGE {
        Err(PairingError::InvalidMessage)
    } else {
        Ok(())
    }
}

fn context(host: &[u8], client: &[u8]) -> Vec<u8> {
    let mut context = b"mycnc-pairing-v1:host:client:".to_vec();
    context.extend_from_slice(&Sha256::digest(host));
    context.extend_from_slice(&Sha256::digest(client));
    context
}

fn confirmation(key: &[u8], context: &[u8]) -> Result<Vec<u8>, PairingError> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).map_err(|_| PairingError::Authentication)?;
    mac.update(CONFIRMATION);
    mac.update(context);
    Ok(mac.finalize().into_bytes().to_vec())
}

#[cfg(test)]
mod expiry_tests {
    use super::*;
    #[test]
    fn expired_window_cannot_reveal_code_or_accept_requests() {
        let identity = crate::identity::PeerIdentity::generate().unwrap();
        let window = PairingWindow::with_lifetime(identity.certificate(), Duration::ZERO).unwrap();
        assert!(matches!(window.display_code(), Err(PairingError::Expired)));
        assert!(matches!(
            window.begin(vec![], vec![]),
            Err(PairingError::Expired)
        ));
    }
}
