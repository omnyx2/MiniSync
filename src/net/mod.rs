//! TLS layer: self-signed certificates + rustls config for P2P encryption.
//!
//! P2P이므로 CA 검증 생략 (NoVerifier). 모든 인증서 수용.

pub mod discovery;
pub mod peers;

use anyhow::Result;
use rcgen::generate_simple_self_signed;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::{ClientConfig, Connection, DigitallySignedStruct, ServerConfig, SignatureScheme};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

/// Server/Client TLS stream을 하나의 타입으로 통합.
///
/// 저수준 `rustls::Connection` + `TcpStream`을 직접 보유한다(`StreamOwned` 대신).
/// 이렇게 하면 read_tls/write_tls/process_new_packets를 명시적으로 호출해
/// 단일 스레드 논블로킹 full-duplex 펌프를 구현할 수 있다 — reader와 writer가
/// 하나의 뮤텍스를 공유하다 교착되는 문제를 근본적으로 제거한다.
pub struct TlsStream {
    pub conn: Connection,
    pub sock: TcpStream,
}

impl TlsStream {
    /// 소켓 논블로킹 모드 토글. Hello 교환(블로킹) 후 펌프 루프 진입 직전에 켠다.
    pub fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        self.sock.set_nonblocking(nonblocking)
    }
}

// Hello 단계용 블로킹 Read/Write. (소켓이 블로킹 상태일 때만 사용)
impl Read for TlsStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            // 이미 복호화된 평문이 있으면 반환. Ok(0)은 clean EOF(close_notify).
            match self.conn.reader().read(buf) {
                Ok(n) => return Ok(n),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
            // 평문이 없으니 TLS를 더 펌프한다. 먼저 보낼 게 있으면 보낸다(handshake).
            while self.conn.wants_write() {
                self.conn.write_tls(&mut self.sock)?;
            }
            let n = self.conn.read_tls(&mut self.sock)?;
            if n == 0 {
                return Ok(0); // TCP EOF
            }
            self.conn
                .process_new_packets()
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        }
    }
}

impl Write for TlsStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        // 평문을 rustls 송신 버퍼에 적재(소켓 I/O 없음, 블로킹 안 함).
        self.conn.writer().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.conn.writer().flush()?;
        while self.conn.wants_write() {
            self.conn.write_tls(&mut self.sock)?;
        }
        self.sock.flush()
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
    let mut conn = rustls::ServerConnection::new(config)?;
    // 송신 평문 버퍼 상한 해제. 기본(~64KB)이면 큰 File 메시지를 writer에 한 번에
    // 적재할 때 WriteZero("failed to write whole buffer")가 나서 세션이 죽는다.
    conn.set_buffer_limit(None);
    Ok(TlsStream {
        conn: Connection::Server(conn),
        sock: tcp,
    })
}

pub fn connect_tls(tcp: TcpStream, config: Arc<ClientConfig>) -> Result<TlsStream> {
    let name = ServerName::try_from("localhost")
        .map_err(|e| anyhow::anyhow!("invalid server name: {e}"))?
        .to_owned();
    let mut conn = rustls::ClientConnection::new(config, name)?;
    conn.set_buffer_limit(None);
    Ok(TlsStream {
        conn: Connection::Client(conn),
        sock: tcp,
    })
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
