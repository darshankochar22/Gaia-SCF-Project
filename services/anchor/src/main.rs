//! Gaia anchoring service.
//!
//! Your Gaia backend calls this whenever a batch step happens. The service
//! wraps the step in a record, canonicalises it (RFC 8785), stores the JSON,
//! and anchors its SHA-256 on the Stellar batch-registry contract.
//!
//!   POST /batches                      {batch_id, product}
//!   POST /batches/{id}/events          {kind, data:{...}}
//!   GET  /batches/{id}                 batch + anchors, read from chain
//!   GET  /records/{hash}.json          stored records (local storage mode)

mod chain;
mod storage;

use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chain::{Chain, ContractError, sv_bytes32, sv_string, sv_symbol, to_json};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use storage::Storage;
use tower_http::{cors::CorsLayer, services::ServeDir};

struct App {
    chain: Chain,
    storage: Storage,
    api_key: String,
    contract_id: String,
}

type Shared = Arc<App>;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if let Err(e) = dotenvy::dotenv() {
        eprintln!("note: .env not loaded: {e}");
    }
    tracing_subscriber::fmt::init();

    let env = |k: &str| std::env::var(k).map_err(|_| anyhow::anyhow!("{k} is not set"));
    let contract_id = env("ANCHOR_CONTRACT_ID")?;
    let chain = Chain::new(
        &std::env::var("STELLAR_RPC_URL").unwrap_or_else(|_| "https://soroban-testnet.stellar.org".into()),
        &std::env::var("STELLAR_NETWORK_PASSPHRASE")
            .unwrap_or_else(|_| "Test SDF Network ; September 2015".into()),
        &contract_id,
        &env("STELLAR_SECRET")?,
    )?;
    let app = Arc::new(App {
        chain,
        storage: Storage::from_env()?,
        api_key: env("ANCHOR_API_KEY")?,
        contract_id,
    });

    let mut router = Router::new()
        .route("/health", get(health))
        .route("/batches", post(create_batch))
        .route("/batches/{id}", get(get_batch))
        .route("/batches/{id}/events", post(add_event));
    if let Some(dir) = app.storage.records_dir() {
        router = router.nest_service("/records", ServeDir::new(dir));
    }
    let router = router.layer(CorsLayer::permissive()).with_state(app.clone());

    let addr = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8787".into());
    tracing::info!("anchor service on {addr}, operator {}", app.chain.public_key());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, router).await?;
    Ok(())
}

// ---------- errors ----------

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        let status = match e.downcast_ref::<ContractError>() {
            Some(ContractError(2 | 5)) => StatusCode::CONFLICT,
            Some(ContractError(3)) => StatusCode::NOT_FOUND,
            Some(ContractError(1)) => StatusCode::FORBIDDEN,
            Some(_) => StatusCode::UNPROCESSABLE_ENTITY,
            None => StatusCode::BAD_GATEWAY,
        };
        tracing::warn!("{e:#}");
        ApiError(status, format!("{e:#}"))
    }
}

fn bad(msg: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, msg.into())
}

fn authorize(app: &App, headers: &HeaderMap) -> Result<(), ApiError> {
    let ok = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|k| k == app.api_key);
    if ok { Ok(()) } else { Err(ApiError(StatusCode::UNAUTHORIZED, "bad api key".into())) }
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_/.".contains(c))
}

fn valid_kind(s: &str) -> bool {
    !s.is_empty() && s.len() <= 32 && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

// ---------- handlers ----------

async fn health(State(app): State<Shared>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "operator": app.chain.public_key(),
        "contract": app.contract_id,
    }))
}

#[derive(Deserialize)]
struct NewBatch {
    batch_id: String,
    product: String,
}

async fn create_batch(
    State(app): State<Shared>,
    headers: HeaderMap,
    Json(body): Json<NewBatch>,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    if !valid_id(&body.batch_id) {
        return Err(bad("batch_id: 1-64 chars of A-Z a-z 0-9 - _ / ."));
    }
    if body.product.is_empty() || body.product.len() > 64 {
        return Err(bad("product: 1-64 chars"));
    }
    let res = app
        .chain
        .write(
            "create_batch",
            vec![app.chain.operator(), sv_string(&body.batch_id)?, sv_string(&body.product)?],
        )
        .await?;
    Ok(Json(json!({ "batch_id": body.batch_id, "tx_hash": res.tx_hash, "ledger": res.ledger })))
}

#[derive(Deserialize)]
struct NewEvent {
    kind: String,
    /// Everything about this step. Keep private data (farmer phone, costs) out.
    data: Value,
    /// When the step happened (ISO 8601). Defaults to now.
    occurred_at: Option<String>,
}

async fn add_event(
    State(app): State<Shared>,
    Path(batch_id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<NewEvent>,
) -> Result<Json<Value>, ApiError> {
    authorize(&app, &headers)?;
    if !valid_id(&batch_id) {
        return Err(bad("invalid batch id"));
    }
    if !valid_kind(&body.kind) {
        return Err(bad("kind: 1-32 chars of a-z 0-9 _ (e.g. collected, processed, packed)"));
    }
    if !body.data.is_object() {
        return Err(bad("data must be a JSON object"));
    }

    // The record names its batch and step, so a hash can't be replayed elsewhere.
    let record = json!({
        "schema": "gaia-trace/1",
        "contract": app.contract_id,
        "batch_id": batch_id,
        "kind": body.kind,
        "occurred_at": body.occurred_at.unwrap_or_else(now_iso),
        "data": body.data,
    });
    let bytes = serde_jcs::to_vec(&record).map_err(|e| bad(e.to_string()))?;
    let hash: [u8; 32] = Sha256::digest(&bytes).into();
    let hash_hex = hex::encode(hash);

    let uri = app.storage.put(&hash_hex, &bytes).await?;
    let res = app
        .chain
        .write(
            "anchor",
            vec![
                app.chain.operator(),
                sv_string(&batch_id)?,
                sv_symbol(&body.kind)?,
                sv_bytes32(hash)?,
                sv_string(&uri)?,
            ],
        )
        .await?;

    Ok(Json(json!({
        "batch_id": batch_id,
        "kind": body.kind,
        "index": to_json(&res.value),
        "hash": hash_hex,
        "uri": uri,
        "tx_hash": res.tx_hash,
        "ledger": res.ledger,
    })))
}

async fn get_batch(
    State(app): State<Shared>,
    Path(batch_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    if !valid_id(&batch_id) {
        return Err(bad("invalid batch id"));
    }
    let batch = app.chain.read("batch", vec![sv_string(&batch_id)?]).await?;
    if matches!(batch, stellar_xdr::ScVal::Void) {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such batch".into()));
    }
    let events = app.chain.read("events", vec![sv_string(&batch_id)?]).await?;
    Ok(Json(json!({ "batch": to_json(&batch), "events": to_json(&events) })))
}

fn now_iso() -> String {
    // Seconds precision UTC, no extra crate needed.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Howard Hinnant's days -> civil date.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}
