use std::path::Path;
use std::sync::Arc;
use tokio_rustls::TlsAcceptor;

use crate::error::{ProxyError, Result};

pub fn build_tls_acceptor(cert_pem: &[u8], key_pem: &[u8]) -> Result<TlsAcceptor> {
    let mut cert_reader = std::io::BufReader::new(cert_pem);
    let certs = rustls_pemfile::certs(&mut cert_reader)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| ProxyError::Tls(format!("cert parse: {e}")))?;

    let mut key_reader = std::io::BufReader::new(key_pem);
    let key = rustls_pemfile::private_key(&mut key_reader)
        .map_err(|e| ProxyError::Tls(format!("key parse: {e}")))?
        .ok_or_else(|| ProxyError::Tls("no private key found".into()))?;

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| ProxyError::Tls(e.to_string()))?;

    Ok(TlsAcceptor::from(Arc::new(config)))
}

fn generate_self_signed() -> Result<(Vec<u8>, Vec<u8>)> {
    let key_pair = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| ProxyError::Tls(format!("key gen: {e}")))?;

    let mut params = rcgen::CertificateParams::new(vec!["Mixer".to_string()])
        .map_err(|e| ProxyError::Tls(format!("cert params: {e}")))?;

    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyEncipherment,
        rcgen::KeyUsagePurpose::DigitalSignature,
        rcgen::KeyUsagePurpose::KeyCertSign,
    ];
    params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];

    let cert = params.self_signed(&key_pair)
        .map_err(|e| ProxyError::Tls(format!("self sign: {e}")))?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    Ok((cert_pem.into_bytes(), key_pem.into_bytes()))
}

pub async fn load_or_generate(cert_path: Option<&Path>, key_path: Option<&Path>) -> Result<(Vec<u8>, Vec<u8>)> {
    match (cert_path, key_path) {
        (Some(cp), Some(kp)) => {
            let cert_exists = tokio::fs::try_exists(cp).await.unwrap_or(false);
            let key_exists = tokio::fs::try_exists(kp).await.unwrap_or(false);

            if cert_exists && key_exists {
                let cert = tokio::fs::read(cp).await?;
                let key = tokio::fs::read(kp).await?;
                Ok((cert, key))
            } else {
                if cert_exists != key_exists {
                    return Err(ProxyError::Config(
                        "tls_cert_path and tls_key_path must both exist or both be absent".into(),
                    ));
                }
                let (cert, key) = generate_self_signed()?;
                if let Some(parent) = cp.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                if let Some(parent) = kp.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(cp, &cert).await?;
                tokio::fs::write(kp, &key).await?;
                tracing::info!("Generated self-signed TLS certificate at {}", cp.display());
                Ok((cert, key))
            }
        }
        (None, None) => generate_self_signed(),
        _ => Err(ProxyError::Config(
            "tls_cert_path and tls_key_path must be specified together".into(),
        )),
    }
}
