use crate::tpu_next_client::TpuClientPayload;
use rand::distributions::Alphanumeric;
use rand::Rng;
use solana_sdk::signature::Signature;
use wincode::{SchemaRead, SchemaWrite};

//(packet, received time, received method)

#[derive(Clone, Debug)]
pub enum PacketSource {
    Quic,
    JsonRpc,
}

#[derive(SchemaWrite, SchemaRead)]
pub struct TransactionPacket {
    pub wire_transaction: Vec<u8>,
    pub mev_protect: bool,
    pub max_retry: Option<u8>,
}

pub trait SendTransactionClient: Send + Sync {
    fn send_transaction(&self, txn: TpuClientPayload);
    fn send_transaction_batch(&self, wire_transaction: Vec<TpuClientPayload>);
}

pub trait ChainStateClient: Send + Sync {
    fn get_slot(&self) -> u64;
    fn confirm_signature_status(&self, signature: &Signature) -> Option<u64>;
}

pub fn generate_random_string(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}
