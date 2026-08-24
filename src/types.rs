use wincode::{SchemaRead, SchemaWrite};

#[derive(SchemaWrite, SchemaRead)]
pub struct TransactionPacket {
    pub wire_transaction: Vec<u8>,
    pub mev_protect: bool,
}
