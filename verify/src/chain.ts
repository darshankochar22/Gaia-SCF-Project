// Reads a batch straight from the Stellar contract (no Gaia server in the
// trust path), then fetches each record and checks its SHA-256.
import {
  Account,
  Contract,
  Keypair,
  TransactionBuilder,
  nativeToScVal,
  rpc,
  scValToNative,
  xdr,
} from '@stellar/stellar-sdk'

export const CONFIG = {
  rpcUrl: import.meta.env.VITE_RPC_URL ?? 'https://soroban-testnet.stellar.org',
  passphrase: import.meta.env.VITE_NETWORK_PASSPHRASE ?? 'Test SDF Network ; September 2015',
  contractId: import.meta.env.VITE_CONTRACT_ID ?? 'CBIVUDP7G6G4GFARTVA2BSREQ7K3YZYSGFYYJWGT7DB4ARU7XXU2BG24',
  explorer: import.meta.env.VITE_EXPLORER ?? 'https://stellar.expert/explorer/testnet',
  ipfsGateway: import.meta.env.VITE_IPFS_GATEWAY ?? 'https://ipfs.io/ipfs/',
}

const LEDGER_SECONDS = 5
const server = new rpc.Server(CONFIG.rpcUrl)
const contract = new Contract(CONFIG.contractId)
// Reads are simulations; any well-formed source account works.
const reader = new Account(Keypair.random().publicKey(), '0')

export type Batch = { id: string; product: string; holder: string; created_at: bigint }
export type Anchor = {
  kind: string
  hash: Uint8Array
  uri: string
  at: bigint
  ledger: number
  operator: string
}

async function read<T>(method: string, ...args: xdr.ScVal[]): Promise<T> {
  const tx = new TransactionBuilder(reader, { fee: '100', networkPassphrase: CONFIG.passphrase })
    .addOperation(contract.call(method, ...args))
    .setTimeout(30)
    .build()
  const sim = await server.simulateTransaction(tx)
  if (rpc.Api.isSimulationError(sim)) throw new Error(sim.error)
  if (!sim.result) throw new Error('no result')
  return scValToNative(sim.result.retval) as T
}

export async function loadBatch(id: string) {
  const idVal = nativeToScVal(id, { type: 'string' })
  const [batch, events] = await Promise.all([
    read<Batch | null>('batch', idVal),
    read<Anchor[]>('events', idVal),
  ])
  return { batch, events }
}

/** When the batch's on-chain records expire unless renewed (approximate). */
export async function storedUntil(id: string): Promise<Date | null> {
  const key = xdr.ScVal.scvVec([
    xdr.ScVal.scvSymbol('Events'),
    nativeToScVal(id, { type: 'string' }),
  ])
  try {
    const [entry, latest] = await Promise.all([
      server.getContractData(CONFIG.contractId, key, rpc.Durability.Persistent),
      server.getLatestLedger(),
    ])
    if (!entry.liveUntilLedgerSeq) return null
    const secs = (entry.liveUntilLedgerSeq - latest.sequence) * LEDGER_SECONDS
    return new Date(Date.now() + secs * 1000)
  } catch {
    return null
  }
}

export function httpUrl(uri: string) {
  return uri.startsWith('ipfs://') ? CONFIG.ipfsGateway + uri.slice(7) : uri
}

export const hex = (b: Uint8Array) => Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('')

export type Check =
  | { state: 'checking' }
  | { state: 'match'; record: Record<string, unknown> }
  | { state: 'mismatch'; reason: string; record?: Record<string, unknown> }
  | { state: 'unreachable'; reason: string }

/** Fetches the record and compares its exact bytes with the anchored hash. */
export async function checkAnchor(batchId: string, a: Anchor): Promise<Check> {
  let bytes: ArrayBuffer
  try {
    const res = await fetch(httpUrl(a.uri))
    if (!res.ok) return { state: 'unreachable', reason: `HTTP ${res.status}` }
    bytes = await res.arrayBuffer()
  } catch (e) {
    return { state: 'unreachable', reason: (e as Error).message }
  }
  const digest = new Uint8Array(await crypto.subtle.digest('SHA-256', bytes))
  let record: Record<string, unknown> | undefined
  try {
    record = JSON.parse(new TextDecoder().decode(bytes))
  } catch {
    /* not JSON */
  }
  if (hex(digest) !== hex(a.hash)) {
    return { state: 'mismatch', reason: 'The file was changed after it was anchored.', record }
  }
  if (!record) return { state: 'mismatch', reason: 'Record is not valid JSON.' }
  if (record.batch_id !== batchId || record.kind !== a.kind || record.contract !== CONFIG.contractId) {
    return { state: 'mismatch', reason: 'This record belongs to a different batch or step.', record }
  }
  return { state: 'match', record }
}
