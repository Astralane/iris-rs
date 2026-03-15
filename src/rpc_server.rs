use crate::dedup_and_retry::DedupPacketPayload;
use crate::rpc::IrisRpcServer;
use crate::types::{PacketSource, TransactionPacket};
use crate::vendor::solana_rpc::decode_transaction;
use agave_transaction_view::transaction_view::TransactionView;
use crossbeam_channel::{Sender, TrySendError};
use jsonrpsee::core::{async_trait, RpcResult};
use jsonrpsee::types::error::INVALID_PARAMS_CODE;
use jsonrpsee::types::ErrorObjectOwned;
use metrics::counter;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_transaction_status::UiTransactionEncoding;
use std::time::Instant;
use tracing::error;

pub struct IrisRpcServerImpl {
    dedup_sender: Sender<DedupPacketPayload>,
}

pub fn invalid_request(reason: &str) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        INVALID_PARAMS_CODE,
        format!("Invalid Request: {reason}"),
        None::<String>,
    )
}

impl IrisRpcServerImpl {
    pub fn new(dedup_sender: Sender<DedupPacketPayload>) -> Self {
        IrisRpcServerImpl { dedup_sender }
    }
}
#[async_trait]
impl IrisRpcServer for IrisRpcServerImpl {
    async fn health(&self) -> String {
        "Ok(3.0.0)".to_string()
    }

    async fn send_transaction(
        &self,
        txn: String,
        params: Option<RpcSendTransactionConfig>,
        mev_protect: Option<bool>,
    ) -> RpcResult<String> {
        counter!("iris_txn_total_transactions").increment(1);
        let mev_protect = mev_protect.unwrap_or(false);
        let max_retry = params.and_then(|param| param.max_retries);
        let encoding = params
            .and_then(|params| params.encoding)
            .unwrap_or(UiTransactionEncoding::Base64);

        let binary_encoding = encoding.into_binary_encoding().ok_or_else(|| {
            counter!("iris_error", "type" => "invalid_encoding").increment(1);
            invalid_request(&format!(
                "unsupported encoding: {encoding}. Supported encodings: base58, base64"
            ))
        })?;
        let wire_transaction = match decode_transaction(txn, binary_encoding) {
            Ok(wire_transaction) => wire_transaction,
            Err(e) => {
                counter!("iris_error", "type" => "cannot_decode_transaction").increment(1);
                error!("cannot decode transaction: {:?}", e);
                return Err(invalid_request(&e.to_string()));
            }
        };
        let tx_view =
            TransactionView::try_new_unsanitized(wire_transaction.as_ref()).map_err(|e| {
                counter!("iris_error", "type" => "cannot_deserialize_transaction").increment(1);
                error!("cannot deserialize transaction: {:?}", e);
                invalid_request("cannot deserialize transaction")
            })?;
        let signature = tx_view.signatures()[0];
        let packet = TransactionPacket {
            wire_transaction,
            mev_protect,
            max_retry: max_retry.map(|i| i as u16),
        };
        if let Err(e) = self
            .dedup_sender
            .try_send((packet, Instant::now(), PacketSource::JsonRpc))
        {
            match e {
                TrySendError::Full(_) => {
                    counter!("deduper_sender_full").increment(1);
                }
                TrySendError::Disconnected(_) => {
                    // server need to exit
                    counter!("deduper_sender_disconnected").increment(1);
                }
            }
        }
        Ok(signature.to_string())
    }
}
