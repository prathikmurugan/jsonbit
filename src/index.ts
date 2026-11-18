/**
 * @jsonbit/core — TypeScript / Node.js SDK powered 100% by the Pure-Rust `jsonbit` engine.
 * Zero SQLite, zero external runtime dependencies.
 */
import { spawnSync } from 'node:child_process'
import fs from 'node:fs'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'
import type { JsonValue } from './types.ts'

export type {
  CategoricalColumnSummary,
  ColumnDef,
  ForeignKeyDef,
  JsonArray,
  JsonObject,
  JsonPrimitive,
  JsonValue,
  NumericColumnSummary,
  OlapDataCard,
  SqlColumnType,
  SqlSliceResult,
  TableDef,
  TableOlapSummary,
} from './types.ts'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const REPO_ROOT = path.resolve(__dirname, '..')

export function resolveJsonBitBinary(): string {
  const exe = process.platform === 'win32' ? 'jsonbit.exe' : 'jsonbit'
  const candidates = [
    process.env.JSONBIT_BIN,
    path.join(REPO_ROOT, 'vendor', `${process.platform}-${process.arch}`, exe),
    path.join(REPO_ROOT, 'target', 'release', exe),
  ].filter((x): x is string => Boolean(x))

  for (const c of candidates) {
    if (fs.existsSync(c)) return c
  }
  return exe
}

export class JsonBit {
  private readonly filePath: string
  private tempFile: string | null = null
  private readonly binPath: string

  private constructor(filePath: string, tempFile: string | null, binPath?: string) {
    this.filePath = filePath
    this.tempFile = tempFile
    this.binPath = binPath ?? resolveJsonBitBinary()
  }

  /** Creates a `JsonBit` handle from an in-memory JSON value or a `.json`/`.jsonl`/`.jsonbit` file path. */
  static from(dataOrPath: unknown, binPath?: string): JsonBit {
    if (typeof dataOrPath === 'string' && fs.existsSync(dataOrPath)) {
      return new JsonBit(dataOrPath, null, binPath)
    }
    const tmp = path.join(
      os.tmpdir(),
      `jsonbit_${process.pid}_${Date.now()}_${Math.random().toString(36).slice(2)}.json`,
    )
    const raw = typeof dataOrPath === 'string' ? dataOrPath : JSON.stringify(dataOrPath)
    fs.writeFileSync(tmp, raw, 'utf8')
    return new JsonBit(tmp, tmp, binPath)
  }

  private run(args: string[]): string {
    const res = spawnSync(this.binPath, args, {
      maxBuffer: 100 * 1024 * 1024,
      encoding: 'utf8',
    })
    if (res.status !== 0) {
      throw new Error((res.stderr || res.stdout || `jsonbit exited with ${res.status}`).trim())
    }
    return res.stdout.trim()
  }

  /** Returns the ~250-token O(1) Semantic Cube Card. */
  getDataCard(): { text: string } {
    return { text: this.run(['card', this.filePath]) }
  }

  /** Returns the 1-Turn Factorized 3NF Holistic Digest (100% of rows + pivot legend). */
  digest(): string {
    return this.run(['digest', this.filePath])
  }

  /** Evaluates a natural-language question on BitVec64 indices and returns the verified slice context. */
  slice(question: string): { context: string } {
    return { context: this.run(['ask', this.filePath, question]) }
  }

  /** Evaluates a natural-language question and returns only the scalar answer string. */
  answer(question: string): string {
    return this.run(['answer', this.filePath, question])
  }

  /** Executes a read-only SQL SELECT query directly against the Pure-Rust Bit-Packed store. */
  sql(query: string): Array<Record<string, unknown>> {
    return JSON.parse(this.run(['sql', this.filePath, query])) as Array<Record<string, unknown>>
  }

  /** Serializes the bit-packed 3NF tables into a compact `.jsonbit` binary snapshot on disk. */
  pack(outPath: string): string {
    return this.run(['pack', this.filePath, outPath])
  }

  /** Reconstructs the original nested JSON document losslessly from the 3NF bit-packed store. */
  rehydrate(): JsonValue {
    return JSON.parse(this.run(['rehydrate', this.filePath])) as JsonValue
  }

  close(): void {
    if (this.tempFile && fs.existsSync(this.tempFile)) {
      fs.unlinkSync(this.tempFile)
      this.tempFile = null
    }
  }
}

export const JsonOlap = JsonBit

export function rehydrateFrom3NF(engine: JsonBit): JsonValue {
  return engine.rehydrate()
}
