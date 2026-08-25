use wincode::{SchemaRead, SchemaWrite};

#[derive(SchemaWrite, SchemaRead)]
pub struct TransactionPacket {
    pub wire_transaction: Vec<u8>,
    pub mev_protect: bool,
    // Kept in the wire schema for compatibility with existing Iris QUIC senders.
    // Retry behavior has been removed, so the receiver intentionally ignores it.
    pub max_retry: Option<u16>,
}

#[cfg(test)]
mod tests {
    use super::TransactionPacket;

    #[test]
    fn decodes_legacy_iris_quic_packet() {
        let encoded = wincode::serialize(&TransactionPacket {
            wire_transaction: vec![1, 2, 3],
            mev_protect: true,
            max_retry: Some(3),
        })
        .unwrap();

        let decoded: TransactionPacket = wincode::deserialize(&encoded).unwrap();
        assert_eq!(decoded.wire_transaction, vec![1, 2, 3]);
        assert!(decoded.mev_protect);
        assert_eq!(decoded.max_retry, Some(3));
    }
}
