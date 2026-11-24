#!/usr/bin/env node
// Bumps the version everywhere, commits, tags and pushes. Usage: npm run release <x.y.z|patch|minor|major> [-- --dry-run]
import { execSync } from 'node:child_process'
import { readFileSync, writeFileSync } from 'node:fs'

const [arg, ...flags] = process.argv.slice(2), dry = flags.includes('--dry-run')
const sh = (c) => execSync(c, { encoding: 'utf8' }).trim(), run = (c) => dry ? console.log(`[dry-run] ${c}`) : execSync(c, { stdio: 'inherit' })
const die = (m) => { console.error(`release: ${m}`); process.exit(1) }

const cur = readFileSync('Cargo.toml', 'utf8').match(/^version = "(.*)"$/m)?.[1] ?? die('no version in Cargo.toml'), [M, m, p] = cur.split(/[.-]/).map(Number)
const next = { major: `${M + 1}.0.0`, minor: `${M}.${m + 1}.0`, patch: `${M}.${m}.${p + 1}` }[arg] ?? arg
if (!/^\d+\.\d+\.\d+(-[0-9A-Za-z.-]+)?$/.test(next ?? '')) die('give a version (1.2.3, 1.2.3-beta.1) or patch|minor|major')
// Semver order: numbers first, then a release outranks its pre-releases (1.1.0 > 1.1.0-beta.2 > 1.1.0-beta.1)
const cmp = (a, b) => { const [x, xp = ''] = a.split(/-(.*)/), [y, yp = ''] = b.split(/-(.*)/), d = x.split('.').map((n, i) => n - y.split('.')[i]).find((n) => n); return d || (xp === yp ? 0 : !xp ? 1 : !yp ? -1 : xp.localeCompare(yp, 'en', { numeric: true })) }
if (!(cmp(next, cur) > 0)) die(`${next} is not newer than ${cur} — the updater only moves forward`)
if (sh('git branch --show-current') !== 'main') die('switch to main first')
if (sh('git status --porcelain')) die('commit or stash your changes first')
if (sh(`git tag -l v${next}`)) die(`tag v${next} already exists`)
if (readFileSync('desktop/tauri.conf.json', 'utf8').includes('REPLACE_WITH_YOUR_UPDATER_PUBLIC_KEY')) die('set the updater public key in desktop/tauri.conf.json first (README: Release)')

const edit = (f, fn) => { const s = readFileSync(f, 'utf8'), o = fn(s); if (o === s) die(`could not update ${f}`); if (!dry) writeFileSync(f, o) }
const lock = (name) => (s) => s.replace(new RegExp(`(name = "${name}"\\r?\\nversion = )".*"`), `$1"${next}"`)
edit('Cargo.toml', (s) => s.replace(/^version = ".*"$/m, `version = "${next}"`))
edit('Cargo.lock', lock('finch'))
edit('desktop/Cargo.toml', (s) => s.replace(/^version = ".*"$/m, `version = "${next}"`))
edit('desktop/Cargo.lock', (s) => lock('finch')(lock('finch-desktop')(s)))
edit('web/package.json', (s) => s.replace(`"version": "${cur}"`, `"version": "${next}"`))

console.log(`${cur} → ${next}${dry ? ' (dry run, nothing written)' : ''}`)
run('git add Cargo.toml Cargo.lock desktop/Cargo.toml desktop/Cargo.lock web/package.json')
run(`git commit -m "Release v${next}"`)
run(`git tag v${next}`)
run(`git push origin main v${next}`)
console.log(dry ? 'Dry run done.' : `Pushed v${next}. Watch the build under Actions, then publish the draft release.`)
