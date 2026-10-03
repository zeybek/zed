//! TLS for database connections, built on the same rustls stack as Zed's HTTP client.

use std::{
    future::Future,
    io,
    path::Path,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use anyhow::{Context as _, Result};
use rustls::{
    ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime, pem::PemObject},
};
use rustls_platform_verifier::BuilderVerifierExt as _;
use settings::DatabaseSslMode;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::connection::TlsConfig;

/// Builds a rustls client configuration for the given TLS settings.
///
/// - `prefer` and `require` encrypt without verifying the certificate, like libpq.
/// - `verify-ca` checks the certificate chain against `root_cert` (or the bundled Mozilla roots)
///   but not the host name.
/// - `verify-full` checks the chain and host name, using `root_cert` or the platform verifier,
///   which trusts the operating system's certificate store.
pub async fn client_config(tls: &TlsConfig) -> Result<Arc<ClientConfig>> {
    // Installs the process-wide crypto provider that `ClientConfig::builder` relies on.
    http_client_tls::tls_config();

    let provider = rustls::crypto::CryptoProvider::get_default()
        .cloned()
        .context("no TLS crypto provider is installed")?;
    let builder = ClientConfig::builder();

    let client_auth = match (&tls.client_cert, &tls.client_key) {
        (Some(cert), Some(key)) => Some(load_client_identity(cert, key).await?),
        (None, None) => None,
        _ => anyhow::bail!("`ssl_cert` and `ssl_key` must be set together"),
    };
    let custom_roots = match &tls.root_cert {
        Some(path) => Some(Arc::new(load_root_store(path).await?)),
        None => None,
    };

    let builder = match tls.mode {
        DatabaseSslMode::Disable | DatabaseSslMode::Prefer | DatabaseSslMode::Require => builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyCertificate { provider })),
        DatabaseSslMode::VerifyCa => {
            let roots = custom_roots.unwrap_or_else(|| {
                Arc::new(RootCertStore {
                    roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
                })
            });
            let inner = WebPkiServerVerifier::builder(roots)
                .build()
                .context("building the TLS certificate verifier")?;
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(IgnoreHostName { inner }))
        }
        DatabaseSslMode::VerifyFull => match custom_roots {
            Some(roots) => builder.with_root_certificates(roots),
            None => builder
                .with_platform_verifier()
                .context("loading the system certificate store")?,
        },
    };

    let config = match client_auth {
        Some((certs, key)) => builder
            .with_client_auth_cert(certs, key)
            .context("invalid TLS client certificate")?,
        None => builder.with_no_client_auth(),
    };
    Ok(Arc::new(config))
}

async fn load_root_store(path: &Path) -> Result<RootCertStore> {
    let contents = tokio::fs::read(path)
        .await
        .with_context(|| format!("reading CA certificates from {}", path.display()))?;
    let mut store = RootCertStore::empty();
    for certificate in CertificateDer::pem_slice_iter(&contents) {
        let certificate = certificate
            .with_context(|| format!("parsing CA certificates in {}", path.display()))?;
        store
            .add(certificate)
            .with_context(|| format!("adding CA certificate from {}", path.display()))?;
    }
    anyhow::ensure!(
        !store.is_empty(),
        "no certificates found in {}",
        path.display()
    );
    Ok(store)
}

async fn load_client_identity(
    cert_path: &Path,
    key_path: &Path,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    let cert_contents = tokio::fs::read(cert_path)
        .await
        .with_context(|| format!("reading client certificate {}", cert_path.display()))?;
    let certs = CertificateDer::pem_slice_iter(&cert_contents)
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("parsing client certificate {}", cert_path.display()))?;
    anyhow::ensure!(
        !certs.is_empty(),
        "no certificates found in {}",
        cert_path.display()
    );
    let key_contents = tokio::fs::read(key_path)
        .await
        .with_context(|| format!("reading client key {}", key_path.display()))?;
    let key = PrivateKeyDer::from_pem_slice(&key_contents)
        .with_context(|| format!("parsing client key {}", key_path.display()))?;
    Ok((certs, key))
}

/// Encrypts without authenticating the server, matching libpq's `prefer` and `require` modes.
/// Handshake signatures are still checked, so the session key belongs to whoever presented the
/// certificate.
#[derive(Debug)]
struct AcceptAnyCertificate {
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Verifies the certificate chain but accepts any host name, like libpq's `verify-ca`.
#[derive(Debug)]
struct IgnoreHostName {
    inner: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for IgnoreHostName {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Err(rustls::Error::InvalidCertificate(
                rustls::CertificateError::NotValidForName
                | rustls::CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            result => result,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

/// Connects `tokio-postgres` sessions over rustls.
#[derive(Clone)]
pub struct MakeRustlsConnect {
    config: Arc<ClientConfig>,
}

impl MakeRustlsConnect {
    pub fn new(config: Arc<ClientConfig>) -> Self {
        Self { config }
    }
}

impl<S> tokio_postgres::tls::MakeTlsConnect<S> for MakeRustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type TlsConnect = RustlsConnect;
    type Error = io::Error;

    fn make_tls_connect(&mut self, domain: &str) -> io::Result<RustlsConnect> {
        let server_name = ServerName::try_from(domain.to_string())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        Ok(RustlsConnect {
            connector: tokio_rustls::TlsConnector::from(self.config.clone()),
            server_name,
        })
    }
}

pub struct RustlsConnect {
    connector: tokio_rustls::TlsConnector,
    server_name: ServerName<'static>,
}

impl<S> tokio_postgres::tls::TlsConnect<S> for RustlsConnect
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = RustlsStream<S>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<RustlsStream<S>>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        Box::pin(async move {
            let stream = self.connector.connect(self.server_name, stream).await?;
            Ok(RustlsStream(stream))
        })
    }
}

pub struct RustlsStream<S>(tokio_rustls::client::TlsStream<S>);

impl<S> tokio_postgres::tls::TlsStream for RustlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn channel_binding(&self) -> tokio_postgres::tls::ChannelBinding {
        // Channel binding needs the server certificate's signature algorithm. Without it, SCRAM
        // still authenticates, just without binding to the TLS session.
        tokio_postgres::tls::ChannelBinding::none()
    }
}

impl<S> AsyncRead for RustlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S> AsyncWrite for RustlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
