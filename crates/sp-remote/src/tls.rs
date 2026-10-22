//! QUIC/TLS 1.3 configuration with self-signed identities.
//!
//! There is no certificate authority. TLS proves that each side holds the key for the
//! certificate it presents; who may connect is decided by pinned fingerprints, and an unknown
//! peer must first pair (SPAKE2 over the connection, bound to both fingerprints).
//! The server therefore accepts any client certificate at the TLS layer. The client accepts any
//! server certificate only while pairing, and otherwise only the pinned one.

use std::{sync::Arc, time::Duration};

use rustls::{
    DigitallySignedStruct, DistinguishedName, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature},
    pki_types::{CertificateDer, ServerName, UnixTime},
    server::danger::{ClientCertVerified, ClientCertVerifier},
};

use crate::{Fingerprint, Identity, RemoteError};

/// ALPN of this protocol version.
pub(crate) const ALPN: &[u8] = b"superposition-remote/1";

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

#[derive(Debug)]
struct AnyClientCertificate(Arc<CryptoProvider>);

impl ClientCertVerifier for AnyClientCertificate {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[derive(Debug)]
struct PinnedServer {
    provider: Arc<CryptoProvider>,
    /// `None` while pairing; pairing then binds whichever certificate was presented.
    expected: Option<Fingerprint>,
}

impl ServerCertVerifier for PinnedServer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.expected {
            Some(expected) if Fingerprint::of(end_entity) != expected => Err(
                rustls::Error::General("the engine's certificate is not the paired one".into()),
            ),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn transport() -> Arc<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.keep_alive_interval(Some(Duration::from_secs(2)));
    transport.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(10)).expect("small timeout"),
    ));
    Arc::new(transport)
}

pub(crate) fn server_config(identity: &Identity) -> Result<quinn::ServerConfig, RemoteError> {
    let provider = provider();
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(AnyClientCertificate(provider)))
        .with_single_cert(vec![identity.certificate.clone()], identity.key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)
        .map_err(|error| RemoteError::Tls(error.to_string()))?;
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    config.transport_config(transport());
    Ok(config)
}

pub(crate) fn client_config(
    identity: &Identity,
    expected: Option<Fingerprint>,
) -> Result<quinn::ClientConfig, RemoteError> {
    let provider = provider();
    let mut tls = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedServer { provider, expected }))
        .with_client_auth_cert(vec![identity.certificate.clone()], identity.key())?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto = quinn::crypto::rustls::QuicClientConfig::try_from(tls)
        .map_err(|error| RemoteError::Tls(error.to_string()))?;
    let mut config = quinn::ClientConfig::new(Arc::new(crypto));
    config.transport_config(transport());
    Ok(config)
}

/// The fingerprint of the certificate the peer presented on this connection.
pub(crate) fn peer_fingerprint(connection: &quinn::Connection) -> Option<Fingerprint> {
    let identity = connection.peer_identity()?;
    let certificates = identity.downcast::<Vec<CertificateDer<'static>>>().ok()?;
    certificates
        .first()
        .map(|certificate| Fingerprint::of(certificate))
}
