use crate::rpc_server::invalid_request;
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use jsonrpsee::core::RpcResult;
use solana_message::v1::MAX_TRANSACTION_SIZE;
use solana_packet::PACKET_DATA_SIZE;
use solana_sdk::bs58;
use solana_transaction_status_client_types::TransactionBinaryEncoding;

const MAX_BASE58_SIZE: usize = 1683; // Golden, bump if PACKET_DATA_SIZE changes
const MAX_BASE64_SIZE: usize = 5464; // ceil(4096 / 3) * 4
const MAX_BASE64_LEGACY_SIZE: usize = 1644; // ceil(1232 / 3) * 4
const V1_BASE64_PREFIX_LOWER_BOUND: &[u8] = b"gQ";
pub fn decode_transaction(
    encoded: String,
    encoding: TransactionBinaryEncoding,
) -> RpcResult<Vec<u8>> {
    let (wire_output, max_raw_size) = match encoding {
        TransactionBinaryEncoding::Base58 => {
            if encoded.len() > MAX_BASE58_SIZE {
                return Err(invalid_request(&format!(
                    "base58 encoded transaction too large: bytes (max: encoded/raw {MAX_BASE58_SIZE}/{PACKET_DATA_SIZE})",
                )));
            }
            let bytes = bs58::decode(encoded)
                .into_vec()
                .map_err(|e| invalid_request(&format!("invalid base58 encoding: {e:?}")))?;
            (bytes, PACKET_DATA_SIZE)
        }
        TransactionBinaryEncoding::Base64 => {
            let (max_encoded_size, max_raw_size) = if encoded
                .as_bytes()
                .get(..2)
                .is_some_and(|prefix| prefix >= V1_BASE64_PREFIX_LOWER_BOUND)
            {
                (MAX_BASE64_SIZE, MAX_TRANSACTION_SIZE)
            } else {
                (MAX_BASE64_LEGACY_SIZE, PACKET_DATA_SIZE)
            };
            if encoded.len() > max_encoded_size {
                return Err(invalid_request(&format!(
                    "base64 encoded transaction too large: bytes (max: encoded/raw {max_encoded_size}/{max_raw_size})",
                )));
            }
            let bytes = BASE64_STANDARD
                .decode(encoded)
                .map_err(|e| invalid_request(&format!("invalid base64 encoding: {e:?}")))?;
            (bytes, max_raw_size)
        }
    };

    if wire_output.len() > max_raw_size {
        return Err(invalid_request(&format!(
            "decoded transaction too large: {} bytes (max: {max_raw_size} bytes)",
            wire_output.len(),
        )));
    }
    Ok(wire_output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_max_size_v1_base64_payload() {
        let mut transaction = vec![0_u8; MAX_TRANSACTION_SIZE];
        transaction[0] = solana_message::v1::V1_PREFIX;
        let encoded = BASE64_STANDARD.encode(transaction.clone());

        assert_eq!(
            decode_transaction(encoded, TransactionBinaryEncoding::Base64).unwrap(),
            transaction
        );
    }

    #[test]
    fn rejects_legacy_base64_payload_over_packet_size() {
        let transaction = vec![0_u8; PACKET_DATA_SIZE + 1];
        let encoded = BASE64_STANDARD.encode(transaction);

        assert!(decode_transaction(encoded, TransactionBinaryEncoding::Base64).is_err());
    }
}
