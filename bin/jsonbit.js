#!/usr/bin/env node
/**
 * Native Pure-Rust `jsonbit` CLI & MCP launcher for npm / npx.
 * Resolves the compiled `jsonbit` binary from:
 *  1. `JSONBIT_BIN` environment variable
 *  2. Local build (`./target/release/jsonbit` or `vendor/<platform>/jsonbit`)
 *  3. Global `jsonbit` on `PATH`
 */
import { spawnSync } from 'node:child_process'
import fs from 'node:fs'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const pkgRoot = path.resolve(__dirname, '..')
const exe = process.platform === 'win32' ? 'jsonbit.exe' : 'jsonbit'

const candidates = [
  process.env.JSONBIT_BIN,
  path.join(pkgRoot, 'vendor', `${process.platform}-${process.arch}`, exe),
  path.join(pkgRoot, 'target', 'release', exe),
  exe,
].filter(Boolean)

let resolvedBin = exe
for (const c of candidates) {
  if (c === exe || fs.existsSync(c)) {
    resolvedBin = c
    break
  }
}

const res = spawnSync(resolvedBin, process.argv.slice(2), {
  stdio: 'inherit',
})

if (res.error) {
  console.error(
    `[jsonbit] Could not execute native binary (${resolvedBin}).\n` +
      `Build from source with \`cargo build --release\` or install via \`cargo install jsonbit\`.`,
  )
  process.exit(1)
}

process.exit(res.status ?? 0)
