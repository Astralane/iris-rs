#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Failed to connect to the RPC server: {0}")]
    RpcConnectionError(String),

    #[error("Failed to fetch leader schedule: {0}")]
    LeaderScheduleFetchError(String),

    #[error("Failed to create pubsub client: {0}")]
    PubSubClientCreationError(String),

    #[error("I/O error: {0}")]
    IoError(#[from] std::io::Error),

    #[error("PubsSubClient error: {0}")]
    PubSubError(#[from] solana_client::pubsub_client::PubsubClientError),

    #[error("Failed to get slot updates from any rpcs")]
    SlotUpdaterTimeout,

    #[error("RPC client error: {0}")]
    RpcClientError(#[from] solana_client::client_error::ClientError),
}
