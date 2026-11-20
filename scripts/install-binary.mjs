#!/usr/bin/env node
/**
 * Downloads the pre-built native `jsonbit` binary from GitHub Releases into
 * `vendor/<platform>-<arch>/jsonbit` on `npm install` when no local build exists.
 */
import { execFileSync } from 'node:child_process'
import fs from 'node:fs'
import https from 'node:https'
import os from 'node:os'
import path from 'node:path'
import { fileURLToPath } from 'node:url'

const __dirname = path.dirname(fileURLToPath(import.meta.url))
const pkgRoot = path.resolve(__dirname, '..')
const pkg = JSON.parse(fs.readFileSync(path.join(pkgRoot, 'package.json'), 'utf8'))

const platform = process.platform
const arch = process.arch
const exe = platform === 'win32' ? 'jsonbit.exe' : 'jsonbit'

const localTarget = path.join(pkgRoot, 'target', 'release', exe)
const vendorDir = path.join(pkgRoot, 'vendor', `${platform}-${arch}`)
const vendorBin = path.join(vendorDir, exe)

if (process.env.JSONBIT_SKIP_DOWNLOAD === '1' || fs.existsSync(localTarget) || fs.existsSync(vendorBin)) {
  process.exit(0)
}

const SUPPORTED = new Set(['linux-x64', 'linux-arm64', 'darwin-arm64', 'darwin-x64', 'win32-x64'])
const key = `${platform}-${arch}`
if (!SUPPORTED.has(key)) {
  console.warn(`[jsonbit] No pre-built binary for ${key}. Install via \`cargo install jsonbit\`.`)
  process.exit(0)
}

const ext = platform === 'win32' ? 'zip' : 'tar.gz'
const url = `https://github.com/prathikmurugan/jsonbit/releases/download/v${pkg.version}/jsonbit-${key}.${ext}`

function download(targetUrl, destPath, redirects = 0) {
  return new Promise((resolve, reject) => {
    if (redirects > 5) return reject(new Error('Too many redirects'))
    https
      .get(targetUrl, { headers: { 'User-Agent': `jsonbit/${pkg.version}` } }, res => {
        if (res.statusCode && res.statusCode >= 300 && res.statusCode < 400 && res.headers.location) {
          res.resume()
          return resolve(download(res.headers.location, destPath, redirects + 1))
        }
        if (res.statusCode !== 200) {
          res.resume()
          return reject(new Error(`HTTP ${res.statusCode} fetching ${targetUrl}`))
        }
        const file = fs.createWriteStream(destPath)
        res.pipe(file)
        file.on('finish', () => file.close(() => resolve()))
        file.on('error', reject)
      })
      .on('error', reject)
  })
}

async function main() {
  const tmpArchive = path.join(os.tmpdir(), `jsonbit-${pkg.version}-${key}.${ext}`)
  try {
    await download(url, tmpArchive)
    fs.mkdirSync(vendorDir, { recursive: true })
    if (platform === 'win32') {
      execFileSync('powershell', [
        '-NoProfile',
        '-Command',
        `Expand-Archive -Force -Path '${tmpArchive}' -DestinationPath '${vendorDir}'`,
      ])
    } else {
      execFileSync('tar', ['-xzf', tmpArchive, '-C', vendorDir])
      fs.chmodSync(vendorBin, 0o755)
    }
  } catch (err) {
    console.warn(
      `[jsonbit] Could not download pre-built binary (${err.message}).\n` +
        `Build locally with \`cargo build --release\` or install via \`cargo install jsonbit\`.`,
    )
  } finally {
    if (fs.existsSync(tmpArchive)) fs.unlinkSync(tmpArchive)
  }
}

main()
