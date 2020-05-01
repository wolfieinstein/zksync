use crate::mempool::MempoolRequest;
use actix_cors::Cors;
use actix_web::{
    middleware,
    web::{self},
    App, HttpResponse, HttpServer, Result as ActixResult,
};
use futures::channel::mpsc;
use models::config_options::ThreadPanicNotify;
use models::node::{
    Account, AccountId, Address, ExecutedOperations, FranklinPriorityOp, PubKeyHash,
};
use models::NetworkStatus;
use std::net::SocketAddr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use storage::{ConnectionPool, StorageProcessor};
use tokio::{runtime::Runtime, time};
use web3::types::H160;

use super::rpc_server::get_ongoing_priority_ops;
use crate::eth_watch::EthWatchRequest;
use storage::chain::operations_ext::records::TransactionsHistoryItem;

#[derive(Default, Clone)]
struct SharedNetworkStatus(Arc<RwLock<NetworkStatus>>);

impl SharedNetworkStatus {
    #[allow(dead_code)]
    fn read(&self) -> NetworkStatus {
        (*self.0.as_ref().read().unwrap()).clone()
    }
}

/// AppState is a collection of records cloned by each thread to shara data between them
#[derive(Clone)]
struct AppState {
    connection_pool: ConnectionPool,
    network_status: SharedNetworkStatus,
    contract_address: String,
    mempool_request_sender: mpsc::Sender<MempoolRequest>,
    eth_watcher_request_sender: mpsc::Sender<EthWatchRequest>,
}

impl AppState {
    fn access_storage(&self) -> ActixResult<StorageProcessor> {
        self.connection_pool
            .access_storage_fragile()
            .map_err(|_| HttpResponse::RequestTimeout().finish().into())
    }

    // Spawns future updating SharedNetworkStatus in the current `actix::System`
    fn spawn_network_status_updater(&self, panic_notify: mpsc::Sender<bool>) {
        let state = self.clone();

        std::thread::Builder::new()
            .name("rest-state-updater".to_string())
            .spawn(move || {
                let _panic_sentinel = ThreadPanicNotify(panic_notify.clone());

                let mut runtime = Runtime::new().expect("tokio runtime creation");

                let state_update_task = async move {
                    let mut timer = time::interval(Duration::from_millis(1000));
                    loop {
                        timer.tick().await;

                        let storage = state.connection_pool.access_storage().expect("db failed");

                        let last_verified = storage
                            .chain()
                            .block_schema()
                            .get_last_verified_block()
                            .unwrap_or(0);
                        let status = NetworkStatus {
                            next_block_at_max: None,
                            last_committed: storage
                                .chain()
                                .block_schema()
                                .get_last_committed_block()
                                .unwrap_or(0),
                            last_verified,
                            total_transactions: storage
                                .chain()
                                .stats_schema()
                                .count_total_transactions()
                                .unwrap_or(0),
                            outstanding_txs: storage
                                .chain()
                                .stats_schema()
                                .count_outstanding_proofs(last_verified)
                                .unwrap_or(0),
                        };

                        // save status to state
                        *state.network_status.0.as_ref().write().unwrap() = status;
                    }
                };
                runtime.block_on(state_update_task);
            })
            .expect("State update thread");
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct TestnetConfigResponse {
    contract_address: String,
}

fn handle_get_testnet_config(data: web::Data<AppState>) -> ActixResult<HttpResponse> {
    let contract_address = data.contract_address.clone();
    Ok(HttpResponse::Ok().json(TestnetConfigResponse { contract_address }))
}

fn handle_get_network_status(data: web::Data<AppState>) -> ActixResult<HttpResponse> {
    let network_status = data.network_status.read();
    Ok(HttpResponse::Ok().json(network_status))
}

#[derive(Debug, Serialize)]
struct AccountStateResponse {
    // None if account is not created yet.
    id: Option<AccountId>,
    commited: Account,
    verified: Account,
}

fn handle_get_account_state(
    data: web::Data<AppState>,
    account_address: web::Path<Address>,
) -> ActixResult<HttpResponse> {
    let storage = data.access_storage()?;

    let (id, verified, commited) = {
        let stored_account_state = storage
            .chain()
            .account_schema()
            .account_state_by_address(&account_address)
            .map_err(|_| HttpResponse::InternalServerError().finish())?;

        let empty_state = |address: &Address| {
            let mut acc = Account::default();
            acc.address = *address;
            acc
        };

        let id = stored_account_state.committed.as_ref().map(|(id, _)| *id);
        let committed = stored_account_state
            .committed
            .map(|(_, acc)| acc)
            .unwrap_or_else(|| empty_state(&account_address));
        let verified = stored_account_state
            .verified
            .map(|(_, acc)| acc)
            .unwrap_or_else(|| empty_state(&account_address));

        (id, verified, committed)
    };

    let res = AccountStateResponse {
        id,
        commited,
        verified,
    };

    Ok(HttpResponse::Ok().json(res))
}

fn handle_get_tokens(data: web::Data<AppState>) -> ActixResult<HttpResponse> {
    let storage = data.access_storage()?;
    let tokens = storage
        .tokens_schema()
        .load_tokens()
        .map_err(|_| HttpResponse::InternalServerError().finish())?;

    let mut vec_tokens = tokens.values().cloned().collect::<Vec<_>>();
    vec_tokens.sort_by_key(|t| t.id);

    Ok(HttpResponse::Ok().json(vec_tokens))
}

fn handle_get_account_transactions(
    data: web::Data<AppState>,
    address: web::Path<PubKeyHash>,
) -> ActixResult<HttpResponse> {
    let storage = data.access_storage()?;
    let txs = storage
        .chain()
        .operations_ext_schema()
        .get_account_transactions(&address)
        .map_err(|_| HttpResponse::InternalServerError().finish())?;
    Ok(HttpResponse::Ok().json(txs))
}

fn handle_get_account_transactions_history(
    data: web::Data<AppState>,
    request_path: web::Path<(Address, i64, i64)>,
) -> ActixResult<HttpResponse> {
    let (address, mut offset, mut limit) = request_path.into_inner();

    const MAX_LIMIT: i64 = 100;
    if limit > MAX_LIMIT || limit < 0 || offset < 0 {
        return Err(HttpResponse::BadRequest().finish().into());
    }

    let eth_watcher_request_sender = data.eth_watcher_request_sender.clone();

    let storage = data.access_storage()?;
    let tokens = storage
        .tokens_schema()
        .load_tokens()
        .map_err(|_| HttpResponse::InternalServerError().finish())?;

    // Fetch ongoing deposits, since they must be reported within the transactions history.
    let mut ongoing_ops = futures::executor::block_on(async move {
        get_ongoing_priority_ops(&eth_watcher_request_sender).await
    })
    .map_err(|_| HttpResponse::InternalServerError().finish())?;

    ongoing_ops.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0));

    // Filter only deposits for the requested address.
    // `map` is used after filter to find the max block number without an
    // additional list pass.
    // `take` is used last to limit the amount of entries.
    let mut transactions_history: Vec<_> = ongoing_ops
        .iter()
        .filter(|(_block, op)| {
            if let FranklinPriorityOp::Deposit(deposit) = &op.data {
                // Address may be set to either sender or recipient.
                deposit.from == address || deposit.to == address
            } else {
                false
            }
        })
        .map(|(_block, op)| {
            let deposit = op.data.try_get_deposit().unwrap();
            let token_symbol = tokens
                .get(&deposit.token)
                .map(|t| t.symbol.clone())
                .unwrap_or("unknown".into());

            let hash_str = format!("0x{}", hex::encode(&op.eth_hash));
            let pq_id = Some(op.serial_id as i64);

            // Account ID may not exist for depositing ops, so it'll be `null`.
            let account_id: Option<u32> = None;

            // Copy the JSON representation of the executed tx so the appearance
            // will be the same as for txs from storage.
            let tx_json = serde_json::json!({
                "account_id": account_id,
                "eth_fee": op.eth_fee.to_string(),
                "priority_op": {
                    "amount": deposit.amount,
                    "from": deposit.from,
                    "to": deposit.to,
                    "token": token_symbol
                },
                "type": "Deposit"
            });

            TransactionsHistoryItem {
                hash: Some(hash_str),
                pq_id,
                tx: tx_json,
                success: None,
                fail_reason: None,
                commited: false,
                verified: false,
            }
        })
        .skip(offset as usize)
        .take(limit as usize)
        .collect();

    if !transactions_history.is_empty() {
        // We've taken at least one transaction, this means
        // offset is consumed completely, and limit is reduced.
        offset = 0;
        limit -= transactions_history.len() as i64;
    } else {
        // We didn't take any items, so we just decrement offset.
        offset -= ongoing_ops.len() as i64;
    };

    let mut storage_transactions = storage
        .chain()
        .operations_ext_schema()
        .get_account_transactions_history(&address, offset, limit)
        .map_err(|_| HttpResponse::InternalServerError().finish())?;

    transactions_history.append(&mut storage_transactions);

    Ok(HttpResponse::Ok().json(transactions_history))
}

fn handle_get_executed_transaction_by_hash(
    data: web::Data<AppState>,
    tx_hash_hex: web::Path<String>,
) -> ActixResult<HttpResponse> {
    if tx_hash_hex.len() < 2 {
        return Err(HttpResponse::BadRequest().finish().into());
    }
    let transaction_hash = hex::decode(&tx_hash_hex.into_inner()[2..])
        .map_err(|_| HttpResponse::BadRequest().finish())?;

    let storage = data.access_storage()?;
    if let Ok(tx) = storage
        .chain()
        .operations_ext_schema()
        .tx_receipt(transaction_hash.as_slice())
    {
        Ok(HttpResponse::Ok().json(tx))
    } else {
        Ok(HttpResponse::Ok().json(()))
    }
}

fn handle_get_tx_by_hash(
    data: web::Data<AppState>,
    hash_hex_with_prefix: web::Path<String>,
) -> ActixResult<HttpResponse> {
    if hash_hex_with_prefix.len() < 2 {
        return Err(HttpResponse::BadRequest().finish().into());
    }

    let hash = {
        let hash = if hash_hex_with_prefix.starts_with("0x") {
            hex::decode(&hash_hex_with_prefix.into_inner()[2..])
        } else if hash_hex_with_prefix.starts_with("sync-tx:") {
            hex::decode(&hash_hex_with_prefix.into_inner()[8..])
        } else {
            return Err(HttpResponse::BadRequest().finish().into());
        };

        hash.map_err(|_| HttpResponse::BadRequest().finish())?
    };

    let storage = data.access_storage()?;

    let res = storage
        .chain()
        .operations_ext_schema()
        .get_tx_by_hash(hash.as_slice())
        .map_err(|_| HttpResponse::InternalServerError().finish())?;

    Ok(HttpResponse::Ok().json(res))
}

fn handle_get_priority_op_receipt(
    data: web::Data<AppState>,
    id: web::Path<u32>,
) -> ActixResult<HttpResponse> {
    let storage = data.access_storage()?;

    let res = storage
        .chain()
        .operations_ext_schema()
        .get_priority_op_receipt(id.into_inner())
        .map_err(|_| HttpResponse::InternalServerError().finish())?;

    Ok(HttpResponse::Ok().json(res))
}

fn handle_get_transaction_by_id(
    data: web::Data<AppState>,
    path: web::Path<(u32, u32)>,
) -> ActixResult<HttpResponse> {
    let (block_id, tx_id) = path.into_inner();

    let storage = data.access_storage()?;

    let executed_ops = storage
        .chain()
        .block_schema()
        .get_block_executed_ops(block_id)
        .map_err(|_| HttpResponse::InternalServerError().finish())?;

    if let Some(exec_op) = executed_ops.get(tx_id as usize) {
        Ok(HttpResponse::Ok().json(exec_op))
    } else {
        Err(HttpResponse::NotFound().finish().into())
    }
}

#[derive(Deserialize)]
struct HandleBlocksQuery {
    max_block: Option<u32>,
    limit: Option<u32>,
}

fn handle_get_blocks(
    data: web::Data<AppState>,
    query: web::Query<HandleBlocksQuery>,
) -> ActixResult<HttpResponse> {
    let max_block = query.max_block.unwrap_or(999_999_999);
    let limit = query.limit.unwrap_or(20);
    if limit > 100 {
        return Err(HttpResponse::BadRequest().finish().into());
    }
    let storage = data.access_storage()?;

    let resp = storage
        .chain()
        .block_schema()
        .load_block_range(max_block, limit)
        .map_err(|e| {
            warn!("handle_get_blocks db fail: {}", e);
            HttpResponse::InternalServerError().finish()
        })?;
    Ok(HttpResponse::Ok().json(resp))
}

fn handle_get_block_by_id(
    data: web::Data<AppState>,
    block_id: web::Path<u32>,
) -> ActixResult<HttpResponse> {
    let storage = data.access_storage()?;
    let mut blocks = storage
        .chain()
        .block_schema()
        .load_block_range(block_id.into_inner(), 1)
        .map_err(|_| HttpResponse::InternalServerError().finish())?;
    if let Some(block) = blocks.pop() {
        Ok(HttpResponse::Ok().json(block))
    } else {
        Err(HttpResponse::NotFound().finish().into())
    }
}

fn handle_get_block_transactions(
    data: web::Data<AppState>,
    path: web::Path<u32>,
) -> ActixResult<HttpResponse> {
    let block_id = path.into_inner();

    let storage = data.access_storage()?;

    let executed_ops = storage
        .chain()
        .block_schema()
        .get_block_executed_ops(block_id)
        .map_err(|_| HttpResponse::InternalServerError().finish())?
        .into_iter()
        .filter(|op| match op {
            ExecutedOperations::Tx(tx) => tx.op.is_some(),
            _ => true,
        })
        .collect::<Vec<_>>();

    #[derive(Serialize)]
    struct ExecutedOperationWithHash {
        op: ExecutedOperations,
        tx_hash: String,
    };

    let executed_ops_with_hashes = executed_ops
        .into_iter()
        .map(|op| {
            let tx_hash = match &op {
                ExecutedOperations::Tx(tx) => tx.tx.hash().to_string(),
                ExecutedOperations::PriorityOp(tx) => {
                    format!("0x{}", hex::encode(&tx.priority_op.eth_hash))
                }
            };

            ExecutedOperationWithHash { op, tx_hash }
        })
        .collect::<Vec<_>>();

    Ok(HttpResponse::Ok().json(executed_ops_with_hashes))
}

#[derive(Deserialize)]
struct BlockSearchQuery {
    query: String,
}

fn handle_block_search(
    data: web::Data<AppState>,
    query: web::Query<BlockSearchQuery>,
) -> ActixResult<HttpResponse> {
    let storage = data.access_storage()?;
    let result = storage
        .chain()
        .block_schema()
        .find_block_by_height_or_hash(query.into_inner().query);
    if let Some(block) = result {
        Ok(HttpResponse::Ok().json(block))
    } else {
        Err(HttpResponse::NotFound().finish().into())
    }
}

fn start_server(state: AppState, bind_to: SocketAddr) {
    HttpServer::new(move || {
        App::new()
            .data(state.clone())
            .wrap(middleware::Logger::default())
            .wrap(Cors::new().send_wildcard().max_age(3600))
            .service(
                web::scope("/api/v0.1")
                    .route(
                        "/blocks/{block_id}/transactions",
                        web::get().to(handle_get_block_transactions),
                    )
                    .route("/testnet_config", web::get().to(handle_get_testnet_config))
                    .route("/status", web::get().to(handle_get_network_status))
                    .route(
                        "/account/{address}",
                        web::get().to(handle_get_account_state),
                    )
                    .route("/tokens", web::get().to(handle_get_tokens))
                    .route(
                        "/account/{id}/transactions",
                        web::get().to(handle_get_account_transactions),
                    )
                    .route(
                        "/account/{address}/history/{offset}/{limit}",
                        web::get().to(handle_get_account_transactions_history),
                    )
                    .route(
                        "/transactions/{tx_hash}",
                        web::get().to(handle_get_executed_transaction_by_hash),
                    )
                    .route(
                        "/transactions_all/{tx_hash}",
                        web::get().to(handle_get_tx_by_hash),
                    )
                    .route(
                        "/priority_operations/{pq_id}/",
                        web::get().to(handle_get_priority_op_receipt),
                    )
                    .route(
                        "/blocks/{block_id}/transactions/{tx_id}",
                        web::get().to(handle_get_transaction_by_id),
                    )
                    .route(
                        "/blocks/{block_id}/transactions",
                        web::get().to(handle_get_block_transactions),
                    )
                    .route("/blocks/{block_id}", web::get().to(handle_get_block_by_id))
                    .route("/blocks", web::get().to(handle_get_blocks))
                    .route("/search", web::get().to(handle_block_search)),
            )
            // Endpoint needed for js isReachable
            .route(
                "/favicon.ico",
                web::get().to(|| HttpResponse::Ok().finish()),
            )
    })
    .bind(bind_to)
    .unwrap()
    .shutdown_timeout(1)
    .start();
}

/// Start HTTP REST API
pub(super) fn start_server_thread_detached(
    connection_pool: ConnectionPool,
    listen_addr: SocketAddr,
    contract_address: H160,
    mempool_request_sender: mpsc::Sender<MempoolRequest>,
    eth_watcher_request_sender: mpsc::Sender<EthWatchRequest>,
    panic_notify: mpsc::Sender<bool>,
) {
    std::thread::Builder::new()
        .name("actix-rest-api".to_string())
        .spawn(move || {
            let _panic_sentinel = ThreadPanicNotify(panic_notify.clone());

            let runtime = actix_rt::System::new("api-server");

            let state = AppState {
                connection_pool,
                network_status: SharedNetworkStatus::default(),
                contract_address: format!("{:?}", contract_address),
                mempool_request_sender,
                eth_watcher_request_sender,
            };
            state.spawn_network_status_updater(panic_notify);

            start_server(state, listen_addr);
            runtime.run().unwrap_or_default();
        })
        .expect("Api server thread");
}
