//! Transactions part of API implementation.

// Built-in uses
use std::convert::TryInto;

// External uses
use actix_web::{
    web::{self, Json},
    Scope,
};
use hex::FromHexError;

// Workspace uses
use zksync_storage::{QueryResult, StorageProcessor};
use zksync_types::{
    aggregated_operations::AggregatedActionType, tx::EthSignData, tx::TxEthSignature, tx::TxHash,
    BlockNumber,
};

// Local uses
use super::{
    error::{Error, TxError},
    response::ApiResult,
    types::{
        IncomingTx, IncomingTxBatch, L1Receipt, L2Receipt, L2Status, Receipt, Transaction, TxData,
    },
};
use crate::api_server::rpc_server::types::TxWithSignature;
use crate::api_server::tx_sender::TxSender;

/// Shared data between `api/v0.2/transaction` endpoints.
#[derive(Clone)]
struct ApiTransactionData {
    tx_sender: TxSender,
}

impl ApiTransactionData {
    fn new(tx_sender: TxSender) -> Self {
        Self { tx_sender }
    }

    fn decode_hash(&self, tx_hash: String) -> Result<Vec<u8>, FromHexError> {
        let tx_hash: &str = if let Some(value) = (&tx_hash).strip_prefix("0x") {
            value
        } else {
            &tx_hash
        };
        hex::decode(tx_hash)
    }

    async fn is_block_finalized(
        storage: &mut StorageProcessor<'_>,
        block_number: BlockNumber,
    ) -> bool {
        storage
            .chain()
            .operations_schema()
            .get_stored_aggregated_operation(block_number, AggregatedActionType::ExecuteBlocks)
            .await
            .map(|operation| operation.confirmed)
            .unwrap_or(false)
    }

    async fn get_l1_receipt(
        storage: &mut StorageProcessor<'_>,
        eth_hash: &[u8],
    ) -> QueryResult<Option<L1Receipt>> {
        if let Some(op) = storage
            .chain()
            .operations_schema()
            .get_executed_priority_operation_by_hash(eth_hash)
            .await?
        {
            let finalized =
                Self::is_block_finalized(storage, BlockNumber(op.block_number as u32)).await;
            Ok(Some(L1Receipt::from((op, finalized))))
        } else {
            Ok(None)
        }
    }

    async fn get_l2_receipt(
        storage: &mut StorageProcessor<'_>,
        tx_hash: TxHash,
    ) -> QueryResult<Option<L2Receipt>> {
        if let Some(receipt) = storage
            .chain()
            .operations_ext_schema()
            .tx_receipt(tx_hash.as_ref())
            .await?
        {
            Ok(Some(L2Receipt::from(receipt)))
        } else if storage
            .chain()
            .mempool_schema()
            .contains_tx(tx_hash)
            .await?
        {
            Ok(Some(L2Receipt {
                tx_hash,
                rollup_block: None,
                status: L2Status::Queued,
                fail_reason: None,
            }))
        } else {
            Ok(None)
        }
    }

    async fn tx_status(&self, tx_hash: &[u8; 32]) -> QueryResult<Option<Receipt>> {
        let mut storage = self.tx_sender.pool.access_storage().await?;
        if let Some(receipt) = Self::get_l1_receipt(&mut storage, tx_hash).await? {
            Ok(Some(Receipt::L1(receipt)))
        } else if let Some(receipt) =
            Self::get_l2_receipt(&mut storage, TxHash::from_slice(tx_hash).unwrap()).await?
        {
            Ok(Some(Receipt::L2(receipt)))
        } else {
            Ok(None)
        }
    }

    fn get_sign_bytes(eth_sign_data: EthSignData) -> String {
        let mut result = String::from("0x");
        match eth_sign_data.signature {
            TxEthSignature::EthereumSignature(sign) => {
                result.push_str(hex::encode(sign.serialize_packed()).as_str())
            }
            TxEthSignature::EIP1271Signature(sign) => result.push_str(hex::encode(sign.0).as_str()),
        }
        result
    }

    async fn get_l1_tx_data(
        storage: &mut StorageProcessor<'_>,
        eth_hash: &[u8],
    ) -> QueryResult<Option<TxData>> {
        let operation = storage
            .chain()
            .operations_schema()
            .get_executed_priority_operation_by_hash(eth_hash)
            .await?;
        if let Some(op) = operation {
            let block_number = BlockNumber(op.block_number as u32);
            let finalized = Self::is_block_finalized(storage, block_number).await;

            let status = if finalized {
                L2Status::Finalized
            } else {
                L2Status::Committed
            };
            let tx = Transaction {
                tx_hash: TxHash::from_slice(eth_hash).unwrap(),
                block_number: Some(block_number),
                op: op.operation,
                status,
                fail_reason: None,
                created_at: op.created_at,
            };

            Ok(Some(TxData {
                tx,
                eth_signature: None,
            }))
        } else {
            Ok(None)
        }
    }

    async fn get_l2_tx_data(
        storage: &mut StorageProcessor<'_>,
        tx_hash: TxHash,
    ) -> QueryResult<Option<TxData>> {
        let operation = storage
            .chain()
            .operations_schema()
            .get_executed_operation(tx_hash.as_ref())
            .await?;

        if let Some(op) = operation {
            let block_number = BlockNumber(op.block_number as u32);
            let finalized = Self::is_block_finalized(storage, block_number).await;

            let status = if op.success {
                if finalized {
                    L2Status::Finalized
                } else {
                    L2Status::Committed
                }
            } else {
                L2Status::Rejected
            };
            let tx = Transaction {
                tx_hash,
                block_number: Some(block_number),
                op: op.tx,
                status,
                fail_reason: op.fail_reason,
                created_at: op.created_at,
            };
            let eth_sign_data: Option<EthSignData> =
                op.eth_sign_data.map(serde_json::from_value).transpose()?;
            let eth_signature = eth_sign_data.map(Self::get_sign_bytes);

            Ok(Some(TxData { tx, eth_signature }))
        } else if let Some(op) = storage
            .chain()
            .mempool_schema()
            .get_mempool_tx(tx_hash)
            .await?
        {
            let tx = Transaction {
                tx_hash,
                block_number: None,
                op: op.tx,
                status: L2Status::Queued,
                fail_reason: None,
                created_at: op.created_at,
            };

            let eth_sign_data: Option<EthSignData> =
                op.eth_sign_data.map(serde_json::from_value).transpose()?;
            let eth_signature = eth_sign_data.map(Self::get_sign_bytes);

            Ok(Some(TxData { tx, eth_signature }))
        } else {
            Ok(None)
        }
    }

    async fn tx_data(&self, tx_hash: &[u8; 32]) -> QueryResult<Option<TxData>> {
        let mut storage = self.tx_sender.pool.access_storage().await?;
        if let Some(tx_data) = Self::get_l1_tx_data(&mut storage, tx_hash).await? {
            Ok(Some(tx_data))
        } else if let Some(tx_data) =
            Self::get_l2_tx_data(&mut storage, TxHash::from_slice(tx_hash).unwrap()).await?
        {
            Ok(Some(tx_data))
        } else {
            Ok(None)
        }
    }
}

// Server implementation

async fn tx_status(
    data: web::Data<ApiTransactionData>,
    web::Path(tx_hash): web::Path<String>,
) -> ApiResult<Option<Receipt>> {
    let decode_result = data.decode_hash(tx_hash);
    match decode_result {
        Ok(tx_hash) => {
            let tx_hash_result: Result<&[u8; 32], _> = tx_hash.as_slice().try_into();
            match tx_hash_result {
                Ok(tx_hash) => {
                    let tx_status = data.tx_status(&tx_hash).await;
                    tx_status.map_err(Error::internal).into()
                }
                Err(_) => Error::from(TxError::IncorrectHash).into(),
            }
        }
        Err(err) => Error::from(err).into(),
    }
}

async fn tx_data(
    data: web::Data<ApiTransactionData>,
    web::Path(tx_hash): web::Path<String>,
) -> ApiResult<Option<TxData>> {
    let decode_result = data.decode_hash(tx_hash);
    match decode_result {
        Ok(tx_hash) => {
            let tx_hash_result: Result<&[u8; 32], _> = tx_hash.as_slice().try_into();
            match tx_hash_result {
                Ok(tx_hash) => {
                    let tx_data = data.tx_data(&tx_hash).await;
                    tx_data.map_err(Error::internal).into()
                }
                Err(_) => Error::from(TxError::IncorrectHash).into(),
            }
        }
        Err(err) => Error::from(err).into(),
    }
}

async fn submit_tx(
    data: web::Data<ApiTransactionData>,
    Json(body): Json<IncomingTx>,
) -> ApiResult<TxHash> {
    let tx_hash = data
        .tx_sender
        .submit_tx(body.tx, body.signature, None)
        .await
        .map_err(Error::from);

    tx_hash.into()
}

async fn submit_batch(
    data: web::Data<ApiTransactionData>,
    Json(body): Json<IncomingTxBatch>,
) -> ApiResult<Vec<TxHash>> {
    let txs = body
        .txs
        .into_iter()
        .map(|tx| TxWithSignature {
            tx,
            signature: None,
        })
        .collect();

    let signatures = body.signature;
    let tx_hashes = data
        .tx_sender
        .submit_txs_batch(txs, Some(signatures))
        .await
        .map_err(Error::from);

    tx_hashes.into()
}

pub fn api_scope(tx_sender: TxSender) -> Scope {
    let data = ApiTransactionData::new(tx_sender);

    web::scope("transaction")
        .data(data)
        .route("", web::get().to(submit_tx))
        .route("{tx_hash}", web::get().to(tx_status))
        .route("{tx_hash}/data", web::get().to(tx_data))
        .route("/batches", web::post().to(submit_batch))
}
