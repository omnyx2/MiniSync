//! TLS layer: self-signed certificates + rustls config for P2P encryption.
//!
//! P2P이므로 CA 검증 생략 (NoVerifier). 모든 인증서 수용.

use anyhow::Result;
use rcgen::generate_simple_self_signed;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, ServerConfig, SignatureScheme, StreamOwned};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

/// Server/Client TLS stream을 하나의 타입으로 통합.
pub enum TlsStream {
    Server(StreamOwned<rustls::ServerConnection, TcpStream>),
    Client(StreamOwned<rustls::ClientConnection, TcpStream>),
}

impl TlsStream {
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> std::io::Result<()> {
        match self {
            TlsStream::Server(s) => s.get_ref().set_read_timeout(dur),
            TlsStream::Client(s) => s.get_ref().set_read_timeout(dur),
        }
    }
}

impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            TlsStream::Server(s) => s.read(buf),
            TlsStream::Client(s) => s.read(buf),
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            TlsStream::Server(s) => s.write(buf),
            TlsStream::Client(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            TlsStream::Server(s) => s.flush(),
            TlsStream::Client(s) => s.flush(),
        }
    }
}

// ── cert generation ─────────────────────────────────────────────────────────

/// 자체서명 인증서 + 개인키 생성.
pub fn generate_self_signed() -> Result<(CertificateDer<'static>, PrivatePkcs8KeyDer<'static>)> {
    let ck = generate_simple_self_signed(vec!["localhost".to_string()])?;
    let cert_der = CertificateDer::from(ck.cert.der().to_vec());
    let key_der = PrivatePkcs8KeyDer::from(ck.key_pair.serialize_der());
    Ok((cert_der, key_der))
}

// ── TLS configs ─────────────────────────────────────────────────────────────

pub fn server_config(
    cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
) -> Result<Arc<ServerConfig>> {
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert], key.into())?;
    Ok(Arc::new(config))
}

pub fn client_config() -> Arc<ClientConfig> {
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerifier))
        .with_no_client_auth();
    Arc::new(config)
}

// ── handshake wrappers ──────────────────────────────────────────────────────

pub fn accept_tls(tcp: TcpStream, config: Arc<ServerConfig>) -> Result<TlsStream> {
    let conn = rustls::ServerConnection::new(config)?;
    Ok(TlsStream::Server(StreamOwned::new(conn, tcp)))
}

pub fn connect_tls(tcp: TcpStream, config: Arc<ClientConfig>) -> Result<TlsStream> {
    let name = ServerName::try_from("localhost")
        .map_err(|e| anyhow::anyhow!("invalid server name: {e}"))?
        .to_owned();
    let conn = rustls::ClientConnection::new(config, name)?;
    Ok(TlsStream::Client(StreamOwned::new(conn, tcp)))
}

// ── NoVerifier (P2P: accept any certificate) ────────────────────────────────

#[derive(Debug)]
struct NoVerifier;

impl ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ED25519,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
        ]
    }
}
