use crate::rpc_server::invalid_request;
use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use jsonrpsee::core::RpcResult;
use solana_packet::PACKET_DATA_SIZE;
use solana_sdk::bs58;
use solana_transaction_status::TransactionBinaryEncoding;

const MAX_BASE58_SIZE: usize = 1683; // Golden, bump if PACKET_DATA_SIZE changes
const MAX_BASE64_SIZE: usize = 1644; // Golden, bump if PACKET_DATA_SIZE changes
pub fn decode_transaction(
    encoded: String,
    encoding: TransactionBinaryEncoding,
) -> RpcResult<bytes::Bytes> {
    let wire_output = match encoding {
        TransactionBinaryEncoding::Base58 => {
            if encoded.len() > MAX_BASE58_SIZE {
                return Err(invalid_request(&format!(
                    "base58 encoded {} too large: bytes (max: encoded/raw {}/{})",
                    encoded.len(),
                    MAX_BASE58_SIZE,
                    PACKET_DATA_SIZE,
                )));
            }
            bs58::decode(encoded)
                .into_vec()
                .map_err(|e| invalid_request(&format!("invalid base58 encoding: {e:?}")))?
        }
        TransactionBinaryEncoding::Base64 => {
            if encoded.len() > MAX_BASE64_SIZE {
                return Err(invalid_request(&format!(
                    "base64 encoded {} too large: bytes (max: encoded/raw {}/{})",
                    encoded.len(),
                    MAX_BASE64_SIZE,
                    PACKET_DATA_SIZE,
                )));
            }
            BASE64_STANDARD
                .decode(encoded)
                .map_err(|e| invalid_request(&format!("invalid base64 encoding: {e:?}")))?
        }
    };
    let wire_output = bytes::Bytes::from(wire_output);
    if wire_output.len() > PACKET_DATA_SIZE {
        return Err(invalid_request(&format!(
            "decoded {} too large: bytes (max: {} bytes)",
            wire_output.len(),
            PACKET_DATA_SIZE
        )));
    }
    Ok(wire_output)
}
