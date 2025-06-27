use crate::traits::BlockListProvider;
use anyhow::Context;
use async_trait::async_trait;
use borsh::{BorshDeserialize, BorshSerialize};
use smart_rpc_client::rpc_provider::SmartRpcClientProvider;
use solana_sdk::pubkey::Pubkey;
use std::net::SocketAddr;
use std::sync::Arc;
use tracing::warn;

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, Eq, PartialEq)]
pub enum PermissionStrategy {
    Deny,
    Allow,
}

impl TryFrom<u8> for PermissionStrategy {
    type Error = std::io::Error;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(PermissionStrategy::Deny),
            1 => Ok(PermissionStrategy::Allow),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "Invalid permission strategy",
            )),
        }
    }
}
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, Eq, PartialEq)]
pub struct Policy {
    pub kind: u8,
    pub strategy: u8,
    pub nonce: u8,
    pub identities_len: [u8; 4],
}
impl Policy {
    pub const LEN: usize = 7;

    pub fn from_slice(data: &[u8]) -> Result<Self, std::io::Error> {
        let mut data = data;
        Self::deserialize(&mut data)
    }
    pub fn try_strategy(&self) -> Result<PermissionStrategy, std::io::Error> {
        self.strategy.try_into()
    }
    pub fn try_deserialize_identities(data: &[u8]) -> Vec<Pubkey> {
        let identities_data = &data[Policy::LEN..];
        identities_data
            .chunks_exact(32)
            .filter_map(|chunk| Pubkey::try_from_slice(chunk).ok())
            .collect::<Vec<Pubkey>>()
    }
}
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, Eq, PartialEq)]
pub struct PolicyV2 {
    pub kind: u8,
    pub strategy: u8,
    pub nonce: u8,
    pub mint: Pubkey,
    pub identities_len: [u8; 4],
}

impl PolicyV2 {
    pub const LEN: usize = 39;

    pub fn from_slice(data: &[u8]) -> Result<Self, std::io::Error> {
        let mut data = data;
        Self::deserialize(&mut data)
    }
    pub fn try_strategy(&self) -> Result<PermissionStrategy, std::io::Error> {
        self.strategy.try_into()
    }
    pub fn try_deserialize_identities(data: &[u8]) -> Vec<Pubkey> {
        let identities_data = &data[PolicyV2::LEN..];
        identities_data
            .chunks_exact(32)
            .filter_map(|chunk| Pubkey::try_from_slice(chunk).ok())
            .collect::<Vec<Pubkey>>()
    }
}
pub struct YellowstoneShieldProvider {
    key: Pubkey,
    rpc: Arc<SmartRpcClientProvider>,
}

impl YellowstoneShieldProvider {
    pub fn new(key: Pubkey, rpc: Arc<SmartRpcClientProvider>) -> Self {
        Self { key, rpc }
    }
    async fn get_blocked_ips(&self) -> anyhow::Result<Vec<SocketAddr>> {
        let cluster_nodes = self.rpc.best_rpc().get_cluster_nodes().await?;
        let blocked_identities = self.get_blocked_identities().await?;
        let tpu_quics_addrs = cluster_nodes
            .iter()
            .filter_map(|node| {
                if blocked_identities.contains(&node.pubkey) {
                    node.tpu_quic
                } else {
                    None
                }
            })
            .collect();
        Ok(tpu_quics_addrs)
    }

    async fn get_blocked_identities(&self) -> anyhow::Result<Vec<String>> {
        let data = self
            .rpc
            .best_rpc()
            .get_account_data(&self.key)
            .await
            .context("cannot fetch")?;

        let (strategy, identities) = match &data[0] {
            0 => {
                let policy = Policy::from_slice(&data).context("cannot deserialize policy")?;
                let strategy = policy
                    .try_strategy()
                    .context("invalid permission strategy")?;
                let identities = Policy::try_deserialize_identities(&data);
                (strategy, identities)
            }
            1 => {
                let policy = PolicyV2::from_slice(&data).context("cannot deserialize policy_v2")?;
                let strategy = policy.try_strategy()?;
                let identities = PolicyV2::try_deserialize_identities(&data);
                (strategy, identities)
            }
            _ => return Err(anyhow::anyhow!("Unknown policy type")),
        };

        if matches!(strategy, PermissionStrategy::Allow) {
            return Err(anyhow::anyhow!(
                "Shield policy for key {} is set to Allow, which is not supported",
                &self.key,
            ));
        };

        if identities.is_empty() {
            warn!(
                "Yellowstone shield is enabled, but no identities found for key: {}",
                &self.key
            );
        }
        Ok(identities
            .iter()
            .map(Pubkey::to_string)
            .collect::<Vec<String>>())
    }
}

#[async_trait]
impl BlockListProvider for YellowstoneShieldProvider {
    async fn fetch_blocked_leaders(&self) -> anyhow::Result<Vec<SocketAddr>> {
        let response = self.get_blocked_ips().await?;
        Ok(response)
    }
}

#[cfg(test)]
pub mod test {
    use smart_rpc_client::rpc_provider::SmartRpcClientProvider;
    use std::str::FromStr;
    use std::sync::Arc;
    use std::time::Duration;

    #[tokio::test]
    pub async fn test_get_identities_for_key() {
        let rpc_provider = Arc::new(SmartRpcClientProvider::new(
            &["http://rpc:8899".to_string()],
            Duration::from_secs(5),
            None,
        ));
        let key =
            solana_sdk::pubkey::Pubkey::from_str("8LXyNpkdnCKCCaUWBku1wD2B1HHaZ41FRgFEN6jNmytv")
                .unwrap();
        let provider = super::YellowstoneShieldProvider::new(key, rpc_provider);
        let identities = provider.get_blocked_identities().await.unwrap();
        assert_eq!(identities.len() > 0, true);
        let addresses = provider.get_blocked_ips().await.unwrap();
        assert_eq!(addresses.len() > 0, true);
    }
}
