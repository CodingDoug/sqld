use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use hyper::client::HttpConnector;
use sha256::try_digest;
use tonic::transport::Channel;

use crate::auth::{self, Auth};
use crate::net::{AddrIncoming, Connector};

pub struct RpcClientConfig<C = HttpConnector> {
    pub remote_url: String,
    pub connector: C,
    pub tls_config: Option<TlsConfig>,
}

impl<C: Connector> RpcClientConfig<C> {
    pub(crate) async fn configure(self) -> anyhow::Result<(Channel, tonic::transport::Uri)> {
        let uri = tonic::transport::Uri::from_maybe_shared(self.remote_url)?;
        let mut builder = Channel::builder(uri.clone());
        if let Some(ref tls_config) = self.tls_config {
            let cert_pem = std::fs::read_to_string(&tls_config.cert)?;
            let key_pem = std::fs::read_to_string(&tls_config.key)?;
            let identity = tonic::transport::Identity::from_pem(cert_pem, key_pem);

            let ca_cert_pem = std::fs::read_to_string(&tls_config.ca_cert)?;
            let ca_cert = tonic::transport::Certificate::from_pem(ca_cert_pem);

            let tls_config = tonic::transport::ClientTlsConfig::new()
                .identity(identity)
                .ca_certificate(ca_cert)
                .domain_name("sqld");
            builder = builder.tls_config(tls_config)?;
        }

        let channel = builder.connect_with_connector_lazy(self.connector);

        Ok((channel, uri))
    }
}

#[derive(Clone)]
pub struct TlsConfig {
    pub cert: PathBuf,
    pub key: PathBuf,
    pub ca_cert: PathBuf,
}

pub struct RpcServerConfig<A = AddrIncoming> {
    pub acceptor: A,
    pub addr: SocketAddr,
    pub tls_config: Option<TlsConfig>,
}

pub struct UserApiConfig<A = AddrIncoming> {
    pub hrana_ws_acceptor: Option<A>,
    pub http_acceptor: Option<A>,
    pub enable_http_console: bool,
    pub self_url: Option<String>,
    pub http_auth: Option<String>,
    pub auth_jwt_key: Option<String>,
}

impl<A> UserApiConfig<A> {
    pub fn get_auth(&self) -> anyhow::Result<Auth> {
        let mut auth = Auth::default();

        if let Some(arg) = self.http_auth.as_deref() {
            if let Some(param) = auth::parse_http_basic_auth_arg(arg)? {
                auth.http_basic = Some(param);
                tracing::info!("Using legacy HTTP basic authentication");
            }
        }

        if let Some(jwt_key) = self.auth_jwt_key.as_deref() {
            let jwt_key =
                auth::parse_jwt_key(jwt_key).context("Could not parse JWT decoding key")?;
            auth.jwt_key = Some(jwt_key);
            tracing::info!("Using JWT-based authentication");
        }

        auth.disabled = auth.http_basic.is_none() && auth.jwt_key.is_none();
        if auth.disabled {
            tracing::warn!(
                "No authentication specified, the server will not require authentication"
            )
        }

        Ok(auth)
    }
}

pub struct AdminApiConfig<A = AddrIncoming> {
    pub acceptor: A,
}

#[derive(Clone)]
pub struct DbConfig {
    pub extensions_path: Option<Arc<Path>>,
    pub bottomless_replication: Option<bottomless::replicator::Options>,
    pub max_log_size: u64,
    pub max_log_duration: Option<f32>,
    pub soft_heap_limit_mb: Option<usize>,
    pub hard_heap_limit_mb: Option<usize>,
    pub max_response_size: u64,
    pub max_total_response_size: u64,
    pub snapshot_exec: Option<String>,
    pub checkpoint_interval: Option<Duration>,
}

impl DbConfig {
    pub fn validate_extensions(&self) -> anyhow::Result<Arc<[PathBuf]>> {
        let mut valid_extensions = vec![];
        if let Some(ext_dir) = &self.extensions_path {
            let extensions_list = ext_dir.join("trusted.lst");

            let file_contents = std::fs::read_to_string(&extensions_list)
                .with_context(|| format!("can't read {}", &extensions_list.display()))?;

            let extensions = file_contents.lines().filter(|c| !c.is_empty());

            for line in extensions {
                let mut ext_info = line.trim().split_ascii_whitespace();

                let ext_sha = ext_info.next().ok_or_else(|| {
                    anyhow::anyhow!("invalid line on {}: {}", &extensions_list.display(), line)
                })?;
                let ext_fname = ext_info.next().ok_or_else(|| {
                    anyhow::anyhow!("invalid line on {}: {}", &extensions_list.display(), line)
                })?;

                anyhow::ensure!(
                    ext_info.next().is_none(),
                    "extension list seem to contain a filename with whitespaces. Rejected"
                );

                let extension_full_path = ext_dir.join(ext_fname);
                let digest = try_digest(extension_full_path.as_path()).with_context(|| {
                    format!(
                        "Failed to get sha256 digest, while trying to read {}",
                        extension_full_path.display()
                    )
                })?;

                anyhow::ensure!(
                    digest == ext_sha,
                    "sha256 differs for {}. Got {}",
                    ext_fname,
                    digest
                );
                valid_extensions.push(extension_full_path);
            }
        }

        Ok(valid_extensions.into())
    }
}

pub struct HeartbeatConfig {
    pub heartbeat_url: String,
    pub heartbeat_period: Duration,
    pub heartbeat_auth: Option<String>,
}
