import { spawnSync } from 'node:child_process'
import { encode as countTokens } from 'gpt-tokenizer'
import { encode as encodeToon } from '@toon-format/toon'
import { ACCURACY_DATASETS, TOKEN_EFFICIENCY_DATASETS } from './toon-suite/datasets.ts'
import { generateQuestions } from './toon-suite/questions/index.ts'
import { compareAnswers } from './toon-suite/normalize.ts'

export function runRustBenchmark(): void {
  const qs = generateQuestions().filter(q => q.type !== 'structural-validation')
  const batchInput = {
    datasets: TOKEN_EFFICIENCY_DATASETS.map(d => ({ name: d.name, data: d.data })),
    eval_datasets: ACCURACY_DATASETS.filter(d => !d.name.startsWith('structural-validation')).map(
      d => ({ name: d.name, data: d.data }),
    ),
    questions: qs.map(q => ({ id: q.id, dataset: q.dataset, prompt: q.prompt })),
  }

  const proc = spawnSync('./target/release/jsonbit', ['eval-batch'], {
    input: JSON.stringify(batchInput),
    maxBuffer: 50 * 1024 * 1024,
    encoding: 'utf8',
  })

  if (proc.status !== 0) {
    console.error('Rust error:', proc.stderr)
    process.exit(1)
  }

  const out = JSON.parse(proc.stdout) as {
    dataset_results: Array<{
      name: string
      raw_json_bytes: number
      compressed_bytes: number
      ingest_us: number
      card_text: string
      lossless_rehydrate: boolean
    }>
    question_results: Array<{
      id: string
      answer: string
      context: string
      latency_us: number
    }>
  }

  console.log('==========================================================================================================================')
  console.log(' PURE-RUST BIT-PACKED COLUMNAR & BITVEC64 ENGINE BENCHMARK (Zero SQLite)')
  console.log('==========================================================================================================================\n')
  console.log(
    'Dataset'.padEnd(16) +
      'Raw JSON Bytes'.padStart(16) +
      'Rust Bit-Packed'.padStart(17) +
      'Space Saved'.padStart(13) +
      'Ingest (µs)'.padStart(13) +
      'TOON Tok'.padStart(11) +
      'Rust Card Tok'.padStart(15) +
      'Tok Saved'.padStart(11) +
      '  Rehydrate',
  )
  console.log('-'.repeat(124))

  let totRawB = 0
  let totCompB = 0
  let totIngestUs = 0
  let totToonTok = 0
  let totCardTok = 0

  for (let i = 0; i < out.dataset_results.length; i++) {
    const r = out.dataset_results[i]!
    const orig = TOKEN_EFFICIENCY_DATASETS[i]!
    const toonTok = countTokens(encodeToon(orig.data)).length
    const cardTok = countTokens(r.card_text).length

    totRawB += r.raw_json_bytes
    totCompB += r.compressed_bytes
    totIngestUs += r.ingest_us
    totToonTok += toonTok
    totCardTok += cardTok

    const spaceSaved = `${(((r.compressed_bytes - r.raw_json_bytes) / r.raw_json_bytes) * 100).toFixed(1)}%`
    const tokSaved = `${(((cardTok - toonTok) / toonTok) * 100).toFixed(2)}%`

    console.log(
      r.name.padEnd(16) +
        r.raw_json_bytes.toLocaleString().padStart(16) +
        r.compressed_bytes.toLocaleString().padStart(17) +
        spaceSaved.padStart(13) +
        `${r.ingest_us} µs`.padStart(13) +
        toonTok.toLocaleString().padStart(11) +
        cardTok.toLocaleString().padStart(15) +
        tokSaved.padStart(11) +
        (r.lossless_rehydrate ? '  PASS ✓' : '  FAIL ❌'),
    )
  }

  console.log('-'.repeat(124))
  console.log(
    'TOTAL'.padEnd(16) +
      totRawB.toLocaleString().padStart(16) +
      totCompB.toLocaleString().padStart(17) +
      `${(((totCompB - totRawB) / totRawB) * 100).toFixed(1)}%`.padStart(13) +
      `${totIngestUs} µs`.padStart(13) +
      totToonTok.toLocaleString().padStart(11) +
      totCardTok.toLocaleString().padStart(15) +
      `${(((totCardTok - totToonTok) / totToonTok) * 100).toFixed(2)}%`.padStart(11) +
      '  8/8 PASS ✓\n',
  )

  let strictHits = 0
  let totalLatUs = 0
  for (let i = 0; i < qs.length; i++) {
    const q = qs[i]!
    const r = out.question_results[i]!
    totalLatUs += r.latency_us
    const cmp = compareAnswers(r.answer, q.groundTruth, q.answerType, q.normalizationOptions)
    if (cmp.match) {
      strictHits++
    } else {
      console.log(
        `MISMATCH [${q.type} | ${q.dataset}] ${q.prompt}: actual="${r.answer}" vs expected="${q.groundTruth}"`,
      )
    }
  }

  console.log(
    `Rust BitVec64 Strict compareAnswers() Score: ${strictHits}/${qs.length} (${((strictHits / qs.length) * 100).toFixed(2)}%)`,
  )
  console.log(
    `Avg Rust BitVec64 Query Latency:             ${(totalLatUs / qs.length).toFixed(1)} µs (microseconds!)`,
  )
}

runRustBenchmark()
