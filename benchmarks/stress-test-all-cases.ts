/**
 * Exhaustive 15-Dataset Adversarial & Unseen-Schema Stress Test for `jsonbit`
 *
 * Tests Codd 3NF lossless rehydration, `.jsonbit` binary snapshot round-trip,
 * `card`, `digest`, `sql`, and edge cases across 15 completely new datasets.
 */
import { spawnSync } from 'node:child_process'
import * as fs from 'node:fs'
import * as path from 'node:path'
import { isDeepStrictEqual } from 'node:util'

const BIN = './target/release/jsonbit'

interface StressCase {
  id: string
  format: 'json' | 'jsonl'
  data: unknown
  rawText?: string
  sqlChecks: Array<{
    sql: string
    verify: (rows: Array<Record<string, unknown>>) => boolean
    desc: string
  }>
}

function buildStressCases(): StressCase[] {
  const cases: StressCase[] = []

  // S1: Bare root array (not wrapped in an object)
  const s1 = Array.from({ length: 40 }, (_, i) => ({
    id: i + 1,
    region: ['us', 'eu', 'apac'][i % 3]!,
    active: i % 4 !== 0,
    score: (i * 17) % 100,
  }))
  cases.push({
    id: 'S1-bare-root-array',
    format: 'json',
    data: s1,
    sqlChecks: [
      {
        sql: `SELECT COUNT(*) AS c FROM records WHERE region = 'eu' AND active = true`,
        verify: r => r[0]?.c === s1.filter(x => x.region === 'eu' && x.active).length,
        desc: 'bare root array filtered count',
      },
    ],
  })

  // S2: 3-level deep nested arrays (departments -> teams -> members)
  const s2 = {
    departments: [
      {
        dept: 'Core',
        teams: [
          {
            team: 'Storage',
            members: [
              { name: 'Ada', level: 6 },
              { name: 'Grace', level: 7 },
            ],
          },
          {
            team: 'Query',
            members: [{ name: 'Ken', level: 6 }],
          },
        ],
      },
      {
        dept: 'AI',
        teams: [
          {
            team: 'Inference',
            members: [
              { name: 'Lin', level: 5 },
              { name: 'Sam', level: 6 },
            ],
          },
        ],
      },
    ],
  }
  cases.push({
    id: 'S2-3level-deep-hierarchy',
    format: 'json',
    data: s2,
    sqlChecks: [
      {
        sql: `SELECT COUNT(*) AS c FROM departments_teams_members WHERE level >= 6`,
        verify: r => r[0]?.c === 4,
        desc: '3-level nested child table query',
      },
    ],
  })

  // S3: Empty child arrays [], explicit nulls vs omitted keys, 0, false, ""
  const s3 = {
    orders: [
      { id: 'O1', note: null, discount: 0, vip: false, items: [] },
      { id: 'O2', discount: 15, vip: true, items: [{ sku: 'A', qty: 2 }] },
      { id: 'O3', note: '', discount: 0, vip: false, items: [{ sku: 'B', qty: 1 }] },
    ],
  }
  cases.push({
    id: 'S3-empty-arrays-and-nulls',
    format: 'json',
    data: s3,
    sqlChecks: [
      {
        sql: `SELECT COUNT(*) AS c FROM orders WHERE note IS NULL`,
        verify: r => r[0]?.c === 2,
        desc: 'null + omitted note count',
      },
      {
        sql: `SELECT COUNT(*) AS c FROM orders WHERE vip = false AND discount = 0`,
        verify: r => r[0]?.c === 2,
        desc: 'zero and false distinction',
      },
    ],
  })

  // S4: Polymorphic mixed types in same column
  const s4 = {
    readings: [
      { sensor: 'T1', val: 42 },
      { sensor: 'T2', val: 'CALIBRATING' },
      { sensor: 'T3', val: false },
      { sensor: 'T4', val: null },
      { sensor: 'T5', val: 98.6 },
    ],
  }
  cases.push({
    id: 'S4-polymorphic-mixed-types',
    format: 'json',
    data: s4,
    sqlChecks: [
      {
        sql: `SELECT COUNT(*) AS c FROM readings WHERE val IS NOT NULL`,
        verify: r => r[0]?.c === 4,
        desc: 'polymorphic non-null count',
      },
    ],
  })

  // S5: Negative & large 64-bit integers
  const s5 = {
    ledger: [
      { id: 1, delta: -500000000000 },
      { id: 2, delta: 0 },
      { id: 3, delta: 500000000000 },
      { id: 4, delta: -42 },
    ],
  }
  cases.push({
    id: 'S5-negative-and-64bit-ints',
    format: 'json',
    data: s5,
    sqlChecks: [
      {
        sql: `SELECT SUM(delta) AS s, MIN(delta) AS mn, MAX(delta) AS mx FROM ledger`,
        verify: r => r[0]?.s === -42 && r[0]?.mn === -500000000000 && r[0]?.mx === 500000000000,
        desc: '64-bit signed negative/positive FOR packing',
      },
    ],
  })

  // S6: Single-row array
  const s6 = { events: [{ id: 1, ok: true, msg: 'boot' }] }
  cases.push({
    id: 'S6-single-row-array',
    format: 'json',
    data: s6,
    sqlChecks: [
      {
        sql: `SELECT msg FROM events WHERE ok = true`,
        verify: r => r[0]?.msg === 'boot',
        desc: 'single-row array query',
      },
    ],
  })

  // S7: Special characters, SQL keywords in values, quotes, newlines, emojis
  const s7 = {
    comments: [
      { id: 1, author: "O'Reilly", body: 'SELECT * FROM users; DROP TABLE x;' },
      { id: 2, author: 'Alice', body: 'Line1\nLine2\tTabbed "quoted" 🔥' },
      { id: 3, author: 'Bob', body: 'Normal AND OR BETWEEN IN test' },
    ],
  }
  cases.push({
    id: 'S7-special-chars-and-sql-quotes',
    format: 'json',
    data: s7,
    sqlChecks: [
      {
        sql: `SELECT id FROM comments WHERE body LIKE '%DROP TABLE%'`,
        verify: r => r[0]?.id === 1,
        desc: 'SQL keywords inside string data',
      },
    ],
  })

  // S8: Dynamic keyed object map
  const s8 = {
    featureFlags: {
      dark_mode: { enabled: true, rollout: 100, owner: 'ui' },
      fast_checkout: { enabled: true, rollout: 50, owner: 'payments' },
      legacy_api: { enabled: false, rollout: 0, owner: 'platform' },
    },
  }
  cases.push({
    id: 'S8-keyed-feature-flags',
    format: 'json',
    data: s8,
    sqlChecks: [
      {
        sql: `SELECT COUNT(*) AS c FROM featureFlags WHERE enabled = true AND rollout >= 50`,
        verify: r => r[0]?.c === 2,
        desc: 'keyed map filtering',
      },
    ],
  })

  // S9: Complex mixed singleton + nested arrays (Kubernetes Deployment)
  const s9 = {
    apiVersion: 'apps/v1',
    kind: 'Deployment',
    metadata: { name: 'api-gateway', namespace: 'prod' },
    spec: {
      replicas: 6,
      containers: [
        { name: 'envoy', image: 'envoyproxy/envoy:v1.30', port: 8080 },
        { name: 'oauth-proxy', image: 'quay.io/oauth2-proxy:v7', port: 4180 },
      ],
    },
  }
  cases.push({
    id: 'S9-k8s-deployment-manifest',
    format: 'json',
    data: s9,
    sqlChecks: [
      {
        sql: `SELECT spec_replicas FROM config`,
        verify: r => r[0]?.spec_replicas === 6,
        desc: 'mixed singleton + child array query',
      },
    ],
  })

  // S10: 5,000-line JSONL with high-cardinality UUIDs
  const s10Rows = Array.from({ length: 5000 }, (_, i) => ({
    reqId: `req-${(i * 2654435761) >>> 0}-${i}`,
    region: ['us-east', 'us-west', 'eu-central', 'ap-south'][i % 4]!,
    status: i % 17 === 0 ? 502 : 200,
    ms: 10 + (i % 500),
    ...(i % 17 === 0 ? { err: { retryable: i % 34 === 0 } } : {}),
  }))
  const expected502 = s10Rows.filter(r => r.status === 502).length
  const expectedRetryable = s10Rows.filter(r => r.err?.retryable === true).length
  cases.push({
    id: 'S10-high-card-5000-jsonl',
    format: 'jsonl',
    data: { records: s10Rows },
    rawText: s10Rows.map(r => JSON.stringify(r)).join('\n'),
    sqlChecks: [
      {
        sql: `SELECT region, COUNT(*) AS c FROM records WHERE status = 502 GROUP BY region ORDER BY c DESC`,
        verify: r => r.length === 4 && r.reduce((s, x) => s + Number(x.c), 0) === expected502,
        desc: '5,000-line JSONL GROUP BY count',
      },
      {
        sql: `SELECT COUNT(*) AS c FROM records WHERE err.retryable = true`,
        verify: r => r[0]?.c === expectedRetryable,
        desc: '5,000-line sparse dot-path filter',
      },
    ],
  })

  // S11: Float precision
  const s11 = {
    measurements: [
      { id: 'M1', temp: -45.6789, ratio: 0.000123 },
      { id: 'M2', temp: 100.0, ratio: 0.0 },
      { id: 'M3', temp: 22.5, ratio: 1.5 },
    ],
  }
  cases.push({
    id: 'S11-float-precision',
    format: 'json',
    data: s11,
    sqlChecks: [
      {
        sql: `SELECT id FROM measurements WHERE temp < 0`,
        verify: r => r[0]?.id === 'M1',
        desc: 'negative float comparison',
      },
    ],
  })

  // S12: 500-row x 20-boolean BitVec64 matrix
  const s12 = {
    flags: Array.from({ length: 500 }, (_, i) => {
      const row: Record<string, unknown> = { id: i + 1 }
      for (let b = 0; b < 20; b++) {
        row[`b${b}`] = ((i + b) % 3) === 0
      }
      return row
    }),
  }
  cases.push({
    id: 'S12-boolean-only-matrix',
    format: 'json',
    data: s12,
    sqlChecks: [
      {
        sql: `SELECT COUNT(*) AS c FROM flags WHERE b0 = true AND b3 = true`,
        verify: r =>
          r[0]?.c === s12.flags.filter(f => f.b0 === true && f.b3 === true).length,
        desc: 'BitVec64 boolean intersection',
      },
    ],
  })

  // S13: 100% null column
  const s13 = {
    users: [
      { id: 1, name: 'A', deletedAt: null },
      { id: 2, name: 'B', deletedAt: null },
      { id: 3, name: 'C', deletedAt: null },
    ],
  }
  cases.push({
    id: 'S13-all-null-column',
    format: 'json',
    data: s13,
    sqlChecks: [
      {
        sql: `SELECT COUNT(*) AS c FROM users WHERE deletedAt IS NULL`,
        verify: r => r[0]?.c === 3,
        desc: '100% null column IS NULL check',
      },
    ],
  })

  // S14: Duplicate leaf key names under different nested paths
  const s14 = {
    events: [
      { id: 1, user: { id: 101, name: 'Alice' }, org: { name: 'Acme' } },
      { id: 2, user: { id: 102, name: 'Bob' }, org: { name: 'Globex' } },
    ],
  }
  cases.push({
    id: 'S14-duplicate-keys-diff-nesting',
    format: 'json',
    data: s14,
    sqlChecks: [
      {
        sql: `SELECT user.id AS uid, org.name AS oname FROM events WHERE user.name = 'Bob'`,
        verify: r => r[0]?.uid === 102 && r[0]?.oname === 'Globex',
        desc: 'disambiguated nested dot-paths',
      },
    ],
  })

  // S15: Healthcare FHIR-style Claims (1-to-Many procedures + nested patient)
  const s15 = {
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
  cases.push({
    id: 'S15-healthcare-fhir-claims',
    format: 'json',
    data: s15,
    sqlChecks: [
      {
        sql: `SELECT patient.state AS st, COUNT(*) AS cnt, SUM(billedUsd) AS tot FROM claims WHERE approved = true GROUP BY patient.state ORDER BY tot DESC`,
        verify: r =>
          r.length === 4 &&
          r.reduce((s, x) => s + Number(x.cnt), 0) === s15.claims.filter(c => c.approved).length,
        desc: 'healthcare claims dot-path GROUP BY + ORDER BY',
      },
      {
        sql: `SELECT COUNT(DISTINCT claimId) AS c FROM claims JOIN claims_procedures WHERE approved = true AND code = 'CPT-99213'`,
        verify: r =>
          r[0]?.c ===
          s15.claims.filter(c => c.approved && c.procedures.some(p => p.code === 'CPT-99213'))
            .length,
        desc: 'healthcare CSR JOIN',
      },
    ],
  })

  return cases
}

function main() {
  const cases = buildStressCases()
  const tmpDir = '/tmp/jsonbit-stress-15'
  fs.rmSync(tmpDir, { recursive: true, force: true })
  fs.mkdirSync(tmpDir, { recursive: true })

  let failures = 0

  for (const tc of cases) {
    const ext = tc.format === 'jsonl' ? 'jsonl' : 'json'
    const fp = path.join(tmpDir, `${tc.id}.${ext}`)
    const snapFp = path.join(tmpDir, `${tc.id}.jsonbit`)
    fs.writeFileSync(fp, tc.rawText ?? JSON.stringify(tc.data, null, 2), 'utf8')

    // 1. card + digest
    const cRes = spawnSync(BIN, ['card', fp], { encoding: 'utf8' })
    const dRes = spawnSync(BIN, ['digest', fp], { encoding: 'utf8' })
    if (cRes.status !== 0 || dRes.status !== 0) {
      console.error(`FAIL [${tc.id}] card/digest exited non-zero`)
      failures++
      continue
    }

    // 2. pack -> .jsonbit -> rehydrate
    const pRes = spawnSync(BIN, ['pack', fp, snapFp], { encoding: 'utf8' })
    if (pRes.status !== 0) {
      console.error(`FAIL [${tc.id}] pack failed: ${pRes.stderr}`)
      failures++
      continue
    }
    const rRes = spawnSync(BIN, ['rehydrate', snapFp], { maxBuffer: 50 * 1024 * 1024, encoding: 'utf8' })
    if (rRes.status !== 0) {
      console.error(`FAIL [${tc.id}] rehydrate failed: ${rRes.stderr}`)
      failures++
      continue
    }
    const rehydrated = JSON.parse(rRes.stdout)
    const lossless = isDeepStrictEqual(rehydrated, tc.data)
    if (!lossless) {
      console.error(`FAIL [${tc.id}] Lossless 3NF / .jsonbit round-trip mismatch!`)
      console.error('  Orig sample:', JSON.stringify(tc.data).slice(0, 220))
      console.error('  Rehy sample:', JSON.stringify(rehydrated).slice(0, 220))
      failures++
    }

    // 3. SQL checks on the packed .jsonbit binary file
    let sqlOk = true
    for (const sq of tc.sqlChecks) {
      const sRes = spawnSync(BIN, ['sql', snapFp, sq.sql], { encoding: 'utf8' })
      if (sRes.status !== 0) {
        console.error(`FAIL [${tc.id}] SQL (${sq.desc}) error: ${sRes.stderr}`)
        sqlOk = false
        failures++
      } else {
        const rows = JSON.parse(sRes.stdout)
        if (!sq.verify(rows)) {
          console.error(`FAIL [${tc.id}] SQL (${sq.desc}) wrong result: ${sRes.stdout}`)
          sqlOk = false
          failures++
        }
      }
    }

    console.log(
      `${lossless && sqlOk ? 'PASS' : 'FAIL'} | ${tc.id.padEnd(32)} | Lossless .jsonbit: ${lossless ? '100%' : 'DIFF'} | SQL Checks: ${sqlOk ? tc.sqlChecks.length + '/' + tc.sqlChecks.length : 'FAIL'}`,
    )
  }

  fs.rmSync(tmpDir, { recursive: true, force: true })
  console.log(`\nSUMMARY: ${cases.length - failures}/${cases.length} adversarial datasets passed completely.`)
  if (failures > 0) process.exit(1)
}

main()
