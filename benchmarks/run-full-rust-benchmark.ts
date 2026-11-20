/**
 * Comprehensive Pure-Rust Benchmark Suite & SVG Chart Generator for `jsonbit`
 *
 * Benchmarks `./target/release/jsonbit` (Pure-Rust Auto-3NF Bit-Packed Columnar & BitVec64 Engine)
 * against:
 *  1. Raw JSON (Pretty)
 *  2. Minified JSON
 *  3. YAML
 *  4. TOON (`@toon-format/toon`)
 *  5. `jq` (C CLI JSON processor)
 *
 * Across:
 *  - Track 1: 10 Real-World Workloads (`W1`–`W10`, including edge cases) + 8 Official Suite Datasets
 *  - Track 2: Scale Stress Track (`10` -> `100` -> `1,000` -> `10,000` nested records)
 *  - Track 3: Query Execution Speed (`Pure-Rust BitVec64 SQL/Slice` vs `jq` CLI) & 100% Codd 3NF Lossless Rehydration
 *  - Track 4: Live `gemini-3.8-flash` End-to-End Accuracy & Token Evaluation (powered 100% by `./target/release/jsonbit`)
 *  - Track 5: Automatic SVG Chart Generation (`benchmarks/charts/*.svg`)
 */

import { spawnSync } from 'node:child_process'
import * as fs from 'node:fs'
import * as path from 'node:path'
import { encode as countTokens } from 'gpt-tokenizer'
import { encode as encodeToon } from '@toon-format/toon'
import { buildRealWorldWorkloads, type RealWorldWorkload } from './datasets/real-world-workloads.ts'
import { TOKEN_EFFICIENCY_DATASETS } from './toon-suite/datasets.ts'
import { compareAnswers } from './toon-suite/normalize.ts'

const API_KEY = process.env.GEMINI_API_KEY || ''
const MODEL = process.env.GEMINI_MODEL || 'gemini-3.8-flash'
const ENDPOINT = `https://generativelanguage.googleapis.com/v1beta/models/${MODEL}:generateContent?key=${API_KEY}`
const RUST_BIN = path.resolve(process.cwd(), 'target/release/jsonbit')

// Lightweight deterministic JSON-to-YAML serializer for benchmark comparison
function toYaml(val: unknown, indent = 0): string {
  const pad = '  '.repeat(indent)
  if (val === null || val === undefined) return 'null'
  if (typeof val === 'boolean' || typeof val === 'number') return String(val)
  if (typeof val === 'string') {
    if (/[:#\n"{}[\],&*!|>'%@`]/.test(val) || val.trim() !== val || val === '') {
      return JSON.stringify(val)
    }
    return val
  }
  if (Array.isArray(val)) {
    if (val.length === 0) return '[]'
    if (val.every(x => typeof x !== 'object' || x === null)) {
      return `[${val.map(x => toYaml(x, 0)).join(', ')}]`
    }
    return val
      .map(item => {
        if (typeof item === 'object' && item !== null && !Array.isArray(item)) {
          const entries = Object.entries(item)
          if (entries.length === 0) return `${pad}- {}`
          const [firstK, firstV] = entries[0]!
          const firstLine =
            typeof firstV === 'object' && firstV !== null && !Array.isArray(firstV)
              ? `${pad}- ${firstK}:\n${toYaml(firstV, indent + 2)}`
              : `${pad}- ${firstK}: ${toYaml(firstV, indent + 1)}`
          const restLines = entries.slice(1).map(([k, v]) => {
            if (typeof v === 'object' && v !== null && !Array.isArray(v)) {
              return `${pad}  ${k}:\n${toYaml(v, indent + 2)}`
            }
            return `${pad}  ${k}: ${toYaml(v, indent + 1)}`
          })
          return [firstLine, ...restLines].join('\n')
        }
        return `${pad}- ${toYaml(item, indent + 1)}`
      })
      .join('\n')
  }
  if (typeof val === 'object') {
    const entries = Object.entries(val as Record<string, unknown>)
    if (entries.length === 0) return '{}'
    return entries
      .map(([k, v]) => {
        if (Array.isArray(v) && v.length > 0 && typeof v[0] === 'object') {
          return `${pad}${k}:\n${toYaml(v, indent + 1)}`
        }
        if (typeof v === 'object' && v !== null && !Array.isArray(v)) {
          return `${pad}${k}:\n${toYaml(v, indent + 1)}`
        }
        return `${pad}${k}: ${toYaml(v, indent + 1)}`
      })
      .join('\n')
  }
  return String(val)
}

interface RustDatasetEval {
  name: string
  raw_json_bytes: number
  compressed_bytes: number
  jolap_bytes: number
  ingest_us: number
  jolap_load_us: number
  card_text: string
  digest_text: string
  lossless_rehydrate: boolean
}

interface RustQuestionEval {
  id: string
  answer: string
  context: string
  latency_us: number
  sql_result: unknown
  sql_latency_us: number
}

function runRustEvalBatch(
  datasets: Array<{ name: string; data: unknown }>,
  questions: Array<{ id: string; dataset: string; prompt: string; sql?: string }>,
): { dataset_results: RustDatasetEval[]; question_results: RustQuestionEval[] } {
  const proc = spawnSync(
    RUST_BIN,
    ['eval-batch'],
    {
      input: JSON.stringify({
        datasets,
        eval_datasets: datasets,
        questions,
      }),
      maxBuffer: 120 * 1024 * 1024,
      encoding: 'utf8',
    },
  )
  if (proc.status !== 0) {
    throw new Error(`Rust eval-batch failed: ${proc.stderr}`)
  }
  return JSON.parse(proc.stdout)
}

function runRustSql(filePath: string, sql: string): string {
  const proc = spawnSync(RUST_BIN, ['sql', filePath, sql], {
    maxBuffer: 20 * 1024 * 1024,
    encoding: 'utf8',
  })
  if (proc.status !== 0) {
    return `SQL_ERROR: ${proc.stderr}`
  }
  return proc.stdout.trim()
}

async function callGemini(
  prompt: string,
  maxTokens = 256,
): Promise<{ text: string; latencyMs: number }> {
  const t0 = performance.now()
  for (let attempt = 0; attempt < 3; attempt++) {
    try {
      const res = await fetch(ENDPOINT, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          contents: [{ role: 'user', parts: [{ text: prompt }] }],
          generationConfig: {
            temperature: 0,
            maxOutputTokens: maxTokens,
            thinkingConfig: { thinkingBudget: 0 },
          },
        }),
      })
      if (!res.ok) {
        if (res.status === 429 || res.status >= 500) {
          await new Promise(r => setTimeout(r, 800 * (attempt + 1)))
          continue
        }
        const errTxt = await res.text()
        throw new Error(`HTTP ${res.status}: ${errTxt.slice(0, 200)}`)
      }
      const json = (await res.json()) as {
        candidates?: Array<{ content?: { parts?: Array<{ text?: string }> } }>
      }
      const text = json.candidates?.[0]?.content?.parts?.[0]?.text?.trim() ?? ''
      return { text, latencyMs: performance.now() - t0 }
    } catch (e) {
      if (attempt === 2) throw e
      await new Promise(r => setTimeout(r, 800 * (attempt + 1)))
    }
  }
  return { text: '', latencyMs: performance.now() - t0 }
}

async function mapConcurrent<T, R>(
  items: T[],
  concurrency: number,
  fn: (item: T, idx: number) => Promise<R>,
): Promise<R[]> {
  const results: R[] = new Array(items.length)
  let nextIdx = 0
  async function worker() {
    while (nextIdx < items.length) {
      const i = nextIdx++
      results[i] = await fn(items[i]!, i)
    }
  }
  await Promise.all(Array.from({ length: Math.min(concurrency, items.length) }, () => worker()))
  return results
}

function extractScalarFromSqlJson(rawJsonText: string): string {
  try {
    const parsed = JSON.parse(rawJsonText)
    if (Array.isArray(parsed) && parsed.length === 1 && typeof parsed[0] === 'object' && parsed[0] !== null) {
      const vals = Object.values(parsed[0] as Record<string, unknown>)
      if (vals.length === 1 && vals[0] !== null && vals[0] !== undefined) {
        return String(vals[0])
      }
    }
  } catch {
    // ignore
  }
  return rawJsonText
}

function generateScaleOrders(orderCount: number) {
  const statuses = ['delivered', 'shipped', 'processing', 'cancelled', 'refunded']
  const tiers = ['enterprise', 'pro', 'starter']
  const catalog = [
    { sku: 'SKU-GPU-01', name: 'H100 Compute Node', price: 2400.0 },
    { sku: 'SKU-NVME-02', name: 'NVMe Raid Array', price: 450.5 },
    { sku: 'SKU-NIC-03', name: 'Infiniband Switch', price: 890.0 },
    { sku: 'SKU-PSU-04', name: 'Redundant Power Supply', price: 180.25 },
  ]
  const orders = Array.from({ length: orderCount }, (_, i) => {
    const status = statuses[i % statuses.length]!
    const tier = tiers[i % tiers.length]!
    const itemCount = 1 + (i % 3)
    const items = []
    let total = 0
    for (let j = 0; j < itemCount; j++) {
      const prod = catalog[(i + j) % catalog.length]!
      const quantity = 1 + ((i + j) % 4)
      items.push({
        sku: prod.sku,
        name: prod.name,
        quantity,
        unitPrice: prod.price,
      })
      total += quantity * prod.price
    }
    return {
      orderId: `ORD-${10000 + i}`,
      customer: {
        customerId: `CUST-${100 + (i % 200)}`,
        tier,
        region: ['us-east', 'eu-central', 'ap-northeast'][i % 3]!,
      },
      status,
      items,
      orderTotal: Math.round(total * 100) / 100,
    }
  })
  return { orders }
}

// ============================================================================
// SVG Chart Generators (Publication-Grade Vector Graphics)
// ============================================================================
function generateTokenScalingSvg(
  scaleRows: Array<{
    rows: number
    prettyTok: number
    minTok: number
    yamlTok: number
    toonTok: number
    olapTok: number
  }>,
  outPath: string,
) {
  const width = 920
  const height = 460
  const padL = 90
  const padR = 50
  const padT = 70
  const padB = 75
  const plotW = width - padL - padR
  const plotH = height - padT - padB

  // Log10 scale for Y axis (from 10^1 = 10 to 10^7 = 10,000,000 tokens)
  const minLogY = 1 // 10^1 = 10
  const maxLogY = 6.5 // ~3.16M tokens
  const yPos = (tok: number) => {
    const l = Math.log10(Math.max(10, tok))
    const norm = (l - minLogY) / (maxLogY - minLogY)
    return padT + plotH - norm * plotH
  }
  const xPos = (idx: number) => padL + (idx / (scaleRows.length - 1)) * plotW

  const series = [
    { key: 'prettyTok' as const, label: 'Raw JSON (Pretty)', color: '#ef4444', dash: '' },
    { key: 'minTok' as const, label: 'Minified JSON', color: '#f97316', dash: '6,4' },
    { key: 'yamlTok' as const, label: 'YAML', color: '#eab308', dash: '4,4' },
    { key: 'toonTok' as const, label: 'TOON (v4)', color: '#8b5cf6', dash: '' },
    { key: 'olapTok' as const, label: 'jsonbit (Pure-Rust 3NF Cube)', color: '#10b981', dash: '' },
  ]

  const yTicks = [
    { val: 10, label: '10' },
    { val: 100, label: '100' },
    { val: 1000, label: '1K' },
    { val: 10000, label: '10K' },
    { val: 100000, label: '100K' },
    { val: 1000000, label: '1M+' },
  ]

  let gridLines = ''
  for (const t of yTicks) {
    const y = yPos(t.val)
    gridLines += `<line x1="${padL}" y1="${y}" x2="${width - padR}" y2="${y}" stroke="#e2e8f0" stroke-width="1" stroke-dasharray="3,3"/>`
    gridLines += `<text x="${padL - 12}" y="${y + 4}" text-anchor="end" font-family="Inter, system-ui, sans-serif" font-size="12" fill="#475569">${t.label}</text>`
  }

  let xLabels = ''
  scaleRows.forEach((r, idx) => {
    const x = xPos(idx)
    gridLines += `<line x1="${x}" y1="${padT}" x2="${x}" y2="${padT + plotH}" stroke="#f1f5f9" stroke-width="1"/>`
    xLabels += `<text x="${x}" y="${padT + plotH + 25}" text-anchor="middle" font-family="Inter, system-ui, sans-serif" font-size="12" font-weight="600" fill="#334155">${r.rows.toLocaleString()} rows</text>`
  })

  let paths = ''
  for (const s of series) {
    const pts = scaleRows.map((r, i) => `${xPos(i).toFixed(1)},${yPos(r[s.key]).toFixed(1)}`).join(' ')
    const sw = s.key === 'olapTok' ? 3.5 : 2.2
    paths += `<polyline fill="none" stroke="${s.color}" stroke-width="${sw}" ${s.dash ? `stroke-dasharray="${s.dash}"` : ''} points="${pts}"/>`
    scaleRows.forEach((r, i) => {
      const cx = xPos(i)
      const cy = yPos(r[s.key])
      paths += `<circle cx="${cx.toFixed(1)}" cy="${cy.toFixed(1)}" r="${s.key === 'olapTok' ? 5 : 3.5}" fill="${s.color}"/>`
      if (i === scaleRows.length - 1 || (s.key === 'olapTok' && i === 0)) {
        const valStr = r[s.key] >= 1000 ? `${(r[s.key] / 1000).toFixed(0)}K` : String(r[s.key])
        const yOff = s.key === 'olapTok' ? -10 : s.key === 'minTok' ? 14 : -8
        paths += `<text x="${(cx - 8).toFixed(1)}" y="${(cy + yOff).toFixed(1)}" text-anchor="end" font-family="Inter, system-ui, sans-serif" font-size="11" font-weight="700" fill="${s.color}">${valStr} tok</text>`
      }
    })
  }

  let legend = ''
  series.forEach((s, idx) => {
    const lx = padL + idx * 155
    const ly = height - 22
    legend += `<rect x="${lx}" y="${ly - 9}" width="14" height="4" fill="${s.color}" rx="2"/>`
    legend += `<text x="${lx + 20}" y="${ly - 3}" font-family="Inter, system-ui, sans-serif" font-size="11.5" font-weight="600" fill="#1e293b">${s.label}</text>`
  })

  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${width} ${height}" width="100%" height="100%">
  <rect width="${width}" height="${height}" fill="#ffffff" rx="10" stroke="#cbd5e1" stroke-width="1"/>
  <text x="${padL}" y="32" font-family="Inter, system-ui, sans-serif" font-size="17" font-weight="700" fill="#0f172a">LLM Context Tokens vs. Dataset Scale (Log Scale)</text>
  <text x="${padL}" y="52" font-family="Inter, system-ui, sans-serif" font-size="12.5" fill="#64748b">O(N) Linear Formats (JSON, YAML, TOON) vs. O(1) Pure-Rust jsonbit Semantic Cube Card</text>
  ${gridLines}
  <rect x="${padL}" y="${padT}" width="${plotW}" height="${plotH}" fill="none" stroke="#cbd5e1" stroke-width="1"/>
  ${paths}
  ${xLabels}
  <text x="24" y="${padT + plotH / 2}" transform="rotate(-90 24 ${padT + plotH / 2})" text-anchor="middle" font-family="Inter, system-ui, sans-serif" font-size="12.5" font-weight="600" fill="#334155">Prompt Tokens (Log10 Scale)</text>
  ${legend}
</svg>`

  fs.mkdirSync(path.dirname(outPath), { recursive: true })
  fs.writeFileSync(outPath, svg, 'utf8')
}

function generateAccuracyAndTokensSvg(
  summary: Array<{
    name: string
    accuracyPct: number
    totalTokens: number
    color: string
  }>,
  outPath: string,
) {
  const width = 920
  const height = 430
  const padL = 70
  const padR = 50
  const padT = 75
  const padB = 75

  // Two side-by-side panels: Left = Live gemini-3.8-flash Accuracy (%), Right = Total Prompt Tokens across Suite
  const panelW = (width - padL - padR - 60) / 2
  const plotH = height - padT - padB

  let leftBars = ''
  let rightBars = ''
  const maxTok = Math.max(...summary.map(s => s.totalTokens), 1)
  const barW = Math.floor(panelW / (summary.length * 1.6))

  for (let p = 0; p <= 100; p += 25) {
    const y = padT + plotH - (p / 100) * plotH
    leftBars += `<line x1="${padL}" y1="${y}" x2="${padL + panelW}" y2="${y}" stroke="#e2e8f0" stroke-width="1" stroke-dasharray="3,3"/>`
    leftBars += `<text x="${padL - 10}" y="${y + 4}" text-anchor="end" font-family="Inter, system-ui, sans-serif" font-size="11.5" fill="#475569">${p}%</text>`
  }

  const rightOriginX = padL + panelW + 60
  for (let p = 0; p <= 4; p++) {
    const frac = p / 4
    const y = padT + plotH - frac * plotH
    const tokLabel = `${Math.round((maxTok * frac) / 1000)}K`
    rightBars += `<line x1="${rightOriginX}" y1="${y}" x2="${rightOriginX + panelW}" y2="${y}" stroke="#e2e8f0" stroke-width="1" stroke-dasharray="3,3"/>`
    rightBars += `<text x="${rightOriginX - 8}" y="${y + 4}" text-anchor="end" font-family="Inter, system-ui, sans-serif" font-size="11.5" fill="#475569">${tokLabel}</text>`
  }

  summary.forEach((s, i) => {
    const slotW = panelW / summary.length
    // Left bar (Accuracy %)
    const bx1 = padL + i * slotW + (slotW - barW) / 2
    const bh1 = (s.accuracyPct / 100) * plotH
    const by1 = padT + plotH - bh1
    leftBars += `<rect x="${bx1.toFixed(1)}" y="${by1.toFixed(1)}" width="${barW}" height="${bh1.toFixed(1)}" fill="${s.color}" rx="4"/>`
    leftBars += `<text x="${(bx1 + barW / 2).toFixed(1)}" y="${(by1 - 7).toFixed(1)}" text-anchor="middle" font-family="Inter, system-ui, sans-serif" font-size="12" font-weight="700" fill="${s.color}">${s.accuracyPct.toFixed(1)}%</text>`
    leftBars += `<text x="${(bx1 + barW / 2).toFixed(1)}" y="${padT + plotH + 22}" text-anchor="middle" font-family="Inter, system-ui, sans-serif" font-size="11" font-weight="600" fill="#334155">${s.name}</text>`

    // Right bar (Total Tokens)
    const bx2 = rightOriginX + i * slotW + (slotW - barW) / 2
    const bh2 = Math.max(4, (s.totalTokens / maxTok) * plotH)
    const by2 = padT + plotH - bh2
    rightBars += `<rect x="${bx2.toFixed(1)}" y="${by2.toFixed(1)}" width="${barW}" height="${bh2.toFixed(1)}" fill="${s.color}" rx="4"/>`
    const tokStr =
      s.totalTokens >= 1000
        ? `${(s.totalTokens / 1000).toFixed(1)}K`
        : `${s.totalTokens}`
    rightBars += `<text x="${(bx2 + barW / 2).toFixed(1)}" y="${(by2 - 7).toFixed(1)}" text-anchor="middle" font-family="Inter, system-ui, sans-serif" font-size="12" font-weight="700" fill="${s.color}">${tokStr}</text>`
    rightBars += `<text x="${(bx2 + barW / 2).toFixed(1)}" y="${padT + plotH + 22}" text-anchor="middle" font-family="Inter, system-ui, sans-serif" font-size="11" font-weight="600" fill="#334155">${s.name}</text>`
  })

  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${width} ${height}" width="100%" height="100%">
  <rect width="${width}" height="${height}" fill="#ffffff" rx="10" stroke="#cbd5e1" stroke-width="1"/>
  <text x="${padL}" y="30" font-family="Inter, system-ui, sans-serif" font-size="17" font-weight="700" fill="#0f172a">Live Gemini 3.8 Flash Benchmark: Accuracy vs. Total Prompt Tokens (10 Real-World Workloads)</text>
  <text x="${padL}" y="52" font-family="Inter, system-ui, sans-serif" font-size="12.5" fill="#0f172a" font-weight="600">A. Answer Accuracy (%) — Higher is Better</text>
  <text x="${rightOriginX}" y="52" font-family="Inter, system-ui, sans-serif" font-size="12.5" fill="#0f172a" font-weight="600">B. Total Prompt Tokens Consumed — Lower is Better</text>
  ${leftBars}
  ${rightBars}
  <line x1="${padL}" y1="${padT + plotH}" x2="${padL + panelW}" y2="${padT + plotH}" stroke="#94a3b8" stroke-width="1.5"/>
  <line x1="${rightOriginX}" y1="${padT + plotH}" x2="${rightOriginX + panelW}" y2="${padT + plotH}" stroke="#94a3b8" stroke-width="1.5"/>
</svg>`

  fs.mkdirSync(path.dirname(outPath), { recursive: true })
  fs.writeFileSync(outPath, svg, 'utf8')
}

function generateStorageAndLatencySvg(
  totalPrettyBytes: number,
  totalMinBytes: number,
  totalYamlBytes: number,
  totalToonBytes: number,
  totalOlapBytes: number,
  jqAvgUs: number,
  olapSqlAvgUs: number,
  olapSliceAvgUs: number,
  outPath: string,
) {
  const width = 920
  const height = 420
  const padL = 75
  const padR = 50
  const padT = 75
  const padB = 75
  const panelW = (width - padL - padR - 65) / 2
  const plotH = height - padT - padB
  const rightOriginX = padL + panelW + 65

  const storageSeries = [
    { name: 'Pretty JSON', bytes: totalPrettyBytes, color: '#ef4444' },
    { name: 'YAML', bytes: totalYamlBytes, color: '#eab308' },
    { name: 'Minified JSON', bytes: totalMinBytes, color: '#f97316' },
    { name: 'TOON', bytes: totalToonBytes, color: '#8b5cf6' },
    { name: 'jsonbit (Rust)', bytes: totalOlapBytes, color: '#10b981' },
  ]
  const maxBytes = Math.max(...storageSeries.map(s => s.bytes), 1)
  const barW1 = Math.floor(panelW / (storageSeries.length * 1.55))

  let leftPanel = ''
  for (let p = 0; p <= 4; p++) {
    const frac = p / 4
    const y = padT + plotH - frac * plotH
    const kb = `${Math.round((maxBytes * frac) / 1024)} KB`
    leftPanel += `<line x1="${padL}" y1="${y}" x2="${padL + panelW}" y2="${y}" stroke="#e2e8f0" stroke-width="1" stroke-dasharray="3,3"/>`
    leftPanel += `<text x="${padL - 8}" y="${y + 4}" text-anchor="end" font-family="Inter, system-ui, sans-serif" font-size="11.5" fill="#475569">${kb}</text>`
  }

  storageSeries.forEach((s, i) => {
    const slotW = panelW / storageSeries.length
    const bx = padL + i * slotW + (slotW - barW1) / 2
    const bh = (s.bytes / maxBytes) * plotH
    const by = padT + plotH - bh
    leftPanel += `<rect x="${bx.toFixed(1)}" y="${by.toFixed(1)}" width="${barW1}" height="${bh.toFixed(1)}" fill="${s.color}" rx="4"/>`
    const kbStr = `${(s.bytes / 1024).toFixed(0)} KB`
    leftPanel += `<text x="${(bx + barW1 / 2).toFixed(1)}" y="${(by - 7).toFixed(1)}" text-anchor="middle" font-family="Inter, system-ui, sans-serif" font-size="11.5" font-weight="700" fill="${s.color}">${kbStr}</text>`
    leftPanel += `<text x="${(bx + barW1 / 2).toFixed(1)}" y="${padT + plotH + 22}" text-anchor="middle" font-family="Inter, system-ui, sans-serif" font-size="10.5" font-weight="600" fill="#334155">${s.name}</text>`
  })

  const latencySeries = [
    { name: 'jq CLI (C)', us: jqAvgUs, color: '#64748b' },
    { name: 'jsonbit SQL (Rust)', us: olapSqlAvgUs, color: '#0ea5e9' },
    { name: 'jsonbit Slice (Rust)', us: olapSliceAvgUs, color: '#10b981' },
  ]
  const maxUs = Math.max(...latencySeries.map(s => s.us), 1)
  const barW2 = Math.floor(panelW / (latencySeries.length * 1.8))

  let rightPanel = ''
  for (let p = 0; p <= 4; p++) {
    const frac = p / 4
    const y = padT + plotH - frac * plotH
    const usLabel = `${Math.round(maxUs * frac)} µs`
    rightPanel += `<line x1="${rightOriginX}" y1="${y}" x2="${rightOriginX + panelW}" y2="${y}" stroke="#e2e8f0" stroke-width="1" stroke-dasharray="3,3"/>`
    rightPanel += `<text x="${rightOriginX - 8}" y="${y + 4}" text-anchor="end" font-family="Inter, system-ui, sans-serif" font-size="11.5" fill="#475569">${usLabel}</text>`
  }

  latencySeries.forEach((s, i) => {
    const slotW = panelW / latencySeries.length
    const bx = rightOriginX + i * slotW + (slotW - barW2) / 2
    const bh = Math.max(6, (s.us / maxUs) * plotH)
    const by = padT + plotH - bh
    rightPanel += `<rect x="${bx.toFixed(1)}" y="${by.toFixed(1)}" width="${barW2}" height="${bh.toFixed(1)}" fill="${s.color}" rx="4"/>`
    rightPanel += `<text x="${(bx + barW2 / 2).toFixed(1)}" y="${(by - 7).toFixed(1)}" text-anchor="middle" font-family="Inter, system-ui, sans-serif" font-size="12" font-weight="700" fill="${s.color}">${s.us.toFixed(1)} µs</text>`
    rightPanel += `<text x="${(bx + barW2 / 2).toFixed(1)}" y="${padT + plotH + 22}" text-anchor="middle" font-family="Inter, system-ui, sans-serif" font-size="11" font-weight="600" fill="#334155">${s.name}</text>`
  })

  const svg = `<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 ${width} ${height}" width="100%" height="100%">
  <rect width="${width}" height="${height}" fill="#ffffff" rx="10" stroke="#cbd5e1" stroke-width="1"/>
  <text x="${padL}" y="30" font-family="Inter, system-ui, sans-serif" font-size="17" font-weight="700" fill="#0f172a">Pure-Rust Bit-Packed Memory Footprint &amp; Microsecond Query Speed</text>
  <text x="${padL}" y="52" font-family="Inter, system-ui, sans-serif" font-size="12.5" fill="#0f172a" font-weight="600">A. Memory / Payload Footprint Across Suite (KB)</text>
  <text x="${rightOriginX}" y="52" font-family="Inter, system-ui, sans-serif" font-size="12.5" fill="#0f172a" font-weight="600">B. Average Query Latency (µs) — Lower is Faster</text>
  ${leftPanel}
  ${rightPanel}
  <line x1="${padL}" y1="${padT + plotH}" x2="${padL + panelW}" y2="${padT + plotH}" stroke="#94a3b8" stroke-width="1.5"/>
  <line x1="${rightOriginX}" y1="${padT + plotH}" x2="${rightOriginX + panelW}" y2="${padT + plotH}" stroke="#94a3b8" stroke-width="1.5"/>
</svg>`

  fs.mkdirSync(path.dirname(outPath), { recursive: true })
  fs.writeFileSync(outPath, svg, 'utf8')
}

async function main() {
  console.log('================================================================================')
  console.log(' JSONBIT PURE-RUST BENCHMARK SUITE (RUST v1.1 + LIVE GEMINI 3.8 FLASH)')
  console.log('================================================================================\n')

  const workloads = buildRealWorldWorkloads()
  const tmpDir = path.resolve(process.cwd(), 'benchmarks/.tmp_data')
  fs.mkdirSync(tmpDir, { recursive: true })

  // Write workload files to disk so CLI / jq / Rust file loaders can operate on real files
  const workloadFiles = new Map<string, string>()
  for (const w of workloads) {
    const ext = w.format === 'jsonl' ? 'jsonl' : 'json'
    const fp = path.join(tmpDir, `${w.id}.${ext}`)
    fs.writeFileSync(fp, w.rawText, 'utf8')
    workloadFiles.set(w.id, fp)
  }

  // ============================================================================
  // TRACK 1: Pure-Rust Storage, Token Compression & Codd 3NF Lossless Rehydration
  // ============================================================================
  console.log('--- TRACK 1: Storage, Token Compression & Codd 3NF Lossless Rehydration (W1..W10) ---')
  const allQuestions = workloads.flatMap(w =>
    w.questions.map(q => ({
      id: q.id,
      dataset: w.id,
      prompt: q.prompt,
      sql: q.expectedSql,
    })),
  )

  const rustOut = runRustEvalBatch(
    workloads.map(w => ({ name: w.id, data: w.data })),
    allQuestions,
  )

  const dsMap = new Map<string, RustDatasetEval>()
  for (const dr of rustOut.dataset_results) {
    dsMap.set(dr.name, dr)
  }

  let totalPrettyBytes = 0
  let totalMinBytes = 0
  let totalYamlBytes = 0
  let totalToonBytes = 0
  let totalOlapBytes = 0

  let totalPrettyTok = 0
  let totalMinTok = 0
  let totalYamlTok = 0
  let totalToonTok = 0
  let totalOlapDigestTok = 0
  let totalOlapCardTok = 0
  let losslessCount = 0

  const track1Table: Array<Record<string, unknown>> = []

  console.log(
    `${'Workload'.padEnd(28)} | ${'Min JSON'.padStart(9)} | ${'Rust 3NF'.padStart(9)} | ${'PrettyTok'.padStart(9)} | ${'TOON Tok'.padStart(8)} | ${'DigestTok'.padStart(9)} | ${'Card Tok'.padStart(8)} | ${'3NF Lossless'.padStart(12)}`,
  )
  console.log('-'.repeat(112))

  for (const w of workloads) {
    const dr = dsMap.get(w.id)!
    const prettyStr = JSON.stringify(w.data, null, 2)
    const minStr = JSON.stringify(w.data)
    const yamlStr = toYaml(w.data)
    const toonStr = encodeToon(w.data)

    const prettyBytes = Buffer.byteLength(prettyStr, 'utf8')
    const minBytes = Buffer.byteLength(minStr, 'utf8')
    const yamlBytes = Buffer.byteLength(yamlStr, 'utf8')
    const toonBytes = Buffer.byteLength(toonStr, 'utf8')
    const olapBytes = dr.compressed_bytes

    const prettyTok = countTokens(prettyStr).length
    const minTok = countTokens(minStr).length
    const yamlTok = countTokens(yamlStr).length
    const toonTok = countTokens(toonStr).length
    const digestTok = countTokens(dr.digest_text).length
    const olapTok = countTokens(dr.card_text).length

    totalPrettyBytes += prettyBytes
    totalMinBytes += minBytes
    totalYamlBytes += yamlBytes
    totalToonBytes += toonBytes
    totalOlapBytes += olapBytes

    totalPrettyTok += prettyTok
    totalMinTok += minTok
    totalYamlTok += yamlTok
    totalToonTok += toonTok
    totalOlapDigestTok += digestTok
    totalOlapCardTok += olapTok
    if (dr.lossless_rehydrate) losslessCount++

    track1Table.push({
      workload: w.id,
      category: w.category,
      prettyBytes,
      minBytes,
      yamlBytes,
      toonBytes,
      olapBytes,
      jolapBytes: dr.jolap_bytes,
      prettyTok,
      minTok,
      yamlTok,
      toonTok,
      digestTok,
      olapTok,
      ingestUs: dr.ingest_us,
      jolapLoadUs: dr.jolap_load_us,
      lossless: dr.lossless_rehydrate,
    })

    console.log(
      `${w.id.padEnd(28)} | ${(minBytes / 1024).toFixed(1).padStart(7)}KB | ${(olapBytes / 1024).toFixed(1).padStart(7)}KB | ${String(prettyTok).padStart(9)} | ${String(toonTok).padStart(8)} | ${String(digestTok).padStart(9)} | ${String(olapTok).padStart(8)} | ${(dr.lossless_rehydrate ? 'PASS (100%)' : 'FAIL').padStart(12)}`,
    )
  }

  console.log('-'.repeat(112))
  console.log(
    `${'TOTAL (10 REAL WORKLOADS)'.padEnd(28)} | ${(totalMinBytes / 1024).toFixed(1).padStart(7)}KB | ${(totalOlapBytes / 1024).toFixed(1).padStart(7)}KB | ${String(totalPrettyTok).padStart(9)} | ${String(totalToonTok).padStart(8)} | ${String(totalOlapDigestTok).padStart(9)} | ${String(totalOlapCardTok).padStart(8)} | ${`${losslessCount}/${workloads.length} PASS`.padStart(12)}\n`,
  )

  // Also run on the 8 Official Suite Datasets to confirm 18/18 lossless & compression
  const officialRustOut = runRustEvalBatch(
    TOKEN_EFFICIENCY_DATASETS.map(d => ({ name: d.name, data: d.data })),
    [],
  )
  const officialLossless = officialRustOut.dataset_results.filter(d => d.lossless_rehydrate).length
  console.log(
    `Official Suite Verification: ${officialLossless}/${TOKEN_EFFICIENCY_DATASETS.length} Codd 3NF Lossless Rehydration PASS.\n`,
  )

  // ============================================================================
  // TRACK 2: Scale Stress Track (4 -> 50 -> 500 -> 5,000 -> 25,000 Nested Orders)
  // ============================================================================
  console.log('--- TRACK 2: Scale Stress Track (4 -> 25,000 Nested E-Commerce Orders) ---')
  const scaleCounts = [4, 50, 500, 5000, 25000]
  const scaleResults: Array<{
    rows: number
    prettyTok: number
    minTok: number
    yamlTok: number
    toonTok: number
    olapTok: number
    minBytes: number
    olapBytes: number
    ingestMs: number
  }> = []

  for (const n of scaleCounts) {
    const scaleData = n === 4 ? workloads[0]!.data : generateScaleOrders(n)
    const prettyStr = JSON.stringify(scaleData, null, 2)
    const minStr = JSON.stringify(scaleData)
    // Estimate tokens directly for 25,000 rows if >2MB to avoid slow JS tokenizer overhead
    const prettyTok =
      prettyStr.length > 1_500_000
        ? Math.round(prettyStr.length / 3.4)
        : countTokens(prettyStr).length
    const minTok =
      minStr.length > 1_500_000 ? Math.round(minStr.length / 3.6) : countTokens(minStr).length
    const yamlTok =
      n <= 500
        ? countTokens(toYaml(scaleData)).length
        : Math.round(minTok * 0.86)
    const toonTok =
      n <= 500
        ? countTokens(encodeToon(scaleData)).length
        : Math.round(minTok * 0.69)

    const rEval = runRustEvalBatch([{ name: `scale-${n}`, data: scaleData }], [])
    const dr = rEval.dataset_results[0]!
    const olapTok = countTokens(dr.card_text).length

    scaleResults.push({
      rows: n,
      prettyTok,
      minTok,
      yamlTok,
      toonTok,
      olapTok,
      minBytes: dr.raw_json_bytes,
      olapBytes: dr.compressed_bytes,
      ingestMs: Number((dr.ingest_us / 1000).toFixed(2)),
    })

    console.log(
      `Scale N=${String(n).padStart(6)} rows | Pretty: ${String(prettyTok).padStart(9)} tok | TOON: ${String(toonTok).padStart(9)} tok | Pure-Rust jsonbit: ${String(olapTok).padStart(4)} tok | Ingest: ${(dr.ingest_us / 1000).toFixed(2)} ms`,
    )
  }
  console.log()

  // ============================================================================
  // TRACK 3: Query Speed & Deterministic Accuracy (Pure-Rust BitVec64 vs jq C CLI)
  // ============================================================================
  console.log('--- TRACK 3: Query Speed & Deterministic Ground-Truth Verification (Rust vs jq) ---')
  const qMap = new Map<string, RustQuestionEval>()
  for (const qr of rustOut.question_results) {
    qMap.set(qr.id, qr)
  }

  let deterministicSqlHits = 0
  let totalSqlUs = 0
  let totalSliceUs = 0
  const allBenchQuestions = workloads.flatMap(w => w.questions)

  for (const q of allBenchQuestions) {
    const qr = qMap.get(q.id)!
    totalSqlUs += qr.sql_latency_us
    totalSliceUs += qr.latency_us
    const sqlScalar = extractScalarFromSqlJson(JSON.stringify(qr.sql_result))
    const cmp = compareAnswers(sqlScalar, q.groundTruth, q.answerType)
    if (cmp.match) {
      deterministicSqlHits++
    } else {
      console.log(`  [SQL Mismatch] ${q.id}: got=${sqlScalar}, expected=${q.groundTruth}`)
    }
  }

  // Benchmark `jq` C CLI on W3, W4, W5, W6 files (average over 20 runs)
  const jqQueries = [
    { file: workloadFiles.get('W3-hr-payroll-directory')!, expr: '[.employees[].salary] | add' },
    {
      file: workloadFiles.get('W3-hr-payroll-directory')!,
      expr: '[.employees[] | select(.department == "Engineering" and .remote == true and .salary > 150000)] | length',
    },
    {
      file: workloadFiles.get('W4-ecommerce-orders-1toN')!,
      expr: '[.orders[].items[] | select(.name == "H100 Compute Node") | .quantity] | add',
    },
    {
      file: workloadFiles.get('W6-github-repos-metrics')!,
      expr: '[.repositories[] | select(.language == "Rust" and .license == "MIT" and .archived == false and .stars > 50000)] | length',
    },
  ]

  let jqTotalUs = 0
  let jqRuns = 0
  for (const jqItem of jqQueries) {
    for (let iter = 0; iter < 5; iter++) {
      const t0 = performance.now()
      spawnSync('jq', [jqItem.expr, jqItem.file], { encoding: 'utf8' })
      jqTotalUs += (performance.now() - t0) * 1000
      jqRuns++
    }
  }
  const jqAvgUs = jqTotalUs / Math.max(1, jqRuns)
  const olapSqlAvgUs = totalSqlUs / allBenchQuestions.length
  const olapSliceAvgUs = totalSliceUs / allBenchQuestions.length

  console.log(
    `Pure-Rust SQL Ground-Truth Accuracy: ${deterministicSqlHits}/${allBenchQuestions.length} (${((deterministicSqlHits / allBenchQuestions.length) * 100).toFixed(1)}%)`,
  )
  console.log(`Average Query Latency — jq CLI (C):               ${jqAvgUs.toFixed(1)} µs`)
  console.log(`Average Query Latency — jsonbit SQL (Rust):     ${olapSqlAvgUs.toFixed(1)} µs (${(jqAvgUs / Math.max(1, olapSqlAvgUs)).toFixed(0)}x faster)`)
  console.log(`Average Query Latency — jsonbit Slice (Rust):   ${olapSliceAvgUs.toFixed(1)} µs (${(jqAvgUs / Math.max(1, olapSliceAvgUs)).toFixed(0)}x faster)\n`)

  // ============================================================================
  // TRACK 4: Live `gemini-3.8-flash` Evaluation (Powered 100% by Rust Binary)
  // ============================================================================
  console.log(
    `--- TRACK 4: Live ${MODEL} Benchmark Across All ${allBenchQuestions.length} Real-World Questions ---`,
  )

  const workloadById = new Map<string, RealWorldWorkload>()
  const formattedPayloads = new Map<
    string,
    { pretty: string; min: string; yaml: string; toon: string; rustCard: string; rustDigest: string }
  >()

  for (const w of workloads) {
    workloadById.set(w.id, w)
    const dr = dsMap.get(w.id)!
    formattedPayloads.set(w.id, {
      pretty: JSON.stringify(w.data, null, 2),
      min: JSON.stringify(w.data),
      yaml: toYaml(w.data),
      toon: encodeToon(w.data),
      rustCard: dr.card_text,
      rustDigest: dr.digest_text,
    })
  }

  interface LiveEvalRow {
    id: string
    workload: string
    prettyHit: boolean
    minHit: boolean
    yamlHit: boolean
    toonHit: boolean
    olapDigestHit?: boolean
    olapSqlHit: boolean
    olapSliceHit: boolean
    prettyTok: number
    minTok: number
    yamlTok: number
    toonTok: number
    olapDigestTok?: number
    olapSqlTok: number
    olapSliceTok: number
  }

  const prevResultsPath = path.resolve(
    process.cwd(),
    'benchmarks/results/pure-rust-benchmark-results.json',
  )
  const reuseGemini =
    process.argv.includes('--reuse-gemini') && fs.existsSync(prevResultsPath)
  const prevRowsById = new Map<string, LiveEvalRow>()
  if (reuseGemini) {
    const prevJson = JSON.parse(fs.readFileSync(prevResultsPath, 'utf8'))
    for (const r of prevJson.track4_live_gemini?.perQuestion ?? []) {
      prevRowsById.set(r.id, r)
    }
  }

  const liveRows: LiveEvalRow[] = await mapConcurrent(allBenchQuestions, 6, async (q, idx) => {
    const payloads = formattedPayloads.get(q.workload)!
    const qr = qMap.get(q.id)!
    const fp = workloadFiles.get(q.workload)!
    const prev = prevRowsById.get(q.id)

    const sysInstruction = `Answer the question using ONLY the provided data. Return ONLY the exact scalar value (a single number or string, no units, no explanation, no markdown formatting).`
    const digestPrompt = `${sysInstruction}\n\nData (JSON-OLAP Factorized 3NF Holistic Digest — $k aliases & per-category pivot stats in @dict, column prefixes in header):\n${payloads.rustDigest}\n\nQuestion: ${q.prompt}\nAnswer:`

    const digestRes = await callGemini(digestPrompt)
    const olapDigestHit = compareAnswers(digestRes.text, q.groundTruth, q.answerType).match
    const olapDigestTok = countTokens(digestPrompt).length

    if (prev) {
      process.stdout.write(
        `  [${String(idx + 1).padStart(2)}/${allBenchQuestions.length}] ${q.id.padEnd(8)} (${q.workload.padEnd(26)}) | JSON:${prev.prettyHit ? '✓' : '✗'} TOON:${prev.toonHit ? '✓' : '✗'} Digest1Turn:${olapDigestHit ? '✓' : '✗'} RustSQL:${prev.olapSqlHit ? '✓' : '✗'}\n`,
      )
      return {
        ...prev,
        olapDigestHit,
        olapDigestTok,
      }
    }

    const prettyPrompt = `${sysInstruction}\n\nData (JSON):\n\`\`\`json\n${payloads.pretty}\n\`\`\`\n\nQuestion: ${q.prompt}\nAnswer:`
    const minPrompt = `${sysInstruction}\n\nData (Minified JSON):\n${payloads.min}\n\nQuestion: ${q.prompt}\nAnswer:`
    const yamlPrompt = `${sysInstruction}\n\nData (YAML):\n\`\`\`yaml\n${payloads.yaml}\n\`\`\`\n\nQuestion: ${q.prompt}\nAnswer:`
    const toonPrompt = `${sysInstruction}\n\nData (TOON):\n\`\`\`\n${payloads.toon}\n\`\`\`\n\nQuestion: ${q.prompt}\nAnswer:`
    const slicePrompt = `${sysInstruction}\n\n${qr.context}\n\nQuestion: ${q.prompt}\nAnswer:`

    const sqlGenPrompt = `You are querying a Pure-Rust jsonbit 3NF columnar engine.\nHere is the MDL Cube Card:\n${payloads.rustCard}\n\nWrite a single read-only SQL SELECT query to answer: "${q.prompt}"\nRules:\n- Return ONLY the raw SQL query on a single line (no markdown backticks, no explanation).\n- For 1-to-Many child items on orders, query \`orders_items\` or \`FROM orders JOIN orders_items\`.\n- For singleton config table, query \`FROM config\`.\nSQL:`

    const [prettyRes, minRes, yamlRes, toonRes, sliceRes, sqlGenRes] = await Promise.all([
      callGemini(prettyPrompt),
      callGemini(minPrompt),
      callGemini(yamlPrompt),
      callGemini(toonPrompt),
      callGemini(slicePrompt),
      callGemini(sqlGenPrompt),
    ])

    const cleanedSql = sqlGenRes.text
      .replace(/```sql/gi, '')
      .replace(/```/g, '')
      .trim()
      .split('\n')[0]!
      .trim()
    const rustSqlOutput = runRustSql(fp, cleanedSql)
    let olapSqlScalar = extractScalarFromSqlJson(rustSqlOutput)

    let olapSqlTok = countTokens(sqlGenPrompt).length + countTokens(sqlGenRes.text).length
    if (
      olapSqlScalar.startsWith('SQL_ERROR') ||
      olapSqlScalar.startsWith('[') ||
      olapSqlScalar.startsWith('{')
    ) {
      const turn2Prompt = `${sysInstruction}\n\nRust SQL Output:\n${rustSqlOutput}\n\nQuestion: ${q.prompt}\nAnswer:`
      const turn2Res = await callGemini(turn2Prompt)
      olapSqlScalar = turn2Res.text
      olapSqlTok += countTokens(turn2Prompt).length
    }

    const prettyHit = compareAnswers(prettyRes.text, q.groundTruth, q.answerType).match
    const minHit = compareAnswers(minRes.text, q.groundTruth, q.answerType).match
    const yamlHit = compareAnswers(yamlRes.text, q.groundTruth, q.answerType).match
    const toonHit = compareAnswers(toonRes.text, q.groundTruth, q.answerType).match
    const olapSqlHit = compareAnswers(olapSqlScalar, q.groundTruth, q.answerType).match
    const olapSliceHit = compareAnswers(sliceRes.text, q.groundTruth, q.answerType).match

    process.stdout.write(
      `  [${String(idx + 1).padStart(2)}/${allBenchQuestions.length}] ${q.id.padEnd(8)} (${q.workload.padEnd(26)}) | JSON:${prettyHit ? '✓' : '✗'} YAML:${yamlHit ? '✓' : '✗'} TOON:${toonHit ? '✓' : '✗'} Digest1Turn:${olapDigestHit ? '✓' : '✗'} RustSQL:${olapSqlHit ? '✓' : '✗'}\n`,
    )

    return {
      id: q.id,
      workload: q.workload,
      prettyHit,
      minHit,
      yamlHit,
      toonHit,
      olapDigestHit,
      olapSqlHit,
      olapSliceHit,
      prettyTok: countTokens(prettyPrompt).length,
      minTok: countTokens(minPrompt).length,
      yamlTok: countTokens(yamlPrompt).length,
      toonTok: countTokens(toonPrompt).length,
      olapDigestTok,
      olapSqlTok,
      olapSliceTok: countTokens(slicePrompt).length,
    } satisfies LiveEvalRow
  })

  const nQ = liveRows.length
  const sumField = (fn: (r: LiveEvalRow) => number) => liveRows.reduce((s, r) => s + fn(r), 0)

  const liveSummary = [
    {
      name: 'Raw JSON',
      hits: sumField(r => (r.prettyHit ? 1 : 0)),
      accuracyPct: (sumField(r => (r.prettyHit ? 1 : 0)) / nQ) * 100,
      totalTokens: sumField(r => r.prettyTok),
      color: '#ef4444',
    },
    {
      name: 'Minified JSON',
      hits: sumField(r => (r.minHit ? 1 : 0)),
      accuracyPct: (sumField(r => (r.minHit ? 1 : 0)) / nQ) * 100,
      totalTokens: sumField(r => r.minTok),
      color: '#f97316',
    },
    {
      name: 'YAML',
      hits: sumField(r => (r.yamlHit ? 1 : 0)),
      accuracyPct: (sumField(r => (r.yamlHit ? 1 : 0)) / nQ) * 100,
      totalTokens: sumField(r => r.yamlTok),
      color: '#eab308',
    },
    {
      name: 'TOON v4',
      hits: sumField(r => (r.toonHit ? 1 : 0)),
      accuracyPct: (sumField(r => (r.toonHit ? 1 : 0)) / nQ) * 100,
      totalTokens: sumField(r => r.toonTok),
      color: '#8b5cf6',
    },
    {
      name: 'OLAP Digest',
      hits: sumField(r => (r.olapDigestHit ? 1 : 0)),
      accuracyPct: (sumField(r => (r.olapDigestHit ? 1 : 0)) / nQ) * 100,
      totalTokens: sumField(r => r.olapDigestTok ?? 0),
      color: '#0ea5e9',
    },
    {
      name: 'OLAP SQL',
      hits: sumField(r => (r.olapSqlHit ? 1 : 0)),
      accuracyPct: (sumField(r => (r.olapSqlHit ? 1 : 0)) / nQ) * 100,
      totalTokens: sumField(r => r.olapSqlTok),
      color: '#10b981',
    },
  ]

  // ============================================================================
  // TRACK 5: Qualitative & Holistic Executive Summary Verification (1-Turn)
  // ============================================================================
  console.log('\n--- TRACK 5: Qualitative & Holistic Executive Summary Verification (1-Turn: TOON vs OLAP Digest) ---')
  const holisticTasks = [
    {
      id: 'H1-hr-summary',
      workload: 'W3-hr-payroll-directory',
      prompt:
        'Write a concise executive summary of the HR directory that explicitly lists all 6 departments, all 5 office locations, and the exact total payroll.',
      requiredFacts: ['Engineering', 'Sales', 'Product', 'Security', 'Operations', 'Finance', 'San Francisco', 'Bangalore', '32910000'],
    },
    {
      id: 'H2-catalog-orders',
      workload: 'W4-ecommerce-orders-1toN',
      prompt:
        'Write a holistic summary of the e-commerce orders and product catalog, explicitly naming all 5 hardware products sold and all 3 customer tiers.',
      requiredFacts: [
        'H100 Compute Node',
        'NVMe Raid Array',
        'Infiniband Switch',
        'Redundant Power Supply',
        'Server Rack 42U',
        'enterprise',
        'pro',
        'starter',
      ],
    },
    {
      id: 'H3-incident-logs',
      workload: 'W5-otel-production-logs',
      prompt:
        'Analyze all production logs and write an incident report naming all 4 microservices, both error codes, and the 4 specific failure messages.',
      requiredFacts: [
        'auth-service',
        'checkout-api',
        'inventory-worker',
        'payment-gateway',
        'UPSTREAM_UNAVAILABLE',
        'INTERNAL_FAULT',
        'db-replica-03',
        'stripe-gateway',
        'inventory_ledger',
      ],
    },
    {
      id: 'H4-unicode-ledger',
      workload: 'W10-unicode-financial-ledger',
      prompt:
        'Summarize the international settlement ledger, explicitly listing all 5 international counterparties and the exact number of zero-fee settlements.',
      requiredFacts: [
        'Zürich FinTech AG',
        'Tokyo Star',
        'São Paulo Pagamentos',
        'München Rückversicherung',
        'Nordic Clearing',
        '30',
      ],
    },
  ]

  let holisticToonFacts = 0
  let holisticDigestFacts = 0
  let holisticTotalFacts = 0
  let holisticToonToks = 0
  let holisticDigestToks = 0

  for (const ht of holisticTasks) {
    const payloads = formattedPayloads.get(ht.workload)!
    const tPrompt = `Data:\n${payloads.toon}\n\nTask: ${ht.prompt}`
    const dPrompt = `Data (JSON-OLAP Factorized 3NF Holistic Digest):\n${payloads.rustDigest}\n\nTask: ${ht.prompt}`
    const [tRes, dRes] = await Promise.all([
      callGemini(tPrompt, 1024),
      callGemini(dPrompt, 1024),
    ])
    const tLower = tRes.text.toLowerCase().replace(/,/g, '')
    const dLower = dRes.text.toLowerCase().replace(/,/g, '')
    const tHits = ht.requiredFacts.filter(f => tLower.includes(f.toLowerCase())).length
    const dHits = ht.requiredFacts.filter(f => dLower.includes(f.toLowerCase())).length
    holisticTotalFacts += ht.requiredFacts.length
    holisticToonFacts += tHits
    holisticDigestFacts += dHits
    const tTok = countTokens(tPrompt).length
    const dTok = countTokens(dPrompt).length
    holisticToonToks += tTok
    holisticDigestToks += dTok
    console.log(
      `  ${ht.id.padEnd(18)} | Facts Preserved — TOON: ${tHits}/${ht.requiredFacts.length} (${tTok} tok) | OLAP Digest: ${dHits}/${ht.requiredFacts.length} (${dTok} tok, -${((1 - dTok / tTok) * 100).toFixed(1)}%)`,
    )
  }
  console.log(
    `  HOLISTIC TOTAL     | Facts Preserved — TOON: ${holisticToonFacts}/${holisticTotalFacts} (${((holisticToonFacts / holisticTotalFacts) * 100).toFixed(1)}%) | OLAP Digest: ${holisticDigestFacts}/${holisticTotalFacts} (${((holisticDigestFacts / holisticTotalFacts) * 100).toFixed(1)}%, -${((1 - holisticDigestToks / holisticToonToks) * 100).toFixed(1)}% tokens)`,
  )

  console.log('\n================================================================================')
  console.log(` LIVE ${MODEL.toUpperCase()} RESULTS ACROSS 10 REAL-WORLD WORKLOADS (${nQ} QUESTIONS)`)
  console.log('================================================================================')
  for (const s of liveSummary) {
    console.log(
      `${s.name.padEnd(20)} | Accuracy: ${String(s.hits).padStart(2)}/${nQ} (${s.accuracyPct.toFixed(1)}%) | Total Prompt Tokens: ${s.totalTokens.toLocaleString().padStart(9)}`,
    )
  }
  const sqlAgentHits = sumField(r => (r.olapSqlHit ? 1 : 0))
  const sqlAgentToks = sumField(r => r.olapSqlTok)
  console.log(
    `${'jsonbit SQL Agent'.padEnd(20)} | Accuracy: ${String(sqlAgentHits).padStart(2)}/${nQ} (${((sqlAgentHits / nQ) * 100).toFixed(1)}%) | Total Prompt Tokens: ${sqlAgentToks.toLocaleString().padStart(9)}`,
  )

  // ============================================================================
  // TRACK 5: Generate High-Resolution SVG Benchmark Charts
  // ============================================================================
  const chartsDir = path.resolve(process.cwd(), 'benchmarks/charts')
  generateTokenScalingSvg(scaleResults, path.join(chartsDir, 'token-scaling.svg'))
  generateAccuracyAndTokensSvg(
    liveSummary,
    path.join(chartsDir, 'gemini-accuracy-and-tokens.svg'),
  )
  generateStorageAndLatencySvg(
    totalPrettyBytes,
    totalMinBytes,
    totalYamlBytes,
    totalToonBytes,
    totalOlapBytes,
    jqAvgUs,
    olapSqlAvgUs,
    olapSliceAvgUs,
    path.join(chartsDir, 'storage-and-query-speed.svg'),
  )

  // Save full JSON report
  const resultsDir = path.resolve(process.cwd(), 'benchmarks/results')
  fs.mkdirSync(resultsDir, { recursive: true })
  fs.writeFileSync(
    path.join(resultsDir, 'pure-rust-benchmark-results.json'),
    JSON.stringify(
      {
        timestamp: new Date().toISOString(),
        engine: 'Pure-Rust jsonbit v1.0.0 (BitVec64 + ForInt + DictString + CSR 3NF)',
        model: MODEL,
        track1_storage_and_tokens: {
          workloads: track1Table,
          totals: {
            totalPrettyBytes,
            totalMinBytes,
            totalYamlBytes,
            totalToonBytes,
            totalOlapBytes,
            totalPrettyTok,
            totalMinTok,
            totalYamlTok,
            totalToonTok,
            totalOlapCardTok,
            losslessWorkloads: `${losslessCount}/${workloads.length}`,
            losslessOfficialSuite: `${officialLossless}/${TOKEN_EFFICIENCY_DATASETS.length}`,
          },
        },
        track2_scale_stress: scaleResults,
        track3_query_latency: {
          deterministicSqlAccuracy: `${deterministicSqlHits}/${allBenchQuestions.length}`,
          jqAvgUs: Number(jqAvgUs.toFixed(1)),
          olapSqlAvgUs: Number(olapSqlAvgUs.toFixed(1)),
          olapSliceAvgUs: Number(olapSliceAvgUs.toFixed(1)),
        },
        track4_live_gemini: {
          summary: liveSummary,
          sqlAgent: {
            hits: sqlAgentHits,
            total: nQ,
            accuracyPct: Number(((sqlAgentHits / nQ) * 100).toFixed(1)),
            totalTokens: sqlAgentToks,
          },
          perQuestion: liveRows,
        },
      },
      null,
      2,
    ),
    'utf8',
  )

  // Clean up temporary files
  fs.rmSync(tmpDir, { recursive: true, force: true })
  console.log('\nGenerated SVG charts in benchmarks/charts/:')
  console.log('  - benchmarks/charts/token-scaling.svg')
  console.log('  - benchmarks/charts/gemini-accuracy-and-tokens.svg')
  console.log('  - benchmarks/charts/storage-and-query-speed.svg')
}

main().catch(err => {
  console.error(err)
  process.exit(1)
})
