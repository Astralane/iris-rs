use rand::distributions::Alphanumeric;
use rand::Rng;

pub const MEV_PROTECT_TRUE_PREFIX: &[u8] = &[0x01];
pub const MEV_PROTECT_FALSE_PREFIX: &[u8] = &[0x00];

pub trait SendTransactionClient: Send + Sync {
    fn send_transaction(&self, txn: bytes::Bytes, mev_protect: bool);
    fn send_transaction_batch(&self, wire_transaction: Vec<bytes::Bytes>, mev_protect: bool);
}

pub trait ChainStateClient: Send + Sync {
    fn get_slot(&self) -> u64;
    fn confirm_signature_status(&self, signature: &str) -> Option<u64>;
}

pub fn generate_random_string(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}
