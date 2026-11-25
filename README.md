# Gaia SCF Project: batch traceability on Stellar

Every Gaia dairy batch gets a public "digital twin" on the Stellar blockchain. A customer scans the QR on the pack and sees where the milk was collected, how it was processed and tested at the plant, how it was packed and when it was dispatched. Their phone reads every record **directly from Stellar** and checks its SHA-256 fingerprint, so the facts don't depend on trusting Gaia's server.

```
Gaia Dashboard (Production page)                Stellar (Soroban)                      Customer's phone
  "Record on chain" ──── record_full(bytes) ───▶ batch-registry contract  ◀── events() / record() ──  /trace/<batch>
  builds 4 records:                              stores each record in full,              re-hashes every record,
  collected · processed · packed · dispatched    computes its SHA-256 on chain            shows ✓ / ✗ per step
```

## The contract: `contracts/batch-registry`

| | |
|---|---|
| Network | Stellar **testnet** |
| Contract ID | `CBIVUDP7G6G4GFARTVA2BSREQ7K3YZYSGFYYJWGT7DB4ARU7XXU2BG24` ([stellar.expert](https://stellar.expert/explorer/testnet/contract/CBIVUDP7G6G4GFARTVA2BSREQ7K3YZYSGFYYJWGT7DB4ARU7XXU2BG24)) |
| Version | v2 (full records on chain), upgraded in place from v1 |
| SDK | soroban-sdk 29 |

**Writes** (operator keys only, except where noted)
- `create_batch(operator, batch_id, product)`: creates the batch twin, held by the operator
- `record_full(operator, batch_id, kind, data) -> index` **(v2)**: stores the record's exact bytes on chain (up to 16 KB) and anchors their SHA-256, computed by the contract
- `anchor(operator, batch_id, kind, hash, uri) -> index` (v1): anchors only a hash; the record lives at `uri`
- `move_twin(batch_id, to)`: the holder hands the twin on
- `keep_alive(batch_id)`: anyone can extend storage (twin, anchors and stored records)
- `set_operator(addr, bool)`, `set_admin(addr)`, `upgrade(wasm_hash)`: admin only

**Reads**: `batch(id)`, `events(id)`, `record(id, index)` (v2), `verify(id, hash)`, `admin()`, `is_operator(addr)`

Limits: 64 anchors per batch. Ids up to 64 chars. Storage is extended to the network maximum on every write.

## What a batch record holds

Each step is a canonical JSON record (`schema: gaia-trace/2`: keys sorted, no spaces):

| Step | Public facts |
|---|---|
| `collected` | village centres that filled the tanks the batch drew from: litres, fat, SNF, pours, farmer count (names are never published) |
| `processed` | plant (Gaia Plant, Tum Gaon, with GPS), milk used and its blend, lab test (fat, SNF, CLR), ingredients, yield |
| `packed` | SKU and pack, packs made, use-by from shelf life, storage, typical nutrition |
| `dispatched` | trucks that carried the SKU: route, vehicle, departed/arrived, stops delivered |

Never published: costs and rates, farmer and customer names, tank names, recipe quantities.

## Repository

```
contracts/batch-registry/   the Soroban contract and its tests
demo-qr/                    QR codes for the 50 demo product pages + printable PDF + links CSV
services/anchor/            first prototype: a standalone Rust (axum) anchoring service (v1)
verify/                     first prototype: a standalone Vite + React verify page (v1)
```

The live integration (recording from the Production page and the public `/trace/<batch>` page) lives in the Gaia Dashboard, a separate private repository.

## Build and test

```bash
cargo test                      # contract unit tests
stellar contract build          # -> target/wasm32v1-none/release/batch_registry.wasm
```

Upgrade the deployed contract (admin key):

```bash
stellar contract upload --wasm target/wasm32v1-none/release/batch_registry.wasm --source-account <admin> --network testnet
stellar contract invoke --id CBIVUDP7G6G4GFARTVA2BSREQ7K3YZYSGFYYJWGT7DB4ARU7XXU2BG24 --source-account <admin> --network testnet -- upgrade --wasm_hash <hash>
```

Read a batch's records straight from the chain:

```bash
stellar contract invoke --id CBIVUDP7G6G4GFARTVA2BSREQ7K3YZYSGFYYJWGT7DB4ARU7XXU2BG24 --source-account <any> --network testnet --send=no -- events --batch_id GAIA-DEMO-TM500-20261125
stellar contract invoke --id CBIVUDP7G6G4GFARTVA2BSREQ7K3YZYSGFYYJWGT7DB4ARU7XXU2BG24 --source-account <any> --network testnet --send=no -- record --batch_id GAIA-DEMO-TM500-20261125 --index 0
```

## Demo

50 demo product pages with dummy figures, one per Gaia SKU, e.g. https://production.vamadairy.in/trace/GAIA-DEMO-TM500-20261125. All QR codes are in [`demo-qr/gaia-all-qr-codes.pdf`](demo-qr/gaia-all-qr-codes.pdf).

## Going to mainnet

Deploy the same WASM with a funded mainnet admin key, authorise an operator key with `set_operator`, and point the Dashboard at the mainnet RPC, passphrase and contract ID. On mainnet, size the storage TTL to outlast each product's shelf life, and keep the admin secret in a password manager. **Never commit keys:** `.env` files are git-ignored.
