/**
 * Multi-Scenario Autonomous Coding Agent Benchmark (`OpenCode v1.19.5`)
 *
 * Compares 3 isolated agent environments (NO AGENTS.md, NO CLAUDE.md, NO prompt hints):
 *  1. `jsonbit` MCP (`opencode.json` only)
 *  2. Raw JSON / JSONL (No MCP)
 *  3. `TOON` v4 (No MCP)
 *
 * Across 6 diverse production scenarios (18 live OpenCode runs total).
 */
import { spawnSync } from 'node:child_process'
import * as fs from 'node:fs'
import * as path from 'node:path'
import { encode as encodeToon } from '@toon-format/toon'
import { buildRealWorldWorkloads } from './datasets/real-world-workloads.ts'
import { TOKEN_EFFICIENCY_DATASETS } from './toon-suite/datasets.ts'

const MODEL = 'opencode/big-pickle'
const OPENCODE_BIN = '/snap/bin/opencode'
const JSONBIT_BIN = path.resolve(process.cwd(), 'target/release/jsonbit')

interface Scenario {
  id: string
  name: string
  promptJson: string
  promptToon: string
  expectedTokens: string[]
}

function setupEnvironments() {
  const workloads = buildRealWorldWorkloads()
  const byId = new Map(workloads.map(w => [w.id, w]))
  const nested500 = TOKEN_EFFICIENCY_DATASETS.find(d => d.name === 'nested')!

  // Build unseen S15 healthcare claims & S10 5,000-line traces
  const claimsData = {
    claims: Array.from({ length: 80 }, (_, i) => ({
      claimId: `CLM-${7000 + i}`,
      patient: { age: 25 + (i % 55), state: ['CA', 'NY', 'TX', 'FL'][i % 4]! },
      approved: i % 5 !== 0,
      billedUsd: 500 + (i % 10) * 250,
      procedures: [
        { code: i % 2 === 0 ? 'CPT-99213' : 'CPT-99214', units: 1 + (i % 3) },
        { code: 'CPT-80050', units: 1 },
      ],
    })),
  }
  const traces5k = Array.from({ length: 5000 }, (_, i) => ({
    reqId: `req-${(i * 2654435761) >>> 0}-${i}`,
    region: ['us-east', 'us-west', 'eu-central', 'ap-south'][i % 4]!,
    status: i % 17 === 0 ? 502 : 200,
    ms: 10 + (i % 500),
    ...(i % 17 === 0 ? { err: { retryable: i % 34 === 0 } } : {}),
  }))

  const base = '/tmp/oc-bench'
  fs.rmSync(base, { recursive: true, force: true })
  for (const sub of ['json', 'toon', 'jsonbit']) {
    fs.mkdirSync(path.join(base, sub), { recursive: true })
  }

  // Write opencode.json ONLY in /tmp/oc-bench/jsonbit (NO AGENTS.md, NO CLAUDE.md)
  fs.writeFileSync(
    path.join(base, 'jsonbit', 'opencode.json'),
    JSON.stringify(
      {
        $schema: 'https://opencode.ai/config.json',
        mcp: {
          jsonbit: {
            type: 'local',
            command: [JSONBIT_BIN, 'mcp'],
            enabled: true,
          },
        },
      },
      null,
      2,
    ),
  )

  const files: Array<{ key: string; extJson: string; data: unknown; rawJson: string }> = [
    {
      key: 'w4_orders',
      extJson: 'json',
      data: byId.get('W4-ecommerce-orders-1toN')!.data,
      rawJson: byId.get('W4-ecommerce-orders-1toN')!.rawText,
    },
    {
      key: 'w5_logs',
      extJson: 'jsonl',
      data: byId.get('W5-otel-production-logs')!.data,
      rawJson: byId.get('W5-otel-production-logs')!.rawText,
    },
    {
      key: 'w6_repos',
      extJson: 'json',
      data: byId.get('W6-github-repos-metrics')!.data,
      rawJson: byId.get('W6-github-repos-metrics')!.rawText,
    },
    {
      key: 'w8_iam',
      extJson: 'json',
      data: byId.get('W8-iam-primitive-arrays')!.data,
      rawJson: byId.get('W8-iam-primitive-arrays')!.rawText,
    },
    {
      key: 'w9_crm',
      extJson: 'json',
      data: byId.get('W9-wide-enterprise-crm')!.data,
      rawJson: byId.get('W9-wide-enterprise-crm')!.rawText,
    },
    {
      key: 'w10_ledger',
      extJson: 'json',
      data: byId.get('W10-unicode-financial-ledger')!.data,
      rawJson: byId.get('W10-unicode-financial-ledger')!.rawText,
    },
    {
      key: 'orders500',
      extJson: 'json',
      data: nested500.data,
      rawJson: JSON.stringify(nested500.data, null, 2),
    },
    {
      key: 'claims',
      extJson: 'json',
      data: claimsData,
      rawJson: JSON.stringify(claimsData, null, 2),
    },
    {
      key: 'traces5k',
      extJson: 'jsonl',
      data: { records: traces5k },
      rawJson: traces5k.map(r => JSON.stringify(r)).join('\n'),
    },
  ]

  for (const f of files) {
    fs.writeFileSync(path.join(base, 'json', `${f.key}.${f.extJson}`), f.rawJson, 'utf8')
    fs.writeFileSync(path.join(base, 'jsonbit', `${f.key}.${f.extJson}`), f.rawJson, 'utf8')
    fs.writeFileSync(path.join(base, 'toon', `${f.key}.toon`), encodeToon(f.data), 'utf8')
  }

  // Ground truth for claims & traces5k
  const stateTotals: Record<string, number> = {}
  for (const c of claimsData.claims) {
    if (c.approved) {
      stateTotals[c.patient.state] = (stateTotals[c.patient.state] ?? 0) + c.billedUsd
    }
  }
  const topState = Object.entries(stateTotals).sort((a, b) => b[1] - a[1])[0]![0]
  const tracesRetryable = traces5k.filter(r => r.status === 502 && r.err?.retryable === true).length

  const scenarios: Scenario[] = [
    {
      id: 'OC1-nested-csr-joins',
      name: '1-to-Many Nested Orders (150 orders / 380 items)',
      promptJson:
        'Look at w4_orders.json in this folder and tell me: 1) Total quantity of H100 Compute Node ordered across all orders, and 2) How many delivered orders include an Infiniband Switch.',
      promptToon:
        'Look at w4_orders.toon in this folder and tell me: 1) Total quantity of H100 Compute Node ordered across all orders, and 2) How many delivered orders include an Infiniband Switch.',
      expectedTokens: ['198', '13'],
    },
    {
      id: 'OC2-sparse-jsonl-triage',
      name: 'Sparse OTEL Production Logs (300 lines)',
      promptJson:
        "Inspect w5_logs.jsonl in this folder and tell me: 1) How many log records mention 'timeout' in their message, 2) How many checkout-api logs had a retryable error, and 3) Which service had the most total requests?",
      promptToon:
        "Inspect w5_logs.toon in this folder and tell me: 1) How many log records mention 'timeout' in their message, 2) How many checkout-api logs had a retryable error, and 3) Which service had the most total requests?",
      expectedTokens: ['31', '6', 'payment-gateway'],
    },
    {
      id: 'OC3-wide-crm-and-unicode',
      name: 'Wide 30-Col CRM + Unicode Financial Ledger',
      promptJson:
        "Check w9_crm.json and w10_ledger.json in this folder and tell me: 1) Total arrUsd of active Fintech accounts in w9_crm, and 2) How many settled transactions in w10_ledger are with '東京スター銀行 (Tokyo Star)'?",
      promptToon:
        "Check w9_crm.toon and w10_ledger.toon in this folder and tell me: 1) Total arrUsd of active Fintech accounts in w9_crm, and 2) How many settled transactions in w10_ledger are with '東京スター銀行 (Tokyo Star)'?",
      expectedTokens: ['4302000', '22'],
    },
    {
      id: 'OC4-iam-and-github-filters',
      name: 'IAM Primitive Arrays + GitHub Repo Telemetry',
      promptJson:
        "Check w8_iam.json and w6_repos.json in this folder and tell me: 1) How many principals in w8_iam have 'admin' in scopes with mfaEnabled=false, and 2) How many unarchived Rust MIT repos in w6_repos have > 50000 stars?",
      promptToon:
        "Check w8_iam.toon and w6_repos.toon in this folder and tell me: 1) How many principals in w8_iam have 'admin' in scopes with mfaEnabled=false, and 2) How many unarchived Rust MIT repos in w6_repos have > 50000 stars?",
      expectedTokens: ['10', '4'],
    },
    {
      id: 'OC5-claims-and-5k-traces',
      name: 'Healthcare FHIR Claims + 5,000-Line Sparse Traces',
      promptJson:
        'Check claims.json and traces5k.jsonl in this folder and tell me: 1) Which patient state has the highest total billedUsd for approved claims, and 2) How many status=502 records in traces5k had err.retryable=true?',
      promptToon:
        'Check claims.toon and traces5k.toon in this folder and tell me: 1) Which patient state has the highest total billedUsd for approved claims, and 2) How many status=502 records in traces5k had err.retryable=true?',
      expectedTokens: [topState, String(tracesRetryable)],
    },
    {
      id: 'OC6-4file-multi-analytics',
      name: '4-File Multi-Workload Suite (orders500 + logs + crm + ledger)',
      promptJson:
        "Analyze orders500.json, w5_logs.jsonl, w9_crm.json, and w10_ledger.json in this folder and answer: 1) Total revenue (sum of total) across all orders in orders500, 2) Checkout-api logs with a retryable error in w5_logs, 3) Total arrUsd of active Fintech accounts in w9_crm, and 4) Settled transactions with '東京スター銀行 (Tokyo Star)' in w10_ledger.",
      promptToon:
        "Analyze orders500.toon, w5_logs.toon, w9_crm.toon, and w10_ledger.toon in this folder and answer: 1) Total revenue (sum of total) across all orders in orders500, 2) Checkout-api logs with a retryable error in w5_logs, 3) Total arrUsd of active Fintech accounts in w9_crm, and 4) Settled transactions with '東京スター銀行 (Tokyo Star)' in w10_ledger.",
      expectedTokens: ['422863.1', '6', '4302000', '22'],
    },
  ]

  return { base, scenarios }
}

function runOpenCode(cwd: string, prompt: string) {
  const t0 = performance.now()
  const res = spawnSync(OPENCODE_BIN, ['run', '-m', MODEL, prompt], {
    cwd,
    encoding: 'utf8',
    maxBuffer: 20 * 1024 * 1024,
    timeout: 180_000,
  })
  const elapsedSec = (performance.now() - t0) / 1000
  const out = (res.stdout || '') + '\n' + (res.stderr || '')
  return { status: res.status, elapsedSec, out }
}

function checkExpected(output: string, expectedTokens: string[]): boolean {
  const normalized = output.replace(/,/g, '')
  return expectedTokens.every(tok => normalized.includes(tok))
}

function main() {
  const { base, scenarios } = setupEnvironments()
  console.log(`Running ${scenarios.length} OpenCode (${MODEL}) scenarios across 3 isolated setups...\n`)

  const results: Array<{
    id: string
    name: string
    json: { sec: number; ok: boolean; scripts: boolean }
    toon: { sec: number; ok: boolean; scripts: boolean }
    jsonbit: { sec: number; ok: boolean; mcpUsed: boolean; scripts: boolean }
  }> = []

  for (const sc of scenarios) {
    const rBit = runOpenCode(path.join(base, 'jsonbit'), sc.promptJson)
    const rJson = runOpenCode(path.join(base, 'json'), sc.promptJson)
    const rToon = runOpenCode(path.join(base, 'toon'), sc.promptToon)

    const bitOk = checkExpected(rBit.out, sc.expectedTokens)
    const jsonOk = checkExpected(rJson.out, sc.expectedTokens)
    const toonOk = checkExpected(rToon.out, sc.expectedTokens)

    const bitMcp = rBit.out.includes('jsonbit_olap_')
    const bitScripts = rBit.out.includes('python3') || rBit.out.includes('awk ')
    const jsonScripts = rJson.out.includes('python3') || rJson.out.includes('$ ')
    const toonScripts = rToon.out.includes('python3') || rToon.out.includes('awk ') || rToon.out.includes('$ ')

    results.push({
      id: sc.id,
      name: sc.name,
      json: { sec: Number(rJson.elapsedSec.toFixed(2)), ok: jsonOk, scripts: jsonScripts },
      toon: { sec: Number(rToon.elapsedSec.toFixed(2)), ok: toonOk, scripts: toonScripts },
      jsonbit: {
        sec: Number(rBit.elapsedSec.toFixed(2)),
        ok: bitOk,
        mcpUsed: bitMcp,
        scripts: bitScripts,
      },
    })

    console.log(
      `${sc.id.padEnd(26)} | jsonbit: ${bitOk ? 'PASS' : 'FAIL'} (${rBit.elapsedSec.toFixed(1)}s, mcp=${bitMcp}) | JSON: ${jsonOk ? 'PASS' : 'FAIL'} (${rJson.elapsedSec.toFixed(1)}s) | TOON: ${toonOk ? 'PASS' : 'FAIL'} (${rToon.elapsedSec.toFixed(1)}s)`,
    )
  }

  const avg = (arr: number[]) => arr.reduce((a, b) => a + b, 0) / arr.length
  const sumOk = (arr: boolean[]) => arr.filter(Boolean).length

  const summary = {
    model: MODEL,
    scenarios: results,
    totals: {
      jsonbit: {
        accuracy: `${sumOk(results.map(r => r.jsonbit.ok))}/${results.length}`,
        avgSec: Number(avg(results.map(r => r.jsonbit.sec)).toFixed(2)),
        totalSec: Number(results.reduce((s, r) => s + r.jsonbit.sec, 0).toFixed(2)),
        mcpDiscovery: `${sumOk(results.map(r => r.jsonbit.mcpUsed))}/${results.length}`,
        bashScriptsNeeded: sumOk(results.map(r => r.jsonbit.scripts)),
      },
      json: {
        accuracy: `${sumOk(results.map(r => r.json.ok))}/${results.length}`,
        avgSec: Number(avg(results.map(r => r.json.sec)).toFixed(2)),
        totalSec: Number(results.reduce((s, r) => s + r.json.sec, 0).toFixed(2)),
      },
      toon: {
        accuracy: `${sumOk(results.map(r => r.toon.ok))}/${results.length}`,
        avgSec: Number(avg(results.map(r => r.toon.sec)).toFixed(2)),
        totalSec: Number(results.reduce((s, r) => s + r.toon.sec, 0).toFixed(2)),
      },
    },
  }

  fs.writeFileSync(
    path.resolve(process.cwd(), 'benchmarks/results/opencode-agent-benchmark.json'),
    JSON.stringify(summary, null, 2),
  )
  fs.rmSync(base, { recursive: true, force: true })

  console.log('\n=== OPENCODE 6-SCENARIO (18-RUN) SUMMARY ===')
  console.log(JSON.stringify(summary.totals, null, 2))
}

main()
