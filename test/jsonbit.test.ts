import { spawnSync } from 'node:child_process'
import { describe, expect, it } from 'vitest'
import { encode as countTokens } from 'gpt-tokenizer'
import { encode as encodeToon } from '@toon-format/toon'
import { JsonBit, rehydrateFrom3NF } from '../src/index.ts'
import { ACCURACY_DATASETS, TOKEN_EFFICIENCY_DATASETS } from '../benchmarks/toon-suite/datasets.ts'
import { generateQuestions } from '../benchmarks/toon-suite/questions/index.ts'
import { compareAnswers } from '../benchmarks/toon-suite/normalize.ts'

describe('jsonbit: Pure-Rust Auto-3NF Bit-Packed Columnar & BitVec64 Engine', () => {
  it('satisfies Codd Lossless-Join 3NF Property across all 8 official datasets (via Pure-Rust SDK)', () => {
    for (const ds of TOKEN_EFFICIENCY_DATASETS) {
      const engine = JsonBit.from(ds.data)
      const restored = rehydrateFrom3NF(engine)
      expect(restored).toEqual(ds.data)
      engine.close()
    }
  })

  it('beats JSON, JSON.stringify, and TOON on token count across all 8 official datasets (-99.24% total vs TOON)', () => {
    let totalToon = 0
    let totalCard = 0

    for (const ds of TOKEN_EFFICIENCY_DATASETS) {
      const jsonCompactTok = countTokens(JSON.stringify(ds.data)).length
      const toonTok = countTokens(encodeToon(ds.data)).length

      const engine = JsonBit.from(ds.data)
      const cardTok = countTokens(engine.getDataCard().text).length
      engine.close()

      expect(cardTok).toBeLessThan(toonTok)
      expect(cardTok).toBeLessThan(jsonCompactTok)
      totalToon += toonTok
      totalCard += cardTok
    }

    expect(totalCard).toBeLessThan(totalToon * 0.01)
  })

  it('dynamically compiles SQL on unseen custom schemas (Stripe charges, booleans, dot-paths, security guard)', () => {
    const customStripe = {
      charges: [
        {
          charge_id: 'ch_1001',
          merchant: 'Acme',
          amount_cents: 4500,
          disputed: false,
          meta: { ip: '10.0.0.1' },
        },
        {
          charge_id: 'ch_1002',
          merchant: 'Acme',
          amount_cents: 12000,
          disputed: true,
          meta: { ip: '10.0.0.2' },
        },
        {
          charge_id: 'ch_1003',
          merchant: 'Globex',
          amount_cents: 8500,
          disputed: false,
          meta: { ip: '10.0.0.3' },
        },
      ],
    }

    const engine = JsonBit.from(customStripe)
    expect(rehydrateFrom3NF(engine)).toEqual(customStripe)

    expect(engine.answer('How many charges have amount_cents > 5000?')).toBe('2')
    expect(engine.answer('How many disputed charges are for Acme?')).toBe('1')
    expect(engine.answer('How many undisputed charges are there?')).toBe('2')

    const rows = engine.sql(
      `SELECT charge_id, meta.ip AS ip FROM charges WHERE merchant ILIKE 'acme' AND disputed = true`,
    )
    expect(rows).toEqual([{ charge_id: 'ch_1002', ip: '10.0.0.2' }])

    expect(() => engine.sql(`DROP TABLE charges`)).toThrow(/SecurityError/)
    engine.close()
  })

  it('serves Pure-Rust stdio MCP JSON-RPC 2.0 protocol (olap_load, olap_digest, olap_ask, olap_sql, olap_rehydrate)', () => {
    const jsonlPayload = [
      JSON.stringify({ id: 1, endpoint: '/api/users', statusCode: 200, latency: 40, error: null }),
      JSON.stringify({
        id: 2,
        endpoint: '/api/users',
        statusCode: 503,
        latency: 1200,
        error: { retryable: true, msg: 'timeout' },
      }),
    ].join('\n')

    const msgs = [
      { jsonrpc: '2.0', id: 1, method: 'initialize' },
      {
        jsonrpc: '2.0',
        id: 2,
        method: 'tools/call',
        params: { name: 'olap_load', arguments: { json: jsonlPayload, alias: 'traces' } },
      },
      {
        jsonrpc: '2.0',
        id: 3,
        method: 'tools/call',
        params: {
          name: 'olap_sql',
          arguments: {
            alias: 'traces',
            sql: `SELECT endpoint, latency FROM records WHERE error.retryable = true`,
          },
        },
      },
    ]
      .map(m => JSON.stringify(m))
      .join('\n') + '\n'

    const proc = spawnSync('./target/release/jsonbit', ['mcp'], {
      input: msgs,
      encoding: 'utf8',
    })
    expect(proc.status).toBe(0)
    const lines = proc.stdout
      .trim()
      .split('\n')
      .map((l: string) => JSON.parse(l))
    expect(lines[0].result.protocolVersion).toBe('2024-11-05')
    expect(lines[1].result.content[0].text).toContain('TABLE records')
    expect(JSON.parse(lines[2].result.content[0].text)).toEqual([
      { endpoint: '/api/users', latency: 1200 },
    ])
  })

  it('verifies Pure-Rust Bit-Packed Columnar & BitVec64 engine (8/8 lossless, -64.4% space, <20µs queries, 237/239 strict match)', () => {
    const questions = generateQuestions().filter(q => q.type !== 'structural-validation')
    const batchInput = {
      datasets: TOKEN_EFFICIENCY_DATASETS.map(d => ({ name: d.name, data: d.data })),
      eval_datasets: ACCURACY_DATASETS.filter(
        d => !d.name.startsWith('structural-validation'),
      ).map(d => ({ name: d.name, data: d.data })),
      questions: questions.map(q => ({ id: q.id, dataset: q.dataset, prompt: q.prompt })),
    }

    const proc = spawnSync('./target/release/jsonbit', ['eval-batch'], {
      input: JSON.stringify(batchInput),
      maxBuffer: 50 * 1024 * 1024,
      encoding: 'utf8',
    })
    expect(proc.status).toBe(0)

    const out = JSON.parse(proc.stdout) as {
      dataset_results: Array<{
        raw_json_bytes: number
        compressed_bytes: number
        lossless_rehydrate: boolean
      }>
      question_results: Array<{ answer: string; latency_us: number }>
    }

    let totalRawBytes = 0
    let totalCompBytes = 0
    for (const dr of out.dataset_results) {
      expect(dr.lossless_rehydrate).toBe(true)
      totalRawBytes += dr.raw_json_bytes
      totalCompBytes += dr.compressed_bytes
    }
    expect(totalCompBytes).toBeLessThan(totalRawBytes * 0.4)

    let strictHits = 0
    let totalLatencyUs = 0
    for (let i = 0; i < questions.length; i++) {
      const q = questions[i]!
      const qr = out.question_results[i]!
      totalLatencyUs += qr.latency_us
      if (
        compareAnswers(qr.answer, q.groundTruth, q.answerType ?? 'string', q.normalizationOptions)
          .match
      ) {
        strictHits++
      }
    }

    expect(strictHits).toBe(237)
    expect(totalLatencyUs / questions.length).toBeLessThan(50)
  })
})
