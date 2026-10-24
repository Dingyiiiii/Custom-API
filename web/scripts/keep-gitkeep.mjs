// Keeps web/dist/.gitkeep present after a build.
//
// vite.config.ts uses `emptyOutDir: true`, which wipes dist/ (including the
// .gitkeep that keeps the directory in a clean checkout). This postbuild step
// writes it back, so the invariant "dist/.gitkeep always exists" survives every
// `npm run build`.
import { existsSync, writeFileSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const here = dirname(fileURLToPath(import.meta.url))
const target = join(here, '..', 'dist', '.gitkeep')

const CONTENTS = `The built single-page app is written here by \`npm run build\` (vite outDir).
Files in this directory are served from disk by the gateway (customsd), which
looks for web/dist at runtime and falls back to a placeholder when it is absent;
CUSTOMS_WEB_DIR overrides the location. This placeholder keeps the directory
present so the gateway has a well-known path before the front end is built.
`

if (!existsSync(target)) {
  writeFileSync(target, CONTENTS, 'utf8')
  console.log('postbuild: wrote dist/.gitkeep')
}
