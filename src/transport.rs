//! The transport between the two machines, behind two traits: the remote [`Dial`]s, the desk
//! [`Accept`]s, and either way the result is an authenticated byte stream (`AsyncRead +
//! AsyncWrite`) that the link, and later gliff's framing, runs on. A QUIC transport can replace
//! TCP by implementing the same two traits.
//!
//! The TCP one runs mutual TLS 1.3. There is no CA and no host name: each side accepts exactly
//! one certificate, the one whose SHA-256 it recorded at pairing, and the handshake fails for
//! anything else, so the desk never hands an unpaired peer's stream to the rest of Navette.

use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, DistinguishedName, ServerConfig,
    SignatureScheme,
};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;

use crate::identity::{Fingerprint, Identity};

/// How long a peer has to finish connecting before it is dropped.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How many connections the desk handshakes with at once.
const MAX_HANDSHAKES: usize = 8;

/// An authenticated byte stream to the paired machine.
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> Stream for T {}

/// The remote's side: it opens the connection to the desk.
pub trait Dial: Send + Sync {
    type Stream: Stream;
    fn dial(&self) -> impl Future<Output = io::Result<Self::Stream>> + Send;
}

/// The desk's side: it waits for the paired remote.
pub trait Accept: Send {
    type Stream: Stream;
    /// The next authenticated stream. An `Err` is one refused or failed connection; the next
    /// call waits for another.
    fn accept(&mut self) -> impl Future<Output = io::Result<Self::Stream>> + Send;
}

/// Refuses an address that is not one machine's own: a wildcard (the desk listens on its LAN
/// address only), multicast or broadcast. An IPv4-mapped IPv6 address is judged as the IPv4 one
/// it stands for, since `[::ffff:0.0.0.0]` binds every interface too.
pub fn refuse_wildcard(address: SocketAddr) -> io::Result<()> {
    let ip = address.ip().to_canonical();
    let broadcast = matches!(ip, IpAddr::V4(v4) if v4.is_broadcast());
    if ip.is_unspecified() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{address} listens on every interface: give the LAN address of this machine instead"
            ),
        ));
    }
    if ip.is_multicast() || broadcast {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "{address} is a multicast or broadcast address: give the LAN address of this machine instead"
            ),
        ));
    }
    Ok(())
}

/// Binds a listener on one explicit address, never a wildcard.
pub async fn bind_lan(address: SocketAddr) -> io::Result<TcpListener> {
    refuse_wildcard(address)?;
    TcpListener::bind(address).await
}

/// Dials the desk over TCP and TLS 1.3, presenting this machine's identity and accepting only the
/// certificate of the desk it paired with.
pub struct TlsDialer {
    address: SocketAddr,
    connector: tokio_rustls::TlsConnector,
    timeout: Duration,
}

impl TlsDialer {
    pub fn new(identity: &Identity, home: Fingerprint, address: SocketAddr) -> io::Result<Self> {
        let provider = provider();
        let config = ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(io::Error::other)?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedPeer::new(home, &provider)))
            .with_client_auth_cert(vec![identity.certificate()], identity.private_key())
            .map_err(io::Error::other)?;
        Ok(Self {
            address,
            connector: tokio_rustls::TlsConnector::from(Arc::new(config)),
            timeout: HANDSHAKE_TIMEOUT,
        })
    }
}

impl Dial for TlsDialer {
    type Stream = tokio_rustls::client::TlsStream<TcpStream>;

    async fn dial(&self) -> io::Result<Self::Stream> {
        let connect = async {
            let tcp = TcpStream::connect(self.address).await?;
            send_at_once(&tcp)?;
            // The name is not checked (the certificate is pinned), but TLS needs one.
            let name = ServerName::try_from("navette").expect("a valid DNS name");
            self.connector.connect(name, tcp).await
        };
        tokio::time::timeout(self.timeout, connect)
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "the desk did not answer in time")
            })?
    }
}

/// Listens on the desk's LAN address and completes TLS 1.3 only with the paired remote.
pub struct TlsAcceptor {
    listener: TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    timeout: Duration,
    handshakes: JoinSet<io::Result<tokio_rustls::server::TlsStream<TcpStream>>>,
}

impl TlsAcceptor {
    pub async fn bind(
        identity: &Identity,
        remote: Fingerprint,
        address: SocketAddr,
    ) -> io::Result<Self> {
        let provider = provider();
        let config = ServerConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(io::Error::other)?
            .with_client_cert_verifier(Arc::new(PinnedPeer::new(remote, &provider)))
            .with_single_cert(vec![identity.certificate()], identity.private_key())
            .map_err(io::Error::other)?;
        Ok(Self {
            listener: bind_lan(address).await?,
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(config)),
            timeout: HANDSHAKE_TIMEOUT,
            handshakes: JoinSet::new(),
        })
    }

    pub fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }
}

impl Accept for TlsAcceptor {
    type Stream = tokio_rustls::server::TlsStream<TcpStream>;

    async fn accept(&mut self) -> io::Result<Self::Stream> {
        // Handshakes run side by side, so a silent connection holds one slot for its timeout
        // and not the whole listener. Past the last slot the kernel's backlog waits.
        loop {
            tokio::select! {
                Some(done) = self.handshakes.join_next() => {
                    return done.map_err(io::Error::other)?;
                }
                accepted = self.listener.accept(), if self.handshakes.len() < MAX_HANDSHAKES => {
                    let (tcp, peer) = accepted?;
                    send_at_once(&tcp)?;
                    let acceptor = self.acceptor.clone();
                    let timeout = self.timeout;
                    self.handshakes.spawn(async move {
                        match tokio::time::timeout(timeout, acceptor.accept(tcp)).await {
                            Ok(Ok(stream)) => Ok(stream),
                            Ok(Err(e)) => Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                format!("refused {peer}: {e}"),
                            )),
                            Err(_) => Err(io::Error::new(
                                io::ErrorKind::TimedOut,
                                format!("dropped {peer}: no handshake in time"),
                            )),
                        }
                    });
                }
            }
        }
    }
}

/// Nagle off: a 10 ms sound frame or a keystroke is a small write, which the kernel would
/// otherwise hold until the peer acknowledges the last one, and with its delayed
/// acknowledgements that is up to 40 ms on Wi-Fi.
fn send_at_once(tcp: &TcpStream) -> io::Result<()> {
    tcp.set_nodelay(true)
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Accepts one certificate, by its SHA-256, on either side of the handshake. The handshake
/// signature is still checked against that certificate's key, so presenting a copy of the
/// certificate without its key fails too.
#[derive(Debug)]
struct PinnedPeer {
    peer: Fingerprint,
    algorithms: WebPkiSupportedAlgorithms,
}

impl PinnedPeer {
    fn new(peer: Fingerprint, provider: &CryptoProvider) -> Self {
        Self {
            peer,
            algorithms: provider.signature_verification_algorithms,
        }
    }

    fn check(&self, end_entity: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        if Fingerprint::of(end_entity) == self.peer {
            Ok(())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }
}

impl ServerCertVerifier for PinnedPeer {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

impl ClientCertVerifier for PinnedPeer {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity)
            .map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    const LOOPBACK: &str = "127.0.0.1:0";

    struct Machines {
        home: Identity,
        remote: Identity,
    }

    fn machines() -> Machines {
        Machines {
            home: Identity::generate().unwrap(),
            remote: Identity::generate().unwrap(),
        }
    }

    async fn home_acceptor(m: &Machines) -> TlsAcceptor {
        TlsAcceptor::bind(&m.home, m.remote.fingerprint(), LOOPBACK.parse().unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn home_refuses_to_bind_a_wildcard_address() {
        for wildcard in [
            "0.0.0.0:0",
            "[::]:0",
            "[::ffff:0.0.0.0]:0",
            "224.0.0.1:0",
            "[ff02::1]:0",
            "[::ffff:224.0.0.1]:0",
            "255.255.255.255:0",
            "[::ffff:255.255.255.255]:0",
        ] {
            let error = bind_lan(wildcard.parse().unwrap()).await.unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput, "{wildcard}");
            assert!(error.to_string().contains("LAN address"));
        }
        let m = machines();
        let refused = TlsAcceptor::bind(
            &m.home,
            m.remote.fingerprint(),
            "0.0.0.0:0".parse().unwrap(),
        )
        .await;
        assert!(refused.is_err());
    }

    #[tokio::test]
    async fn home_binds_only_the_configured_address() {
        let acceptor = home_acceptor(&machines()).await;
        let bound = acceptor.local_addr().unwrap();
        assert_eq!(bound.ip(), "127.0.0.1".parse::<std::net::IpAddr>().unwrap());
        assert_ne!(bound.port(), 0);
    }

    #[tokio::test]
    async fn paired_machines_open_a_mutual_tls_link() {
        let m = machines();
        let mut acceptor = home_acceptor(&m).await;
        let dialer = TlsDialer::new(
            &m.remote,
            m.home.fingerprint(),
            acceptor.local_addr().unwrap(),
        )
        .unwrap();
        let (home, remote) = tokio::join!(acceptor.accept(), dialer.dial());
        let (mut home, mut remote) = (home.unwrap(), remote.unwrap());
        remote.write_all(b"ping").await.unwrap();
        let mut buf = [0u8; 4];
        home.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        home.write_all(b"pong").await.unwrap();
        remote.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
    }

    #[tokio::test]
    async fn both_ends_send_each_message_without_waiting_for_an_ack() {
        let m = machines();
        let mut acceptor = home_acceptor(&m).await;
        let dialer = TlsDialer::new(
            &m.remote,
            m.home.fingerprint(),
            acceptor.local_addr().unwrap(),
        )
        .unwrap();
        let (home, remote) = tokio::join!(acceptor.accept(), dialer.dial());
        assert!(home.unwrap().get_ref().0.nodelay().unwrap());
        assert!(remote.unwrap().get_ref().0.nodelay().unwrap());
    }

    #[tokio::test]
    async fn an_unpaired_client_is_refused_before_any_message_is_read() {
        let m = machines();
        let mut acceptor = home_acceptor(&m).await;
        let address = acceptor.local_addr().unwrap();
        // A client with no certificate at all, which trusts the desk certificate.
        let provider = provider();
        let config = ClientConfig::builder_with_provider(provider.clone())
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(PinnedPeer::new(
                m.home.fingerprint(),
                &provider,
            )))
            .with_no_client_auth();
        let client = async {
            let tcp = TcpStream::connect(address).await.unwrap();
            let name = ServerName::try_from("navette").unwrap();
            if let Ok(mut tls) = tokio_rustls::TlsConnector::from(Arc::new(config))
                .connect(name, tcp)
                .await
            {
                let _ = tls.write_all(b"hello").await;
                let _ = tls.flush().await;
            }
        };
        let (accepted, ()) = tokio::join!(acceptor.accept(), client);
        let error = accepted.expect_err("an unpaired client must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn a_client_with_another_certificate_is_refused_before_any_message_is_read() {
        let m = machines();
        let mut acceptor = home_acceptor(&m).await;
        let stranger = Identity::generate().unwrap();
        let dialer = TlsDialer::new(
            &stranger,
            m.home.fingerprint(),
            acceptor.local_addr().unwrap(),
        )
        .unwrap();
        let client = async {
            if let Ok(mut tls) = dialer.dial().await {
                let _ = tls.write_all(b"hello").await;
                let _ = tls.flush().await;
            }
        };
        let (accepted, ()) = tokio::join!(acceptor.accept(), client);
        let error = accepted.expect_err("another certificate must be refused");
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    }

    #[tokio::test]
    async fn the_remote_refuses_a_home_with_another_certificate() {
        let m = machines();
        let impostor = Identity::generate().unwrap();
        let mut acceptor =
            TlsAcceptor::bind(&impostor, m.remote.fingerprint(), LOOPBACK.parse().unwrap())
                .await
                .unwrap();
        let dialer = TlsDialer::new(
            &m.remote,
            m.home.fingerprint(),
            acceptor.local_addr().unwrap(),
        )
        .unwrap();
        let (accepted, dialed) = tokio::join!(acceptor.accept(), dialer.dial());
        assert!(dialed.is_err());
        assert!(accepted.is_err());
    }

    #[tokio::test]
    async fn a_silent_client_does_not_hold_the_listener() {
        let mut acceptor = home_acceptor(&machines())
            .await
            .with_handshake_timeout(Duration::from_millis(200));
        let _silent = TcpStream::connect(acceptor.local_addr().unwrap())
            .await
            .unwrap();
        let error = tokio::time::timeout(Duration::from_secs(5), acceptor.accept())
            .await
            .expect("the handshake timeout must fire")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[tokio::test]
    async fn silent_clients_do_not_keep_the_paired_remote_out() {
        let m = machines();
        // A timeout far longer than the test: only concurrency can let the remote through.
        let mut acceptor = home_acceptor(&m)
            .await
            .with_handshake_timeout(Duration::from_secs(60));
        let address = acceptor.local_addr().unwrap();
        let _silent: Vec<TcpStream> = connect_silently(address, 5).await;
        let dialer = TlsDialer::new(&m.remote, m.home.fingerprint(), address).unwrap();
        let home = tokio::spawn(async move { acceptor.accept().await });
        let remote = tokio::time::timeout(Duration::from_secs(5), dialer.dial())
            .await
            .expect("the paired remote must get through the silent clients")
            .unwrap();
        let home = tokio::time::timeout(Duration::from_secs(5), home)
            .await
            .expect("the desk must hand on the verified stream")
            .unwrap()
            .unwrap();
        drop((home, remote));
    }

    async fn connect_silently(address: SocketAddr, count: usize) -> Vec<TcpStream> {
        let mut clients = Vec::new();
        for _ in 0..count {
            clients.push(TcpStream::connect(address).await.unwrap());
        }
        clients
    }
}
