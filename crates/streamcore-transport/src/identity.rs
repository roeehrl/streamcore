//! Pinned mutual TLS identities. Native shells persist the exported identity in
//! their OS credential store; the core never writes private keys to a file.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

pub const SESSION_ALPN: &[u8] = b"streamcore/2";
/// The name every peer certificate carries. It identifies nothing (the whole
/// self-signed certificate is pinned); it only has to be the same on both ends.
pub const PEER_DNS_NAME: &str = "peer.streamcore.invalid";
const MAX_IDENTITY_BYTES: usize = 32_768;
const MAX_CERTIFICATE_BYTES: usize = 8_192;
const MAX_TRUSTED_PEERS: usize = 256;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum IdentityError {
    #[error("invalid identity or certificate")]
    InvalidIdentity,
    #[error("invalid trusted peer set")]
    InvalidTrust,
    #[error("cryptographic operation failed")]
    Crypto,
    /// The certificate does not carry [`PEER_DNS_NAME`]: it was made before the
    /// wire names changed. It can never complete a handshake with this version,
    /// so this is reported up front. An identity of one's own is replaced with
    /// a new one ([`PeerIdentity::generate`]); a peer's certificate is removed.
    /// Either way the two devices pair again. A listener's trust set is the
    /// exception: [`PeerIdentity::server_config`] skips such peers.
    #[error("the certificate predates the current wire names: pair again")]
    LegacyIdentity,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredIdentity {
    version: u32,
    certificate: Vec<u8>,
    private_key: Vec<u8>,
}

impl Drop for StoredIdentity {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.private_key.zeroize();
    }
}

/// No Debug/Serialize implementation: private material must never reach logs.
#[derive(uniffi::Object)]
pub struct PeerIdentity {
    certificate: CertificateDer<'static>,
    private_key: PrivateKeyDer<'static>,
}

#[uniffi::export]
impl PeerIdentity {
    #[uniffi::constructor]
    pub fn generate() -> Result<Self, IdentityError> {
        let key = rcgen::KeyPair::generate().map_err(|_| IdentityError::Crypto)?;
        let mut params = rcgen::CertificateParams::new(vec![PEER_DNS_NAME.into()])
            .map_err(|_| IdentityError::Crypto)?;
        params.key_usages = vec![rcgen::KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![
            rcgen::ExtendedKeyUsagePurpose::ServerAuth,
            rcgen::ExtendedKeyUsagePurpose::ClientAuth,
        ];
        // Pairwise, explicitly revoked identities are long lived. The entire
        // self-signed certificate is pinned, never a system/public CA or a name.
        let certificate = params
            .self_signed(&key)
            .map_err(|_| IdentityError::Crypto)?;
        Self::from_parts(certificate.der().clone(), key.serialize_der())
    }

    #[uniffi::constructor]
    pub fn restore(protected_data: Vec<u8>) -> Result<Self, IdentityError> {
        let protected_data = Zeroizing::new(protected_data);
        if protected_data.len() > MAX_IDENTITY_BYTES {
            return Err(IdentityError::InvalidIdentity);
        }
        let stored: StoredIdentity =
            serde_json::from_slice(&protected_data).map_err(|_| IdentityError::InvalidIdentity)?;
        if stored.version != 1 {
            return Err(IdentityError::InvalidIdentity);
        }
        Self::from_parts(
            stored.certificate.clone().into(),
            stored.private_key.clone(),
        )
    }

    /// Contains private material. The native caller must store this only in its
    /// OS credential store and release its temporary byte buffer after saving.
    pub fn export_for_secure_storage(&self) -> Result<Vec<u8>, IdentityError> {
        serde_json::to_vec(&StoredIdentity {
            version: 1,
            certificate: self.certificate.to_vec(),
            private_key: self.private_key.secret_der().to_vec(),
        })
        .map_err(|_| IdentityError::Crypto)
    }

    pub fn certificate(&self) -> Vec<u8> {
        self.certificate.to_vec()
    }

    pub fn fingerprint(&self) -> String {
        certificate_fingerprint(&self.certificate)
    }
}

impl PeerIdentity {
    fn from_parts(
        certificate: CertificateDer<'static>,
        private_key: Vec<u8>,
    ) -> Result<Self, IdentityError> {
        validate_certificate(&certificate)?;
        let private_key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(private_key));
        let provider = rustls::crypto::ring::default_provider();
        rustls::sign::CertifiedKey::from_der(
            vec![certificate.clone()],
            private_key.clone_key(),
            &provider,
        )
        .map_err(|_| IdentityError::InvalidIdentity)?;
        Ok(Self {
            certificate,
            private_key,
        })
    }

    pub fn client_config(&self, peer: Vec<u8>) -> Result<Arc<rustls::ClientConfig>, IdentityError> {
        let cert: CertificateDer<'static> = peer.into();
        let roots = roots_for(std::slice::from_ref(&cert))?;
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier =
            rustls::client::WebPkiServerVerifier::builder_with_provider(roots, provider.clone())
                .build()
                .map_err(|_| IdentityError::InvalidTrust)?;
        let mut config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|_| IdentityError::Crypto)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedServer { cert, verifier }))
            .with_client_auth_cert(vec![self.certificate.clone()], self.private_key.clone_key())
            .map_err(|_| IdentityError::InvalidIdentity)?;
        config.alpn_protocols = vec![SESSION_ALPN.to_vec()];
        config.enable_early_data = false;
        // Every connection proves the current pin. Resumption must not bypass a
        // revoked peer or retain a session after changing the trust set.
        config.resumption = rustls::client::Resumption::disabled();
        Ok(Arc::new(config))
    }

    pub fn server_config(
        &self,
        peers: Vec<Vec<u8>>,
    ) -> Result<Arc<rustls::ServerConfig>, IdentityError> {
        if peers.len() > MAX_TRUSTED_PEERS {
            return Err(IdentityError::InvalidTrust);
        }
        // A peer certificate from before the wire names changed can never
        // complete a handshake, so leaving it out changes no trust decision. It
        // is skipped, not failed on: one stale entry must not stop a host from
        // listening. `legacy_certificate_count` tells a consumer how many of its
        // peers must pair again. A malformed certificate still fails.
        let mut certs: Vec<CertificateDer<'static>> = Vec::with_capacity(peers.len());
        for peer in peers {
            let certificate = CertificateDer::from(peer);
            match validate_certificate(&certificate) {
                Ok(()) => certs.push(certificate),
                Err(IdentityError::LegacyIdentity) => {}
                Err(error) => return Err(error),
            }
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let verifier: Arc<dyn ClientCertVerifier> = if certs.is_empty() {
            // First launch can listen before pairing. Empty trust is mandatory
            // client authentication with no approved peer, never anonymous TLS.
            Arc::new(NoTrustedClients)
        } else {
            let roots = roots_for(&certs)?;
            let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
                roots,
                provider.clone(),
            )
            .build()
            .map_err(|_| IdentityError::InvalidTrust)?;
            Arc::new(PinnedClients { certs, verifier })
        };
        let mut config = rustls::ServerConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|_| IdentityError::Crypto)?
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![self.certificate.clone()], self.private_key.clone_key())
            .map_err(|_| IdentityError::InvalidIdentity)?;
        config.alpn_protocols = vec![SESSION_ALPN.to_vec()];
        config.max_early_data_size = 0;
        config.send_tls13_tickets = 0;
        config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
        Ok(Arc::new(config))
    }
}

pub fn certificate_fingerprint(certificate: &[u8]) -> String {
    use std::fmt::Write;
    let mut text = String::with_capacity(64);
    for byte in Sha256::digest(certificate) {
        // Formatting a byte into a String cannot fail.
        let _ = write!(&mut text, "{byte:02x}");
    }
    text
}

/// How many of `certificates` were made before the wire names changed: peers a
/// listener skips ([`PeerIdentity::server_config`]) and that must pair again.
/// Malformed entries are not counted.
#[uniffi::export]
pub fn legacy_certificate_count(certificates: Vec<Vec<u8>>) -> u32 {
    let legacy = certificates
        .into_iter()
        .filter(|certificate| {
            matches!(
                validate_certificate(&CertificateDer::from(certificate.clone())),
                Err(IdentityError::LegacyIdentity)
            )
        })
        .count();
    u32::try_from(legacy).unwrap_or(u32::MAX)
}

#[uniffi::export]
pub fn validate_peer_certificate(certificate: Vec<u8>) -> Result<String, IdentityError> {
    validate_certificate(&certificate.clone().into())?;
    Ok(certificate_fingerprint(&certificate))
}

pub fn validate_certificate(certificate: &CertificateDer<'_>) -> Result<(), IdentityError> {
    if certificate.is_empty() || certificate.len() > MAX_CERTIFICATE_BYTES {
        return Err(IdentityError::InvalidIdentity);
    }
    rustls::RootCertStore::empty()
        .add(certificate.clone())
        .map_err(|_| IdentityError::InvalidIdentity)?;
    // A well-formed certificate under another name is one from before the wire
    // names changed. Said here, where it is loaded, instead of as a handshake
    // that fails for no visible reason.
    let parsed = rustls::server::ParsedCertificate::try_from(certificate)
        .map_err(|_| IdentityError::InvalidIdentity)?;
    let name = ServerName::try_from(PEER_DNS_NAME).map_err(|_| IdentityError::Crypto)?;
    rustls::client::verify_server_name(&parsed, &name).map_err(|_| IdentityError::LegacyIdentity)
}

fn roots_for(
    certs: &[CertificateDer<'static>],
) -> Result<Arc<rustls::RootCertStore>, IdentityError> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        validate_certificate(cert)?;
        roots
            .add(cert.clone())
            .map_err(|_| IdentityError::InvalidTrust)?;
    }
    Ok(Arc::new(roots))
}

fn untrusted() -> rustls::Error {
    rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer)
}

#[derive(Debug)]
struct PinnedServer {
    cert: CertificateDer<'static>,
    verifier: Arc<rustls::client::WebPkiServerVerifier>,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end != &self.cert || !intermediates.is_empty() {
            return Err(untrusted());
        }
        self.verifier
            .verify_server_cert(end, intermediates, name, ocsp, now)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verifier.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verifier.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.verifier.supported_verify_schemes()
    }
}

#[derive(Debug)]
struct PinnedClients {
    certs: Vec<CertificateDer<'static>>,
    verifier: Arc<dyn ClientCertVerifier>,
}

impl ClientCertVerifier for PinnedClients {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.verifier.root_hint_subjects()
    }
    fn verify_client_cert(
        &self,
        end: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        if !self.certs.contains(end) || !intermediates.is_empty() {
            return Err(untrusted());
        }
        self.verifier.verify_client_cert(end, intermediates, now)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verifier.verify_tls12_signature(message, cert, dss)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verifier.verify_tls13_signature(message, cert, dss)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.verifier.supported_verify_schemes()
    }
}

#[derive(Debug)]
struct NoTrustedClients;
impl ClientCertVerifier for NoTrustedClients {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }
    fn verify_client_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Err(untrusted())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(untrusted())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Err(untrusted())
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
