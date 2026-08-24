use crate::rpc::IrisRpcServer;
use crate::tpu_next_client::{TpuClientNextSender, TpuClientPayload};
use crate::vendor::solana_rpc::decode_transaction;
use agave_transaction_view::transaction_view::TransactionView;
use jsonrpsee::core::{async_trait, RpcResult};
use jsonrpsee::types::error::INVALID_PARAMS_CODE;
use jsonrpsee::types::ErrorObjectOwned;
use metrics::counter;
use solana_rpc_client_api::config::RpcSendTransactionConfig;
use solana_transaction_status_client_types::UiTransactionEncoding;
use tracing::error;

pub struct IrisRpcServerImpl {
    tpu_sender: TpuClientNextSender,
}

pub fn invalid_request(reason: &str) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        INVALID_PARAMS_CODE,
        format!("Invalid Request: {reason}"),
        None::<String>,
    )
}

impl IrisRpcServerImpl {
    pub fn new(tpu_sender: TpuClientNextSender) -> Self {
        Self { tpu_sender }
    }
}
#[async_trait]
impl IrisRpcServer for IrisRpcServerImpl {
    async fn health(&self) -> String {
        format!("Ok({})", env!("CARGO_PKG_VERSION"))
    }

    async fn send_transaction(
        &self,
        txn: String,
        params: Option<RpcSendTransactionConfig>,
        mev_protect: Option<bool>,
    ) -> RpcResult<String> {
        counter!("iris_txn_total_transactions").increment(1);
        let mev_protect = mev_protect.unwrap_or(false);
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
                return Err(e);
            }
        };
        let tx_view =
            TransactionView::try_new_unsanitized(wire_transaction.as_ref()).map_err(|e| {
                counter!("iris_error", "type" => "cannot_deserialize_transaction").increment(1);
                error!("cannot deserialize transaction: {:?}", e);
                invalid_request("cannot deserialize transaction")
            })?;
        let signature = tx_view
            .signatures()
            .first()
            .copied()
            .ok_or_else(|| invalid_request("transaction has no signatures"))?;
        self.tpu_sender
            .send_transaction(TpuClientPayload::new(wire_transaction, mev_protect));
        Ok(signature.to_string())
    }
}
