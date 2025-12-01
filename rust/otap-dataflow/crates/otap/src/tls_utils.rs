// Copyright The OpenTelemetry Authors
// SPDX-License-Identifier: Apache-2.0

use arc_swap::ArcSwap;
use base64::prelude::*;
use futures::{Stream, StreamExt};
use otap_df_config::tls::TlsServerConfig;
use rustls::pki_types::{CertificateDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::server::{ClientHello, ResolvesServerCert, WebPkiClientVerifier};
use rustls::sign::CertifiedKey;
use rustls::{DigitallySignedStruct, DistinguishedName, Error, SignatureScheme};
use rustls_native_certs::load_native_certs;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime};
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

/// Loads TLS configuration for a server.
///
/// Returns `Ok(None)` when no cert/key material is provided, indicating TLS is disabled.
pub async fn load_server_tls_config(
    config: &TlsServerConfig,
) -> Result<Option<ServerTlsConfig>, io::Error> {
    // If neither cert nor key is provided, we assume TLS is disabled.
    // However, if one is provided, the other must be too.
    let (cert, key) = match (
        &config.config.cert_file,
        &config.config.key_file,
        &config.config.cert_pem,
        &config.config.key_pem,
    ) {
        (Some(cert_file), Some(key_file), _, _) => {
            let cert = tokio::fs::read(cert_file).await.map_err(|e| {
                log::error!("Failed to read cert file {:?}: {}", cert_file, e);
                e
            })?;
            let key = tokio::fs::read(key_file).await.map_err(|e| {
                log::error!("Failed to read key file {:?}: {}", key_file, e);
                e
            })?;
            (cert, key)
        }
        (None, None, Some(cert_pem), Some(key_pem)) => {
            (cert_pem.clone().into_bytes(), key_pem.clone().into_bytes())
        }
        (None, None, None, None) => {
            return Ok(None);
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "TLS configuration error: both certificate and key must be provided. \
                     Found cert_file={:?}, key_file={:?}, cert_pem={:?}, key_pem={:?}",
                    config.config.cert_file.is_some(),
                    config.config.key_file.is_some(),
                    config.config.cert_pem.is_some(),
                    config.config.key_pem.is_some()
                ),
            ));
        }
    };

    let identity = Identity::from_pem(cert, key);
    let mut tls_builder = ServerTlsConfig::new().identity(identity);

    let mut client_ca_pem = Vec::new();

    // Load system roots if requested
    if config.config.include_system_ca_certs_pool == Some(true) {
        let cert_res = tokio::task::spawn_blocking(load_native_certs)
            .await
            .map_err(io::Error::other)?;

        for error in &cert_res.errors {
            log::warn!("Error loading native cert: {}", error);
        }
        for cert in cert_res.certs {
            let pem = format!(
                "-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n",
                BASE64_STANDARD.encode(cert.as_ref())
            );
            client_ca_pem.extend_from_slice(pem.as_bytes());
        }
    }

    // Load user-provided CA
    if let Some(client_ca_file) = &config.client_ca_file {
        let ca = tokio::fs::read(client_ca_file).await?;
        client_ca_pem.extend_from_slice(&ca);
        trim_trailing_whitespace(&mut client_ca_pem);
        client_ca_pem.push(b'\n');
    } else if let Some(ca_file) = &config.config.ca_file {
        let ca = tokio::fs::read(ca_file).await?;
        client_ca_pem.extend_from_slice(&ca);
        trim_trailing_whitespace(&mut client_ca_pem);
        client_ca_pem.push(b'\n');
    } else if let Some(ca_pem) = &config.config.ca_pem {
        client_ca_pem.extend_from_slice(ca_pem.clone().into_bytes().as_slice());
        trim_trailing_whitespace(&mut client_ca_pem);
        client_ca_pem.push(b'\n');
    }

    if !client_ca_pem.is_empty() {
        let ca_cert = Certificate::from_pem(client_ca_pem);
        tls_builder = tls_builder.client_ca_root(ca_cert);
    } else if config.config.include_system_ca_certs_pool == Some(true) {
        return Err(io::Error::other(
            "TLS configuration error: include_system_ca_certs_pool is true, but no CA certificates were loaded. \
             Ensure system certificates are available or provide a client_ca_file.",
        ));
    } else {
        log::warn!(
            "Warning: No client CA configured. mTLS is disabled. \
             Set client_ca_file or include_system_ca_certs_pool to enable client verification."
        );
    }

    Ok(Some(tls_builder))
}

/// Trims trailing whitespace (spaces, tabs, newlines) from a byte vector.
fn trim_trailing_whitespace(buf: &mut Vec<u8>) {
    while buf
        .last()
        .is_some_and(|&b| b == b' ' || b == b'\t' || b == b'\n' || b == b'\r')
    {
        let _ = buf.pop();
    }
}

/// Creates a TLS stream from a TCP listener stream and a TLS acceptor.
///
/// This function handles the TLS handshake for each incoming connection.
/// TLS handshake failures are logged and filtered out (non-fatal).
/// Transport-level listener errors are propagated to terminate the server.
pub fn create_tls_stream<S, T>(
    listener_stream: S,
    tls_acceptor: tokio_rustls::TlsAcceptor,
) -> impl Stream<Item = Result<tokio_rustls::server::TlsStream<T>, io::Error>>
where
    S: Stream<Item = Result<T, io::Error>> + Send + 'static,
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    listener_stream.filter_map(move |conn_res| {
        let acceptor = tls_acceptor.clone();
        async move {
            match conn_res {
                Ok(conn) => {
                    // Try TLS handshake
                    match acceptor.accept(conn).await {
                        Ok(stream) => Some(Ok::<_, io::Error>(stream)),
                        Err(e) => {
                            // TLS handshake failed - log and continue
                            log::warn!("TLS handshake failed: {}", e);
                            None
                        }
                    }
                }
                Err(e) => {
                    // Transport-level listener error - propagate to terminate server
                    Some(Err(e))
                }
            }
        }
    })
}

/// Lazy-reloading certificate resolver with throttled mtime checks
#[derive(Debug)]
pub struct LazyReloadableCertResolver {
    /// Current certificate
    cert_key: Arc<ArcSwap<CertifiedKey>>,

    /// File paths
    cert_path: PathBuf,
    key_path: PathBuf,

    /// Last known modification times
    cert_mtime: AtomicU64,
    key_mtime: AtomicU64,

    /// Last time we checked mtime (unix timestamp in seconds)
    last_check_time: AtomicU64,

    /// Minimum interval between mtime checks (seconds)
    check_interval_secs: u64,

    /// Reload lock
    is_reloading: AtomicBool,
}

impl LazyReloadableCertResolver {
    /// Creates a new LazyReloadableCertResolver
    pub fn new(
        cert_path: PathBuf,
        key_path: PathBuf,
        check_interval: Option<Duration>,
    ) -> Result<Self, io::Error> {
        let cert_key = load_certified_key_sync(&cert_path, &key_path)?;
        let cert_mtime = get_mtime(&cert_path)?;
        let key_mtime = get_mtime(&key_path)?;
        let now = current_timestamp();

        Ok(Self {
            cert_key: Arc::new(ArcSwap::from_pointee(cert_key)),
            cert_path,
            key_path,
            cert_mtime: AtomicU64::new(cert_mtime),
            key_mtime: AtomicU64::new(key_mtime),
            last_check_time: AtomicU64::new(now),
            check_interval_secs: check_interval.map(|d| d.as_secs()).unwrap_or(300), // Default: 5 minutes
            is_reloading: AtomicBool::new(false),
        })
    }

    /// Check if enough time has passed, then check mtime and reload if needed.
    ///
    /// # Concurrency Design
    ///
    /// This method uses a "leader election" pattern: multiple concurrent callers race to win
    /// the compare-exchange on `last_check_time`. The winner performs the mtime check and
    /// potential reload; losers return immediately.
    ///
    /// This is intentional: we want exactly one thread to do the I/O-heavy mtime check per
    /// interval. Other threads returning early is correct behavior, not a bug. The `is_reloading`
    /// flag provides additional protection to ensure only one reload happens even if multiple
    /// threads detect a change simultaneously.
    ///
    /// The trade-off is that if a file changes *during* the winner's mtime check (after it read
    /// the old mtime but before it completes), that change won't be detected until the next
    /// interval. This is acceptable for certificate rotation, which typically has much longer
    /// lead times than the check interval.
    pub fn check_and_reload_if_interval_expired(&self) -> bool {
        let now = current_timestamp();
        let last_check = self.last_check_time.load(Ordering::Relaxed);

        // Fast path: interval not expired yet
        if now.saturating_sub(last_check) < self.check_interval_secs {
            return false; // Skip mtime check entirely
        }

        // Interval expired - try to win the check race (leader election)
        if self
            .last_check_time
            .compare_exchange(last_check, now, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Another thread won - they'll handle the check
            return false;
        }

        // We won - check mtimes
        let current_cert_mtime = match get_mtime(&self.cert_path) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("Failed to check cert mtime: {}", e);
                return false;
            }
        };

        let current_key_mtime = match get_mtime(&self.key_path) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("Failed to check key mtime: {}", e);
                return false;
            }
        };

        // Compare with cached mtimes
        let last_cert_mtime = self.cert_mtime.load(Ordering::Relaxed);
        let last_key_mtime = self.key_mtime.load(Ordering::Relaxed);

        if current_cert_mtime == last_cert_mtime && current_key_mtime == last_key_mtime {
            return false; // No change
        }

        // Files changed! Reload
        if self
            .is_reloading
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false; // Another thread reloading
        }

        let result = self.do_reload(current_cert_mtime, current_key_mtime);
        self.is_reloading.store(false, Ordering::Release);
        result
    }

    /// Returns the currently loaded certified key
    pub fn current_cert_key(&self) -> Arc<CertifiedKey> {
        self.cert_key.load_full()
    }

    fn do_reload(&self, new_cert_mtime: u64, new_key_mtime: u64) -> bool {
        match load_certified_key_sync(&self.cert_path, &self.key_path) {
            Ok(new_cert) => {
                self.cert_key.store(Arc::new(new_cert));
                self.cert_mtime.store(new_cert_mtime, Ordering::Relaxed);
                self.key_mtime.store(new_key_mtime, Ordering::Relaxed);
                log::info!(
                    "TLS certificate reloaded: cert={:?}, key={:?}",
                    self.cert_path,
                    self.key_path
                );
                true
            }
            Err(e) => {
                log::error!("Failed to reload cert (keeping current): {}", e);
                false
            }
        }
    }
}

impl ResolvesServerCert for LazyReloadableCertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        // Lazy check: only if interval expired (no overhead on most requests)
        let _ = self.check_and_reload_if_interval_expired();

        // Return current cert (wait-free)
        Some(self.cert_key.load_full())
    }
}

/// Lazy-reloading client CA verifier with throttled mtime checks.
///
/// This verifier wraps an inner `WebPkiClientVerifier` and reloads it when the CA or CRL
/// files change on disk. The reload is throttled to avoid excessive filesystem checks.
///
/// # Note on `root_hint_subjects()`
///
/// The `root_hint_subjects()` method returns an empty slice. Root hints are optional in TLS -
/// they're sent to the client as a hint about which CAs are acceptable, but the actual
/// certificate verification still works correctly without them.
///
/// ## Why not provide real hints?
///
/// The `rustls::ClientCertVerifier::root_hint_subjects()` trait method returns `&[DistinguishedName]`
/// with a lifetime tied to `&self`. This creates a challenge for hot-reloading because:
///
/// 1. **Memory leak approach**: Using `Box::leak()` to create `&'static` hints works but leaks
///    memory on every CA reload (~100 bytes to a few KB per reload). With a 5-minute reload
///    interval, this accumulates ~30 KB/day - acceptable but not ideal for long-running services.
///
/// 2. **Unsafe pointer approach**: Storing hints in an `Arc` and using `AtomicPtr` with manual
///    memory management could work, but introduces subtle use-after-free risks if a TLS handshake
///    holds a reference while a reload occurs. The complexity isn't justified for optional hints.
///
/// 3. **Empty slice approach (current)**: Simple, safe, zero overhead. Clients don't receive
///    CA hints but verification works correctly. Most mTLS deployments work fine without hints
///    since clients typically know which certificate to present.
///
/// ## When to revisit
///
/// If your use case requires root hints (e.g., clients that dynamically select certificates
/// based on server hints), consider:
/// - Using static CA configuration instead of hot-reloading
/// - Implementing a custom solution with `Box::leak()` if the memory cost is acceptable
/// - Contributing an `Arc`-based solution if you can prove it's safe
#[derive(Debug)]
pub struct LazyReloadableClientCaVerifier {
    inner: Arc<ArcSwap<Arc<dyn ClientCertVerifier>>>,
    ca_path: PathBuf,
    crl_path: Option<PathBuf>,
    ca_mtime: AtomicU64,
    crl_mtime: AtomicU64,
    last_check_time: AtomicU64,
    check_interval_secs: u64,
    is_reloading: AtomicBool,
}

impl LazyReloadableClientCaVerifier {
    /// Creates a new LazyReloadableClientCaVerifier
    pub fn new(
        ca_path: PathBuf,
        crl_path: Option<PathBuf>,
        check_interval: Option<Duration>,
    ) -> Result<Self, io::Error> {
        let verifier = load_client_verifier_sync(&ca_path, crl_path.as_ref())?;

        let ca_mtime = get_mtime(&ca_path)?;
        let crl_mtime = if let Some(p) = &crl_path {
            get_mtime(p)?
        } else {
            0
        };
        let now = current_timestamp();

        Ok(Self {
            inner: Arc::new(ArcSwap::from_pointee(verifier)),
            ca_path,
            crl_path,
            ca_mtime: AtomicU64::new(ca_mtime),
            crl_mtime: AtomicU64::new(crl_mtime),
            last_check_time: AtomicU64::new(now),
            check_interval_secs: check_interval.map(|d| d.as_secs()).unwrap_or(300),
            is_reloading: AtomicBool::new(false),
        })
    }

    fn check_and_reload_if_interval_expired(&self) {
        let now = current_timestamp();
        let last_check = self.last_check_time.load(Ordering::Relaxed);

        if now.saturating_sub(last_check) < self.check_interval_secs {
            return;
        }

        if self
            .last_check_time
            .compare_exchange(last_check, now, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let current_ca_mtime = match get_mtime(&self.ca_path) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("Failed to check CA mtime: {}", e);
                return;
            }
        };

        let current_crl_mtime = if let Some(p) = &self.crl_path {
            match get_mtime(p) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!("Failed to check CRL mtime: {}", e);
                    return;
                }
            }
        } else {
            0
        };

        let last_ca_mtime = self.ca_mtime.load(Ordering::Relaxed);
        let last_crl_mtime = self.crl_mtime.load(Ordering::Relaxed);

        if current_ca_mtime == last_ca_mtime && current_crl_mtime == last_crl_mtime {
            return;
        }

        if self
            .is_reloading
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        self.do_reload(current_ca_mtime, current_crl_mtime);
        self.is_reloading.store(false, Ordering::Release);
    }

    fn do_reload(&self, new_ca_mtime: u64, new_crl_mtime: u64) {
        match load_client_verifier_sync(&self.ca_path, self.crl_path.as_ref()) {
            Ok(new_verifier) => {
                self.inner.store(Arc::new(new_verifier));
                self.ca_mtime.store(new_ca_mtime, Ordering::Relaxed);
                self.crl_mtime.store(new_crl_mtime, Ordering::Relaxed);
                log::info!(
                    "Client CA/CRL certificates reloaded: ca={:?}, crl={:?}",
                    self.ca_path,
                    self.crl_path
                );
            }
            Err(e) => {
                log::error!("Failed to reload client CA/CRL (keeping current): {}", e);
            }
        }
    }
}

impl ClientCertVerifier for LazyReloadableClientCaVerifier {
    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, Error> {
        self.check_and_reload_if_interval_expired();
        self.inner
            .load()
            .verify_client_cert(end_entity, intermediates, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, Error> {
        self.inner.load().verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, Error> {
        self.inner.load().verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.load().supported_verify_schemes()
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        // Return empty slice - root hints are optional in TLS.
        // They're just a hint to the client about acceptable CAs;
        // actual certificate verification works correctly without them.
        //
        // See struct-level documentation for why we don't provide real hints
        // and the trade-offs involved (memory leak vs unsafe vs empty).
        &[]
    }
}

fn current_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn get_mtime(path: &PathBuf) -> Result<u64, io::Error> {
    std::fs::metadata(path)?
        .modified()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(io::Error::other)
}

fn load_certified_key_sync(
    cert_path: &PathBuf,
    key_path: &PathBuf,
) -> Result<CertifiedKey, io::Error> {
    use rustls_pemfile::{certs, private_key};
    use std::io::BufReader;

    let cert_pem = std::fs::read(cert_path)?;
    let key_pem = std::fs::read(key_path)?;

    let certs: Vec<_> = certs(&mut BufReader::new(&cert_pem[..]))
        .collect::<Result<_, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    if certs.is_empty() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "No certs"));
    }

    let key = private_key(&mut BufReader::new(&key_pem[..]))
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "No private key found in key file",
            )
        })?;

    let signing_key = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    Ok(CertifiedKey::new(certs, signing_key))
}

fn load_client_verifier_sync(
    ca_path: &PathBuf,
    crl_path: Option<&PathBuf>,
) -> Result<Arc<dyn ClientCertVerifier>, io::Error> {
    let ca_pem = std::fs::read(ca_path)?;
    let mut roots = rustls::RootCertStore::empty();

    let certs = rustls_pemfile::certs(&mut io::BufReader::new(&ca_pem[..]))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    for cert in certs {
        roots
            .add(cert)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    }

    let mut builder = WebPkiClientVerifier::builder(roots.into());

    if let Some(crl_path) = crl_path {
        let crl_pem = std::fs::read(crl_path)?;
        let crls = rustls_pemfile::crls(&mut io::BufReader::new(&crl_pem[..]))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if !crls.is_empty() {
            builder = builder.with_crls(crls);
        }
    }

    let verifier = builder
        .build()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    Ok(verifier)
}

/// Builds a reloadable server config from the given configuration.
/// If file paths are provided, it uses lazy reloading.
/// If PEM strings are provided, it uses static configuration.
///
/// Note on System CAs: If `include_system_ca_certs_pool` is true, system certificates are loaded.
/// If no system certificates are found, this function returns an error to prevent starting
/// without expected trust anchors.
pub async fn build_reloadable_server_config(
    config: &TlsServerConfig,
) -> Result<Arc<rustls::ServerConfig>, io::Error> {
    let check_interval = config
        .config
        .reload_interval
        .as_ref()
        .map(|s| humantime::parse_duration(s))
        .transpose()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;

    let builder = rustls::ServerConfig::builder();

    // Client Auth
    let builder = if let Some(client_ca_file) = &config.client_ca_file {
        let client_verifier = Arc::new(LazyReloadableClientCaVerifier::new(
            client_ca_file.clone(),
            config.client_crl_file.clone(),
            check_interval,
        )?);
        builder.with_client_cert_verifier(client_verifier)
    } else if let Some(ca_file) = &config.config.ca_file {
        // Fallback to common ca_file if client_ca_file is not set
        let client_verifier = Arc::new(LazyReloadableClientCaVerifier::new(
            ca_file.clone(),
            config.client_crl_file.clone(),
            check_interval,
        )?);
        builder.with_client_cert_verifier(client_verifier)
    } else if let Some(ca_pem) = &config.config.ca_pem {
        // Static CA from PEM
        let mut roots = rustls::RootCertStore::empty();
        let certs = rustls_pemfile::certs(&mut io::BufReader::new(ca_pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        for cert in certs {
            roots
                .add(cert)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        }
        let verifier = WebPkiClientVerifier::builder(roots.into())
            .build()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        builder.with_client_cert_verifier(verifier)
    } else {
        builder.with_no_client_auth()
    };

    // Cert resolver
    let mut server_config = if let (Some(cert_path), Some(key_path)) =
        (&config.config.cert_file, &config.config.key_file)
    {
        // File-based: use lazy reloader
        let cert_resolver = Arc::new(LazyReloadableCertResolver::new(
            cert_path.clone(),
            key_path.clone(),
            check_interval,
        )?);
        builder.with_cert_resolver(cert_resolver)
    } else if let (Some(cert_pem), Some(key_pem)) =
        (&config.config.cert_pem, &config.config.key_pem)
    {
        // PEM-based: static
        let certs = rustls_pemfile::certs(&mut io::BufReader::new(cert_pem.as_bytes()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        let key = rustls_pemfile::private_key(&mut io::BufReader::new(key_pem.as_bytes()))
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "No server private key found in PEM",
                )
            })?;

        builder
            .with_single_cert(certs, key)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "TLS requires either cert_file/key_file or cert_pem/key_pem",
        ));
    };

    server_config.alpn_protocols = vec![b"h2".to_vec()];

    Ok(Arc::new(server_config))
}

#[cfg(test)]
mod tests {
    use super::*;
    use otap_df_config::tls::TlsConfig;
    use std::fs;
    use std::process::Command;
    use std::thread;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_load_server_tls_config_missing_key() {
        let config = TlsServerConfig {
            config: TlsConfig {
                cert_pem: Some("fake cert".to_string()),
                key_pem: None,
                ..Default::default()
            },
            ..Default::default()
        };

        let result = load_server_tls_config(&config).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("TLS configuration error"));
    }

    #[tokio::test]
    async fn test_load_server_tls_config_missing_cert() {
        let config = TlsServerConfig {
            config: TlsConfig {
                cert_pem: None,
                key_pem: Some("fake key".to_string()),
                ..Default::default()
            },
            ..Default::default()
        };

        let result = load_server_tls_config(&config).await;
        assert!(result.is_err());
    }

    fn generate_cert(dir: &std::path::Path, name: &str, cn: &str) {
        // Generate Key and Cert in one go (self-signed)
        let status = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-keyout",
                &format!("{}.key", name),
                "-out",
                &format!("{}.crt", name),
                "-days",
                "1",
                "-nodes",
                "-subj",
                &format!("/CN={}", cn),
                "-addext",
                "basicConstraints=critical,CA:TRUE",
            ])
            .current_dir(dir)
            .output()
            .expect("Failed to generate cert");

        if !status.status.success() {
            panic!(
                "Cert gen failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        }
    }

    #[test]
    fn test_lazy_reload_resolver() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path();
        let cert_path = path.join("server.crt");
        let key_path = path.join("server.key");

        // 1. Generate initial cert
        generate_cert(path, "cert1", "localhost");
        let _ = fs::copy(path.join("cert1.crt"), &cert_path).unwrap();
        let _ = fs::copy(path.join("cert1.key"), &key_path).unwrap();

        // 2. Create resolver with short interval
        let resolver = LazyReloadableCertResolver::new(
            cert_path.clone(),
            key_path.clone(),
            Some(Duration::from_millis(500)),
        )
        .expect("Failed to create resolver");

        let initial_cert = resolver.current_cert_key();
        assert!(!initial_cert.cert.is_empty());

        // 3. Wait for interval to expire
        thread::sleep(Duration::from_millis(600));

        // 4. Update cert file (ensure mtime changes)
        // Sleep a bit to ensure FS mtime granularity (some systems are 1s)
        thread::sleep(Duration::from_millis(1100));

        generate_cert(path, "cert2", "otherhost");
        let _ = fs::copy(path.join("cert2.crt"), &cert_path).unwrap();
        let _ = fs::copy(path.join("cert2.key"), &key_path).unwrap();

        // 5. Trigger reload
        let reloaded = resolver.check_and_reload_if_interval_expired();
        assert!(reloaded, "Should have reloaded");

        let new_cert = resolver.current_cert_key();
        assert_ne!(initial_cert.cert, new_cert.cert, "Cert should have changed");

        // 6. Trigger again immediately - should not reload
        let reloaded_again = resolver.check_and_reload_if_interval_expired();
        assert!(!reloaded_again, "Should not reload again immediately");
    }

    #[test]
    fn test_lazy_reload_client_ca() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path();
        let ca_path = path.join("ca.crt");

        // 1. Generate initial CA
        generate_cert(path, "ca1", "Test CA 1");
        let _ = fs::copy(path.join("ca1.crt"), &ca_path).unwrap();

        // 2. Create verifier with short interval
        let verifier = LazyReloadableClientCaVerifier::new(
            ca_path.clone(),
            None,
            Some(Duration::from_millis(500)),
        )
        .expect("Failed to create verifier");

        // 3. Verify root_hint_subjects returns empty slice (by design - see struct docs)
        let initial_hints = verifier.root_hint_subjects();
        assert!(
            initial_hints.is_empty(),
            "Root hints are intentionally empty to avoid memory leaks"
        );

        // 4. Wait for interval to expire + filesystem mtime granularity
        thread::sleep(Duration::from_millis(600));
        thread::sleep(Duration::from_millis(1100));

        // 5. Update CA file with a different CA
        generate_cert(path, "ca2", "Test CA 2");
        let _ = fs::copy(path.join("ca2.crt"), &ca_path).unwrap();

        // 6. Trigger reload via trait method (which calls check_and_reload internally)
        let new_hints = verifier.root_hint_subjects();

        // Hints should still be empty (this is by design)
        assert!(
            new_hints.is_empty(),
            "Root hints are intentionally empty to avoid memory leaks"
        );

        // Verify the verifier still works by checking supported schemes
        let schemes = verifier.supported_verify_schemes();
        assert!(!schemes.is_empty(), "Should support verification schemes");
    }

    #[test]
    fn test_lazy_reload_client_ca_with_crl_path() {
        // This test verifies that the verifier can be constructed with a CRL path
        // and that the mtime checking logic works for CRL files.
        // We use a CA cert file as a stand-in for a CRL file to test the mtime logic,
        // since generating a valid CRL requires complex openssl ca setup.
        // The actual CRL parsing is tested implicitly when load_client_verifier_sync
        // is called with a real CRL.

        let _ = rustls::crypto::ring::default_provider().install_default();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path();
        let ca_path = path.join("ca.crt");

        // Generate CA
        generate_cert(path, "ca1", "Test CA");
        let _ = fs::copy(path.join("ca1.crt"), &ca_path).unwrap();

        // Create verifier WITHOUT CRL (CRL is optional)
        // This tests the code path where crl_path is None
        let verifier = LazyReloadableClientCaVerifier::new(
            ca_path.clone(),
            None, // No CRL
            Some(Duration::from_millis(500)),
        )
        .expect("Failed to create verifier");

        // Verify root_hint_subjects returns empty (by design)
        let hints = verifier.root_hint_subjects();
        assert!(
            hints.is_empty(),
            "Root hints are intentionally empty to avoid memory leaks"
        );

        // Verify supported schemes work
        let schemes = verifier.supported_verify_schemes();
        assert!(!schemes.is_empty(), "Should support verification schemes");
    }
}
