import { useEffect, useState } from 'react'
import {
  CONFIG,
  checkAnchor,
  hex,
  httpUrl,
  loadBatch,
  storedUntil,
  type Anchor,
  type Batch,
  type Check,
} from './chain'

const STEP_LABELS: Record<string, string> = {
  collected: 'Collected',
  chilled: 'Chilled',
  lab_tested: 'Lab tested',
  processed: 'Processed',
  packed: 'Packed',
  dispatched: 'Dispatched',
  delivered: 'Delivered',
}

const UNITS: [RegExp, string][] = [
  [/_pct$/, ' %'],
  [/_c$/, ' °C'],
  [/_kg$/, ' kg'],
  [/^litres$|_litres$/, ' L'],
  [/_min$/, ' min'],
  [/_s$/, ' s'],
]

function label(key: string) {
  const base = key
    .replace(/_(pct|c|kg|min|s)$/, '')
    .replace(/_/g, ' ')
  return base.charAt(0).toUpperCase() + base.slice(1)
}

function unit(key: string) {
  return UNITS.find(([re]) => re.test(key))?.[1] ?? ''
}

const dateFmt = new Intl.DateTimeFormat('en-IN', { day: 'numeric', month: 'short', year: 'numeric' })
const timeFmt = new Intl.DateTimeFormat('en-IN', {
  day: 'numeric',
  month: 'short',
  year: 'numeric',
  hour: '2-digit',
  minute: '2-digit',
  timeZoneName: 'short',
})

function Value({ k, v }: { k: string; v: unknown }) {
  if (k === 'gps' && Array.isArray(v) && v.length === 2) {
    const [lat, lon] = v as number[]
    return (
      <a href={`https://www.openstreetmap.org/?mlat=${lat}&mlon=${lon}#map=14/${lat}/${lon}`} target="_blank" rel="noreferrer">
        {lat.toFixed(4)}° N, {lon.toFixed(4)}° E ↗
      </a>
    )
  }
  if (v === null || v === undefined || v === '') return <span className="muted">Not stated</span>
  if (typeof v === 'boolean') return <>{v ? 'Yes' : 'No'}</>
  if (typeof v === 'object') return <code>{JSON.stringify(v)}</code>
  return (
    <>
      {String(v)}
      {unit(k)}
    </>
  )
}

function Step({ batchId, anchor }: { batchId: string; anchor: Anchor }) {
  const [check, setCheck] = useState<Check>({ state: 'checking' })
  useEffect(() => {
    checkAnchor(batchId, anchor).then(setCheck)
  }, [batchId, anchor])

  const record = 'record' in check ? check.record : undefined
  const data = (record?.data ?? {}) as Record<string, unknown>
  const occurred = typeof record?.occurred_at === 'string' ? new Date(record.occurred_at) : null
  const anchoredAt = new Date(Number(anchor.at) * 1000)

  return (
    <li className="step">
      <div className="step-date">{dateFmt.format(occurred ?? anchoredAt)}</div>
      <article className="card">
        <h2>{STEP_LABELS[anchor.kind] ?? label(anchor.kind)}</h2>
        {Object.keys(data).length > 0 && (
          <dl>
            {Object.entries(data).map(([k, v]) => (
              <div className="row" key={k}>
                <dt>{label(k)}</dt>
                <dd>
                  <Value k={k} v={v} />
                </dd>
              </div>
            ))}
          </dl>
        )}

        <p className={`status ${check.state}`}>
          {check.state === 'checking' && 'Checking against Stellar…'}
          {check.state === 'match' && `✓ Matches its Stellar anchor of ${timeFmt.format(anchoredAt)}`}
          {check.state === 'mismatch' && `✗ Does not match: ${check.reason}`}
          {check.state === 'unreachable' && `Record could not be fetched (${check.reason}). The anchor is still on chain.`}
        </p>

        <div className="links">
          <a href={`${CONFIG.explorer}/ledger/${anchor.ledger}`} target="_blank" rel="noreferrer">
            Stellar anchor ↗
          </a>
          <a href={httpUrl(anchor.uri)} target="_blank" rel="noreferrer">
            Record file ↗
          </a>
        </div>
        <details>
          <summary>Raw record</summary>
          <p className="mono small">sha256 {hex(anchor.hash)}</p>
          {record && <pre>{JSON.stringify(record, null, 2)}</pre>}
        </details>
      </article>
    </li>
  )
}

function batchIdFromUrl() {
  const q = new URLSearchParams(location.search).get('b')
  if (q) return q
  const m = location.pathname.match(/\/b\/([^/]+)/)
  return m ? decodeURIComponent(m[1]) : ''
}

export default function App() {
  const [batchId] = useState(batchIdFromUrl)
  const [state, setState] = useState<
    | { s: 'idle' }
    | { s: 'loading' }
    | { s: 'error'; msg: string }
    | { s: 'ok'; batch: Batch | null; events: Anchor[]; until: Date | null }
  >(batchId ? { s: 'loading' } : { s: 'idle' })

  useEffect(() => {
    if (!batchId) return
    Promise.all([loadBatch(batchId), storedUntil(batchId)])
      .then(([{ batch, events }, until]) => setState({ s: 'ok', batch, events, until }))
      .catch((e) => setState({ s: 'error', msg: (e as Error).message }))
  }, [batchId])

  if (state.s === 'idle') {
    return (
      <main className="page">
        <h1>Gaia batch check</h1>
        <form className="lookup">
          <label htmlFor="b">Batch number from the pack</label>
          <input id="b" name="b" placeholder="GAIA-TM500-20261125-A" autoComplete="off" />
          <button>Check</button>
        </form>
      </main>
    )
  }
  if (state.s === 'loading') return <main className="page"><p className="lead">Reading batch {batchId} from Stellar…</p></main>
  if (state.s === 'error')
    return (
      <main className="page">
        <p className="lead">Could not read the blockchain right now.</p>
        <p className="muted small">{state.msg}</p>
      </main>
    )
  if (!state.batch)
    return (
      <main className="page">
        <p className="lead">No batch called “{batchId}” is recorded on chain.</p>
        <p className="muted">Check the number on the pack and try again.</p>
      </main>
    )

  const { batch, events, until } = state
  return (
    <main className="page">
      <header className="hero">
        <p className="eyebrow">Gaia · batch {batch.id}</p>
        <h1>{batch.product}</h1>
        <p className="lead">
          Your device just checked this batch's {events.length} recorded step{events.length === 1 ? '' : 's'} against the
          Stellar blockchain.
        </p>
        {until && <p className="eyebrow">Stored until {dateFmt.format(until)}</p>}
      </header>

      {events.length === 0 ? (
        <p className="muted">No steps recorded yet.</p>
      ) : (
        <ol className="timeline">
          {events.map((a, i) => (
            <Step key={i} batchId={batch.id} anchor={a} />
          ))}
        </ol>
      )}

      <footer className="twin">
        <span>
          Twin held by <span className="mono">{batch.holder.slice(0, 6)}…{batch.holder.slice(-4)}</span>
        </span>
        <a href={`${CONFIG.explorer}/contract/${CONFIG.contractId}`} target="_blank" rel="noreferrer">
          Contract ↗
        </a>
      </footer>
    </main>
  )
}
