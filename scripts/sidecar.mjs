#!/usr/bin/env node
// Builds the finch command line (release, SQLCipher + keychain) and copies it to desktop/binaries/finch-<target triple>, where the
// desktop bundle picks it up. Usage: node scripts/sidecar.mjs [--target <triple>|universal-apple-darwin]; FINCH_FEATURES overrides the features (local tries)
import { execSync } from 'node:child_process'
import { copyFileSync, mkdirSync } from 'node:fs'

const sh = (c) => execSync(c, { stdio: 'inherit' }), i = process.argv.indexOf('--target'), want = i > 0 ? process.argv[i + 1] : null
const host = execSync('rustc -vV', { encoding: 'utf8' }).match(/^host: (.*)$/m)[1], ext = (t) => t.includes('windows') ? '.exe' : ''
const build = (t) => { sh(`cargo build --release --locked --features ${process.env.FINCH_FEATURES ?? 'sqlcipher,keychain'}${t ? ` --target ${t}` : ''}`); return `target/${t ? `${t}/` : ''}release/finch${ext(t ?? host)}` }
mkdirSync('desktop/binaries', { recursive: true })
if (want === 'universal-apple-darwin') sh(`lipo -create -output desktop/binaries/finch-universal-apple-darwin ${build('aarch64-apple-darwin')} ${build('x86_64-apple-darwin')}`)
else { const t = want ?? host; copyFileSync(build(want), `desktop/binaries/finch-${t}${ext(t)}`) }
console.log('sidecar ready in desktop/binaries')
