//! 控制面监听：rustls 终止 TLS；明文只给回环或显式 `--insecure`。

use axum::Router;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use std::io::{self, ErrorKind};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

/// 控制面对外线。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenWire {
    PlaintextLoopback,
    PlaintextInsecure,
    Rustls,
}

impl ListenWire {
    pub fn uses_tls(self) -> bool {
        matches!(self, Self::Rustls)
    }
}

#[derive(Debug, Clone)]
pub struct TlsPaths {
    pub cert: PathBuf,
    pub key: PathBuf,
}

impl TlsPaths {
    pub fn from_opts(cert: Option<String>, key: Option<String>) -> anyhow::Result<Option<Self>> {
        match (
            cert.filter(|s| !s.trim().is_empty()),
            key.filter(|s| !s.trim().is_empty()),
        ) {
            (None, None) => Ok(None),
            (Some(cert), Some(key)) => Ok(Some(Self {
                cert: PathBuf::from(cert),
                key: PathBuf::from(key),
            })),
            _ => anyhow::bail!("--tls-cert and --tls-key must be set together"),
        }
    }
}

/// 非回环明文必须 `--insecure` / `RUPI_LISTEN_INSECURE`。有证书则走 rustls。
pub fn listen_wire(
    bind: &str,
    tls: Option<&TlsPaths>,
    insecure: bool,
) -> anyhow::Result<ListenWire> {
    let loopback = rupi_runtime::is_loopback_bind(bind);
    if tls.is_some() {
        return Ok(ListenWire::Rustls);
    }
    if loopback {
        return Ok(ListenWire::PlaintextLoopback);
    }
    if insecure {
        return Ok(ListenWire::PlaintextInsecure);
    }
    anyhow::bail!(
        "non-loopback listen {bind} requires --tls-cert/--tls-key or --insecure (RUPI_LISTEN_INSECURE=1)"
    )
}

pub fn load_tls_config(paths: &TlsPaths) -> anyhow::Result<Arc<ServerConfig>> {
    load_tls_config_from_pem(
        &std::fs::read_to_string(&paths.cert)?,
        &std::fs::read_to_string(&paths.key)?,
    )
}

pub fn load_tls_config_from_pem(
    cert_pem: &str,
    key_pem: &str,
) -> anyhow::Result<Arc<ServerConfig>> {
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
    let certs = pem_certs(cert_pem)?;
    if certs.is_empty() {
        anyhow::bail!("tls cert pem has no CERTIFICATE blocks");
    }
    let key = pem_key(key_pem)?;
    let cfg = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| anyhow::anyhow!("tls cert/key: {e}"))?;
    Ok(Arc::new(cfg))
}

fn pem_blocks(pem: &str, label: &str) -> anyhow::Result<Vec<Vec<u8>>> {
    let begin = format!("-----BEGIN {label}-----");
    let end = format!("-----END {label}-----");
    let mut out = Vec::new();
    let mut rest = pem;
    while let Some(start) = rest.find(&begin) {
        let after = &rest[start + begin.len()..];
        let Some(stop) = after.find(&end) else {
            anyhow::bail!("pem: missing {end}");
        };
        let b64: String = after[..stop]
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        out.push(pem_b64(&b64)?);
        rest = &after[stop + end.len()..];
    }
    Ok(out)
}

fn pem_certs(pem: &str) -> anyhow::Result<Vec<CertificateDer<'static>>> {
    Ok(pem_blocks(pem, "CERTIFICATE")?
        .into_iter()
        .map(CertificateDer::from)
        .collect())
}

fn pem_key(pem: &str) -> anyhow::Result<PrivateKeyDer<'static>> {
    if let Some(raw) = pem_blocks(pem, "PRIVATE KEY")?.into_iter().next() {
        return Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(raw)));
    }
    if let Some(raw) = pem_blocks(pem, "RSA PRIVATE KEY")?.into_iter().next() {
        return Ok(PrivateKeyDer::Pkcs1(raw.into()));
    }
    if let Some(raw) = pem_blocks(pem, "EC PRIVATE KEY")?.into_iter().next() {
        return Ok(PrivateKeyDer::Sec1(raw.into()));
    }
    anyhow::bail!("tls key pem has no PRIVATE KEY block")
}

pub(crate) fn pem_b64(s: &str) -> anyhow::Result<Vec<u8>> {
    rupi_runtime::store::b64_decode(s)
}

struct TlsListener {
    inner: TcpListener,
    acceptor: TlsAcceptor,
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (stream, addr) = match self.inner.accept().await {
                Ok(v) => v,
                Err(e) => {
                    if fatal_accept(&e) {
                        tracing::error!("listen accept: {e}");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    continue;
                }
            };
            match self.acceptor.accept(stream).await {
                Ok(tls) => return (tls, addr),
                Err(e) => tracing::warn!("tls handshake: {e}"),
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

fn fatal_accept(e: &io::Error) -> bool {
    !matches!(
        e.kind(),
        ErrorKind::ConnectionAborted | ErrorKind::ConnectionReset | ErrorKind::Interrupted
    )
}

pub async fn serve_router(
    listener: TcpListener,
    router: Router,
    tls: Option<Arc<ServerConfig>>,
) -> anyhow::Result<()> {
    match tls {
        None => axum::serve(listener, router)
            .await
            .map_err(|e| anyhow::anyhow!(e)),
        Some(cfg) => {
            let tls_listener = TlsListener {
                inner: listener,
                acceptor: TlsAcceptor::from(cfg),
            };
            axum::serve(tls_listener, router)
                .await
                .map_err(|e| anyhow::anyhow!(e))
        }
    }
}

/// 给测试和启动共用：从 PEM 文件装证书。
pub fn tls_from_files(
    cert: impl AsRef<Path>,
    key: impl AsRef<Path>,
) -> anyhow::Result<Arc<ServerConfig>> {
    load_tls_config(&TlsPaths {
        cert: cert.as_ref().to_path_buf(),
        key: key.as_ref().to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;
    use std::process::Command;

    #[test]
    fn plaintext_only_loopback_or_explicit_insecure() {
        assert_eq!(
            listen_wire("127.0.0.1:8080", None, false).unwrap(),
            ListenWire::PlaintextLoopback
        );
        assert!(!listen_wire("127.0.0.1:8080", None, false)
            .unwrap()
            .uses_tls());
        assert!(listen_wire("0.0.0.0:8080", None, false).is_err());
        assert_eq!(
            listen_wire("0.0.0.0:8080", None, true).unwrap(),
            ListenWire::PlaintextInsecure
        );
        let tls = TlsPaths {
            cert: PathBuf::from("c.pem"),
            key: PathBuf::from("k.pem"),
        };
        assert_eq!(
            listen_wire("0.0.0.0:8080", Some(&tls), false).unwrap(),
            ListenWire::Rustls
        );
        assert!(TlsPaths::from_opts(Some("c.pem".into()), None).is_err());
        assert!(TlsPaths::from_opts(None, None).unwrap().is_none());
    }

    fn write_self_signed(dir: &std::path::Path) -> (PathBuf, PathBuf) {
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        let st = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-keyout",
                key.to_str().unwrap(),
                "-out",
                cert.to_str().unwrap(),
                "-days",
                "1",
                "-nodes",
                "-subj",
                "/CN=localhost",
            ])
            .status()
            .expect("openssl");
        assert!(st.success(), "openssl req failed");
        (cert, key)
    }

    #[tokio::test]
    async fn rustls_terminates_https_on_loopback() {
        let dir = std::env::temp_dir().join(format!("rupi-tls-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (cert, key) = write_self_signed(&dir);
        let cfg = tls_from_files(&cert, &key).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let router = Router::new().route("/health", get(|| async { "ok" }));
        tokio::spawn(async move {
            let _ = serve_router(listener, router, Some(cfg)).await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(40)).await;
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let resp = client
            .get(format!("https://127.0.0.1:{}/health", addr.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status().as_u16(), 200);
        assert_eq!(resp.text().await.unwrap(), "ok");
        let _ = std::fs::remove_dir_all(dir);
    }
}
