// Verifies the console's credential handling without a browser.
//
// `npm run build` type-checks the module but cannot exercise it, and this is
// exactly the code that only runs on the deployed console - the path where a
// mistake costs an operator a locked-out UI. Node runs the real module with
// minimal browser globals instead: typescript (already a devDependency) strips
// the types, and the globals below stand in for sessionStorage, window and
// history.
//
//   node scripts/check-auth.mjs

import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { dirname, join } from 'node:path'
import { createRequire } from 'node:module'

const require = createRequire(import.meta.url)
const ts = require('typescript')

const here = dirname(fileURLToPath(import.meta.url))
const source = readFileSync(join(here, '..', 'src', 'auth.ts'), 'utf8')
const js = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
}).outputText

let failures = 0
function check(name, got, want) {
  const ok = JSON.stringify(got) === JSON.stringify(want)
  if (!ok) failures++
  console.log(`${ok ? 'ok  ' : 'FAIL'} ${name}${ok ? '' : ` (got ${JSON.stringify(got)}, want ${JSON.stringify(want)})`}`)
}

/** Installs browser globals for one scenario and loads a fresh module copy. */
async function withBrowser({ hash = '', pathname = '/', search = '' } = {}) {
  const store = new Map()
  const replaced = []
  globalThis.sessionStorage = {
    getItem: (k) => (store.has(k) ? store.get(k) : null),
    setItem: (k, v) => store.set(k, String(v)),
    removeItem: (k) => store.delete(k),
  }
  globalThis.window = {
    location: { hash, pathname, search },
    history: {
      replaceState: (_s, _t, url) => replaced.push(url),
    },
  }
  const mod = await import(
    'data:text/javascript;base64,' + Buffer.from(js).toString('base64') +
    // A unique fragment per scenario defeats the module cache.
    '#' + Math.random().toString(36).slice(2)
  )
  return { mod, store, replaced }
}

// No fragment: nothing adopted, nothing stored.
{
  const { mod, store, replaced } = await withBrowser({})
  check('no fragment adopts nothing', mod.adoptFragmentToken(), false)
  check('no fragment stores nothing', store.size, 0)
  check('no fragment rewrites nothing', replaced, [])
}

// A fragment token is adopted, stored and stripped from the URL.
{
  const { mod, store, replaced } = await withBrowser({ hash: '#token=s3cret' })
  check('fragment token is adopted', mod.adoptFragmentToken(), true)
  check('fragment token is stored', store.get('customs.admin_token'), 's3cret')
  check('fragment is stripped from the address bar', replaced, ['/'])
  check('the stored token is then sent', mod.authHeaders(), { Authorization: 'Bearer s3cret' })
}

// Other fragments are preserved; only the credential key is removed.
{
  const { mod, replaced } = await withBrowser({ hash: '#token=s3cret&view=audit' })
  check('unrelated fragment keys survive', mod.adoptFragmentToken(), true)
  check('only the token key is stripped', replaced, ['/#view=audit'])
}

// An empty credential is not a credential.
{
  const { mod, store } = await withBrowser({ hash: '#token=' })
  check('an empty fragment token is ignored', mod.adoptFragmentToken(), false)
  check('an empty fragment token stores nothing', store.size, 0)
}

// Without a credential the console sends no Authorization header, which is what
// a loopback deployment wants.
{
  const { mod } = await withBrowser({})
  check('no credential means no header', mod.authHeaders(), {})
  mod.saveToken('abc')
  check('a saved credential is sent', mod.authHeaders(), { Authorization: 'Bearer abc' })
  mod.clearToken()
  check('forgetting the credential removes the header', mod.authHeaders(), {})
}

console.log(failures === 0 ? '\nconsole credential handling OK' : `\n${failures} check(s) failed`)
process.exit(failures === 0 ? 0 : 1)
