#![no_std]
//! Gaia batch registry.
//!
//! Every product batch gets a "twin": an on-chain record with a holder.
//! Each step in the batch's life (collected, processed, packed, dispatched ...)
//! is anchored as a SHA-256 hash of its JSON record.
//!
//! v1 `anchor`: only the hash goes on chain; the JSON lives at `uri`.
//! v2 `record_full`: the JSON itself is stored on chain too (the contract
//! hashes it), so the record can be read back from Stellar with `record`.

use soroban_sdk::{
    contract, contracterror, contractevent, contractimpl, contracttype, Address, Bytes, BytesN,
    ContractExecutable, Env, String, Symbol, Vec,
};

/// Roughly one year of ledgers (~5 s each), capped to the network maximum.
const TARGET_TTL: u32 = 6_312_000;
/// Re-extend when fewer than ~30 days remain.
const TTL_THRESHOLD: u32 = 518_400;
/// Keeps one batch's event list well inside the entry size limit.
pub const MAX_EVENTS_PER_BATCH: u32 = 64;
const MAX_ID_LEN: u32 = 64;
const MAX_URI_LEN: u32 = 256;
/// Largest record stored on chain (a step's JSON is ~1-3 KB).
pub const MAX_RECORD_LEN: u32 = 16_384;
/// The `uri` of an anchor whose record is stored on chain: read it with `record(batch_id, index)`.
pub const ON_CHAIN_URI: &str = "stellar:record";

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum Error {
    NotOperator = 1,
    BatchExists = 2,
    BatchNotFound = 3,
    TooManyEvents = 4,
    DuplicateHash = 5,
    InvalidInput = 6,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Batch {
    pub id: String,
    pub product: String,
    pub holder: Address,
    pub created_at: u64,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Anchor {
    /// Step name, e.g. `collected`, `processed`, `packed`.
    pub kind: Symbol,
    /// SHA-256 of the canonical (RFC 8785) JSON record.
    pub hash: BytesN<32>,
    /// Where the JSON lives (ipfs://..., https://...).
    pub uri: String,
    /// Ledger close time when anchored (unix seconds).
    pub at: u64,
    pub ledger: u32,
    pub operator: Address,
}

#[contracttype]
enum DataKey {
    Admin,
    Operator(Address),
    Batch(String),
    Events(String),
    /// v2: the full record of anchor `index` of a batch
    Record(String, u32),
}

#[contractevent(topics = ["batch_created"], data_format = "single-value")]
pub struct BatchCreated {
    #[topic]
    pub batch_id: String,
    pub product: String,
}

#[contractevent(topics = ["anchored"])]
pub struct Anchored {
    #[topic]
    pub batch_id: String,
    #[topic]
    pub kind: Symbol,
    pub index: u32,
    pub hash: BytesN<32>,
    pub uri: String,
}

#[contractevent(topics = ["twin_moved"], data_format = "single-value")]
pub struct TwinMoved {
    #[topic]
    pub batch_id: String,
    pub to: Address,
}

#[contract]
pub struct BatchRegistry;

#[contractimpl]
impl BatchRegistry {
    pub fn __constructor(env: Env, admin: Address) {
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::Operator(admin), &true);
        bump_instance(&env);
    }

    // ---------- admin ----------

    pub fn admin(env: Env) -> Address {
        env.storage().instance().get(&DataKey::Admin).unwrap()
    }

    pub fn set_admin(env: Env, new_admin: Address) {
        Self::admin(env.clone()).require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        bump_instance(&env);
    }

    /// Allow or revoke a key (plant terminal, backend service) to anchor.
    pub fn set_operator(env: Env, operator: Address, enabled: bool) {
        Self::admin(env.clone()).require_auth();
        let key = DataKey::Operator(operator);
        if enabled {
            env.storage().instance().set(&key, &true);
        } else {
            env.storage().instance().remove(&key);
        }
        bump_instance(&env);
    }

    pub fn is_operator(env: Env, operator: Address) -> bool {
        env.storage()
            .instance()
            .has(&DataKey::Operator(operator))
    }

    pub fn upgrade(env: Env, wasm_hash: BytesN<32>) {
        Self::admin(env.clone()).require_auth();
        env.deployer()
            .update_current_contract(ContractExecutable::Wasm(wasm_hash));
    }

    // ---------- writes ----------

    /// Creates the batch twin. The operator holds it until it is moved.
    pub fn create_batch(
        env: Env,
        operator: Address,
        batch_id: String,
        product: String,
    ) -> Result<(), Error> {
        require_operator(&env, &operator)?;
        check_len(&batch_id, MAX_ID_LEN)?;
        check_len(&product, MAX_ID_LEN)?;

        let key = DataKey::Batch(batch_id.clone());
        if env.storage().persistent().has(&key) {
            return Err(Error::BatchExists);
        }
        let batch = Batch {
            id: batch_id.clone(),
            product: product.clone(),
            holder: operator,
            created_at: env.ledger().timestamp(),
        };
        env.storage().persistent().set(&key, &batch);
        bump(&env, &key);
        bump_instance(&env);

        BatchCreated { batch_id, product }.publish(&env);
        Ok(())
    }

    /// v1: anchors one step's hash; the record itself lives at `uri`. Returns the index.
    pub fn anchor(
        env: Env,
        operator: Address,
        batch_id: String,
        kind: Symbol,
        hash: BytesN<32>,
        uri: String,
    ) -> Result<u32, Error> {
        require_operator(&env, &operator)?;
        check_len(&uri, MAX_URI_LEN)?;
        push_anchor(&env, operator, batch_id, kind, hash, uri)
    }

    /// v2: stores the whole record on chain and anchors its SHA-256, computed here so the
    /// stored bytes and the hash can never disagree. Returns the anchor's index.
    pub fn record_full(
        env: Env,
        operator: Address,
        batch_id: String,
        kind: Symbol,
        data: Bytes,
    ) -> Result<u32, Error> {
        require_operator(&env, &operator)?;
        if data.len() == 0 || data.len() > MAX_RECORD_LEN {
            return Err(Error::InvalidInput);
        }
        let hash: BytesN<32> = env.crypto().sha256(&data).to_bytes();
        let uri = String::from_str(&env, ON_CHAIN_URI);
        let index = push_anchor(&env, operator, batch_id.clone(), kind, hash, uri)?;
        let key = DataKey::Record(batch_id, index);
        env.storage().persistent().set(&key, &data);
        bump(&env, &key);
        Ok(index)
    }

    /// Hands the twin to someone else (distributor, retailer). Holder signs.
    pub fn move_twin(env: Env, batch_id: String, to: Address) -> Result<(), Error> {
        let key = DataKey::Batch(batch_id.clone());
        let mut batch: Batch = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(Error::BatchNotFound)?;
        batch.holder.require_auth();
        batch.holder = to.clone();
        env.storage().persistent().set(&key, &batch);
        bump(&env, &key);
        TwinMoved { batch_id, to }.publish(&env);
        Ok(())
    }

    /// Anyone may pay to keep a batch's records alive longer (its twin, anchors and stored records).
    pub fn keep_alive(env: Env, batch_id: String) -> Result<(), Error> {
        let batch_key = DataKey::Batch(batch_id.clone());
        if !env.storage().persistent().has(&batch_key) {
            return Err(Error::BatchNotFound);
        }
        bump(&env, &batch_key);
        let events_key = DataKey::Events(batch_id.clone());
        let events: Vec<Anchor> = env
            .storage()
            .persistent()
            .get(&events_key)
            .unwrap_or_else(|| Vec::new(&env));
        if !events.is_empty() {
            bump(&env, &events_key);
        }
        for i in 0..events.len() {
            let key = DataKey::Record(batch_id.clone(), i);
            if env.storage().persistent().has(&key) {
                bump(&env, &key);
            }
        }
        bump_instance(&env);
        Ok(())
    }

    // ---------- reads ----------

    pub fn batch(env: Env, batch_id: String) -> Option<Batch> {
        env.storage().persistent().get(&DataKey::Batch(batch_id))
    }

    pub fn events(env: Env, batch_id: String) -> Vec<Anchor> {
        env.storage()
            .persistent()
            .get(&DataKey::Events(batch_id))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// v2: the full record of anchor `index`, when it was stored on chain with `record_full`.
    pub fn record(env: Env, batch_id: String, index: u32) -> Option<Bytes> {
        env.storage()
            .persistent()
            .get(&DataKey::Record(batch_id, index))
    }

    /// Returns the index of the anchor with this hash, if any.
    pub fn verify(env: Env, batch_id: String, hash: BytesN<32>) -> Option<u32> {
        Self::events(env, batch_id)
            .iter()
            .position(|e| e.hash == hash)
            .map(|i| i as u32)
    }
}

/// Appends an anchor to a batch's event list and returns its index.
fn push_anchor(
    env: &Env,
    operator: Address,
    batch_id: String,
    kind: Symbol,
    hash: BytesN<32>,
    uri: String,
) -> Result<u32, Error> {
    let batch_key = DataKey::Batch(batch_id.clone());
    if !env.storage().persistent().has(&batch_key) {
        return Err(Error::BatchNotFound);
    }

    let events_key = DataKey::Events(batch_id.clone());
    let mut events: Vec<Anchor> = env
        .storage()
        .persistent()
        .get(&events_key)
        .unwrap_or_else(|| Vec::new(env));

    if events.len() >= MAX_EVENTS_PER_BATCH {
        return Err(Error::TooManyEvents);
    }
    if events.iter().any(|e| e.hash == hash) {
        return Err(Error::DuplicateHash);
    }

    let index = events.len();
    events.push_back(Anchor {
        kind: kind.clone(),
        hash: hash.clone(),
        uri: uri.clone(),
        at: env.ledger().timestamp(),
        ledger: env.ledger().sequence(),
        operator,
    });
    env.storage().persistent().set(&events_key, &events);
    bump(env, &events_key);
    bump(env, &batch_key);
    bump_instance(env);

    Anchored {
        batch_id,
        kind,
        index,
        hash,
        uri,
    }
    .publish(env);
    Ok(index)
}

fn require_operator(env: &Env, operator: &Address) -> Result<(), Error> {
    operator.require_auth();
    if env
        .storage()
        .instance()
        .has(&DataKey::Operator(operator.clone()))
    {
        Ok(())
    } else {
        Err(Error::NotOperator)
    }
}

fn check_len(s: &String, max: u32) -> Result<(), Error> {
    if s.len() == 0 || s.len() > max {
        Err(Error::InvalidInput)
    } else {
        Ok(())
    }
}

fn target_ttl(env: &Env) -> u32 {
    TARGET_TTL.min(env.storage().max_ttl())
}

fn bump(env: &Env, key: &DataKey) {
    let to = target_ttl(env);
    env.storage()
        .persistent()
        .extend_ttl(key, TTL_THRESHOLD.min(to), to);
}

fn bump_instance(env: &Env) {
    let to = target_ttl(env);
    env.storage()
        .instance()
        .extend_ttl(TTL_THRESHOLD.min(to), to);
}

mod test;
