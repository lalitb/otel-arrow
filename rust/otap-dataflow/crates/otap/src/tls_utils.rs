use base64::prelude::*;
use otap_df_config::tls::TlsServerConfig;
use rustls_native_certs::load_native_certs;
use std::io;
use tonic::transport::{Certificate, Identity, ServerTlsConfig};

/// Loads TLS configuration for a server.
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
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "TLS configuration error: both certificate and key must be provided",
            ));
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
        if !client_ca_pem.ends_with(b"\n") {
            client_ca_pem.push(b'\n');
        }
    } else if let Some(ca_file) = &config.config.ca_file {
        let ca = tokio::fs::read(ca_file).await?;
        client_ca_pem.extend_from_slice(&ca);
        if !client_ca_pem.ends_with(b"\n") {
            client_ca_pem.push(b'\n');
        }
    } else if let Some(ca_pem) = &config.config.ca_pem {
        client_ca_pem.extend_from_slice(ca_pem.clone().into_bytes().as_slice());
        if !client_ca_pem.ends_with(b"\n") {
            client_ca_pem.push(b'\n');
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use otap_df_config::tls::TlsConfig;

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
}
