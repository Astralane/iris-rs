use anyhow::anyhow;
use quinn::Endpoint;
use quinn_proto::crypto::rustls::QuicClientConfig;
use quinn_proto::{ClientConfig, IdleTimeout, TransportConfig};
use solana_quic_definitions::{QUIC_KEEP_ALIVE, QUIC_MAX_TIMEOUT, QUIC_SEND_FAIRNESS};
use solana_streamer::nonblocking::quic::ALPN_TPU_PROTOCOL_ID;
use solana_tls_utils::{tls_client_config_builder, QuicClientCertificate};
use std::net::SocketAddr;
use std::sync::Arc;
use solana_sdk::signature::Keypair;

pub fn configure_client_endpoint(
    bind: SocketAddr,
    stake_identity: Option<&Keypair>,
) -> anyhow::Result<Endpoint> {
    let client_certificate = QuicClientCertificate::new(stake_identity);
    let client_config = create_client_config(client_certificate);
    let endpoint = create_client_endpoint(bind, client_config)?;
    Ok(endpoint)
}

pub fn create_client_config(client_certificate: QuicClientCertificate) -> ClientConfig {
    // adapted from QuicLazyInitializedEndpoint::create_endpoint
    let mut crypto = tls_client_config_builder()
        .with_client_auth_cert(
            vec![client_certificate.certificate.clone()],
            client_certificate.key.clone_key(),
        )
        .expect("Failed to set QUIC client certificates");
    crypto.enable_early_data = true;
    crypto.alpn_protocols = vec![ALPN_TPU_PROTOCOL_ID.to_vec()];

    let transport_config = {
        let mut res = TransportConfig::default();

        let timeout = IdleTimeout::try_from(QUIC_MAX_TIMEOUT).unwrap();
        res.max_idle_timeout(Some(timeout));
        res.keep_alive_interval(Some(QUIC_KEEP_ALIVE));
        res.send_fairness(QUIC_SEND_FAIRNESS);

        res
    };

    let mut config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(crypto).unwrap()));
    config.transport_config(Arc::new(transport_config));

    config
}

pub fn create_client_endpoint(
    bind_addr: SocketAddr,
    client_config: ClientConfig,
) -> anyhow::Result<Endpoint> {
    let mut endpoint =
        Endpoint::client(bind_addr).map_err(|e| anyhow!("Failed to create endpoint: {:?}", e))?;
    endpoint.set_default_client_config(client_config);
    Ok(endpoint)
}
