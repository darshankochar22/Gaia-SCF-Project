//! Talks to the batch-registry Soroban contract through Stellar RPC.

use anyhow::{Context, Result, anyhow, bail};
use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use stellar_rpc_client::Client;
use stellar_xdr::{
    AccountId, BytesM, ContractId, DecoratedSignature, Hash, HostFunction, InvokeContractArgs,
    InvokeHostFunctionOp, Limits, Memo, MuxedAccount, Operation, OperationBody, Preconditions,
    PublicKey, ReadXdr, ScAddress, ScBytes, ScString, ScSymbol, ScVal, SequenceNumber, Signature,
    SignatureHint, StringM, Transaction, TransactionEnvelope, TransactionExt,
    TransactionV1Envelope, Uint256, VecM, WriteXdr,
};
use tokio::sync::Mutex;

pub struct Chain {
    rpc: Client,
    network_id: [u8; 32],
    contract: [u8; 32],
    key: SigningKey,
    /// One write at a time so sequence numbers never collide.
    write_lock: Mutex<()>,
}

/// A contract-level failure, e.g. `Error(Contract, #5)` = DuplicateHash.
#[derive(Debug)]
pub struct ContractError(pub u32);

impl std::fmt::Display for ContractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self.0 {
            1 => "NotOperator",
            2 => "BatchExists",
            3 => "BatchNotFound",
            4 => "TooManyEvents",
            5 => "DuplicateHash",
            6 => "InvalidInput",
            _ => "Unknown",
        };
        write!(f, "contract error #{} ({name})", self.0)
    }
}
impl std::error::Error for ContractError {}

pub struct Submitted {
    pub tx_hash: String,
    pub ledger: Option<u32>,
    pub value: ScVal,
}

impl Chain {
    pub fn new(rpc_url: &str, passphrase: &str, contract_id: &str, secret: &str) -> Result<Self> {
        let contract = match stellar_strkey::Strkey::from_string(contract_id)? {
            stellar_strkey::Strkey::Contract(c) => c.0,
            _ => bail!("ANCHOR_CONTRACT_ID must be a C... contract address"),
        };
        let seed = stellar_strkey::ed25519::PrivateKey::from_string(secret)
            .map_err(|_| anyhow!("STELLAR_SECRET must be an S... secret key"))?
            .0;
        Ok(Self {
            rpc: Client::new(rpc_url)?,
            network_id: Sha256::digest(passphrase.as_bytes()).into(),
            contract,
            key: SigningKey::from_bytes(&seed),
            write_lock: Mutex::new(()),
        })
    }

    pub fn public_key(&self) -> String {
        format!("{}", stellar_strkey::ed25519::PublicKey(self.key.verifying_key().to_bytes()))
    }

    pub fn operator(&self) -> ScVal {
        ScVal::Address(ScAddress::Account(AccountId(
            PublicKey::PublicKeyTypeEd25519(Uint256(self.key.verifying_key().to_bytes())),
        )))
    }

    /// Simulates a call and returns its value without submitting (reads).
    pub async fn read(&self, function: &str, args: Vec<ScVal>) -> Result<ScVal> {
        let tx = self.build(function, args, 0).await?;
        let sim = self.rpc.simulate_transaction_envelope(&tx, None).await?;
        if let Some(err) = sim.error {
            return Err(parse_error(&err));
        }
        sim.results()?
            .into_iter()
            .next()
            .map(|r| r.xdr)
            .ok_or_else(|| anyhow!("simulation returned no result"))
    }

    /// Simulates, attaches resources + auth, signs and submits a call.
    pub async fn write(&self, function: &str, args: Vec<ScVal>) -> Result<Submitted> {
        let _guard = self.write_lock.lock().await;

        let mut env = self.build(function, args, 0).await?;
        let sim = self.rpc.simulate_transaction_envelope(&env, None).await?;
        if let Some(err) = sim.error {
            return Err(parse_error(&err));
        }
        if sim.restore_preamble.is_some() {
            bail!("archived state must be restored first (call keep_alive / restore)");
        }
        let result = sim
            .results()?
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("simulation returned no result"))?;
        let data = sim.transaction_data()?;

        let TransactionEnvelope::Tx(TransactionV1Envelope { tx, .. }) = &mut env else {
            unreachable!()
        };
        // 100 stroops inclusion fee + resource fee, with 15% headroom.
        tx.fee = 100 + (sim.min_resource_fee as f64 * 1.15) as u32;
        tx.ext = TransactionExt::V1(data);
        let mut ops = tx.operations.to_vec();
        if let OperationBody::InvokeHostFunction(op) = &mut ops[0].body {
            op.auth = result.auth.try_into()?;
        }
        tx.operations = ops.try_into()?;

        let signed = self.sign(tx.clone())?;
        let res = self
            .rpc
            .send_transaction_polling(&signed)
            .await
            .context("submitting transaction")?;
        if res.status != "SUCCESS" {
            bail!("transaction {} failed: {}", res.tx_hash.unwrap_or_default(), res.status);
        }
        Ok(Submitted {
            tx_hash: hex::encode(signed.hash(self.network_id)?),
            ledger: res.ledger,
            value: result.xdr,
        })
    }

    async fn build(&self, function: &str, args: Vec<ScVal>, fee: u32) -> Result<TransactionEnvelope> {
        let account = self.rpc.get_account(&self.public_key()).await?;
        let op = Operation {
            source_account: None,
            body: OperationBody::InvokeHostFunction(InvokeHostFunctionOp {
                host_function: HostFunction::InvokeContract(InvokeContractArgs {
                    contract_address: ScAddress::Contract(ContractId(Hash(self.contract))),
                    function_name: ScSymbol(function.try_into()?),
                    args: args.try_into()?,
                }),
                auth: VecM::default(),
            }),
        };
        Ok(TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: Transaction {
                source_account: MuxedAccount::Ed25519(Uint256(self.key.verifying_key().to_bytes())),
                fee: fee.max(100),
                seq_num: SequenceNumber(account.seq_num.0 + 1),
                cond: Preconditions::None,
                memo: Memo::None,
                operations: vec![op].try_into()?,
                ext: TransactionExt::V0,
            },
            signatures: VecM::default(),
        }))
    }

    fn sign(&self, tx: Transaction) -> Result<TransactionEnvelope> {
        let unsigned = TransactionEnvelope::Tx(TransactionV1Envelope {
            tx: tx.clone(),
            signatures: VecM::default(),
        });
        let hash = unsigned.hash(self.network_id)?;
        let sig = self.key.sign(&hash);
        let pk = self.key.verifying_key().to_bytes();
        let decorated = DecoratedSignature {
            hint: SignatureHint(pk[28..].try_into()?),
            signature: Signature(sig.to_bytes().to_vec().try_into()?),
        };
        Ok(TransactionEnvelope::Tx(TransactionV1Envelope {
            tx,
            signatures: vec![decorated].try_into()?,
        }))
    }
}

fn parse_error(msg: &str) -> anyhow::Error {
    // RPC reports e.g. "HostError: Error(Contract, #5)".
    if let Some(i) = msg.find("Error(Contract, #") {
        let rest = &msg[i + 17..];
        if let Some(code) = rest.split(')').next().and_then(|c| c.parse().ok()) {
            return ContractError(code).into();
        }
    }
    anyhow!("simulation failed: {}", msg.lines().next().unwrap_or(msg))
}

// ---------- ScVal helpers ----------

pub fn sv_string(s: &str) -> Result<ScVal> {
    Ok(ScVal::String(ScString(StringM::try_from(s)?)))
}

pub fn sv_symbol(s: &str) -> Result<ScVal> {
    Ok(ScVal::Symbol(ScSymbol(StringM::try_from(s)?)))
}

pub fn sv_bytes32(b: [u8; 32]) -> Result<ScVal> {
    Ok(ScVal::Bytes(ScBytes(BytesM::try_from(b.to_vec())?)))
}

/// Turns any contract return value into plain JSON for API responses.
pub fn to_json(v: &ScVal) -> serde_json::Value {
    use serde_json::{Value, json};
    match v {
        ScVal::Void => Value::Null,
        ScVal::Bool(b) => json!(b),
        ScVal::U32(n) => json!(n),
        ScVal::I32(n) => json!(n),
        ScVal::U64(n) => json!(n),
        ScVal::I64(n) => json!(n),
        ScVal::String(s) => json!(s.to_utf8_string_lossy()),
        ScVal::Symbol(s) => json!(s.to_utf8_string_lossy()),
        ScVal::Bytes(b) => json!(hex::encode(b.as_slice())),
        ScVal::Address(a) => json!(a.to_string()),
        ScVal::Vec(Some(items)) => Value::Array(items.iter().map(to_json).collect()),
        ScVal::Vec(None) => Value::Array(vec![]),
        ScVal::Map(Some(m)) => {
            let mut out = serde_json::Map::new();
            for e in m.iter() {
                let k = match &e.key {
                    ScVal::Symbol(s) => s.to_utf8_string_lossy(),
                    other => to_json(other).to_string(),
                };
                out.insert(k, to_json(&e.val));
            }
            Value::Object(out)
        }
        other => json!(other.to_xdr_base64(Limits::none()).unwrap_or_default()),
    }
}

#[allow(dead_code)]
pub fn from_b64(s: &str) -> Result<ScVal> {
    Ok(ScVal::from_xdr_base64(s, Limits::none())?)
}
