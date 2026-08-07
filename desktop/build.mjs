import { execFileSync } from 'node:child_process';
import { cp, mkdir, rm } from 'node:fs/promises';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = dirname(fileURLToPath(import.meta.url));
const output = join(root, 'dist');

await rm(output, { force: true, recursive: true });
await mkdir(output, { recursive: true });
await Promise.all([
  cp(join(root, 'ui', 'index.html'), join(output, 'index.html')),
  cp(join(root, 'ui', 'app.css'), join(output, 'app.css'))
]);
execFileSync('esbuild', [
  join(root, 'ui', 'app.js'),
  '--bundle',
  '--format=iife',
  '--minify',
  `--outfile=${join(output, 'app.js')}`,
  '--target=chrome100,safari15'
], { stdio: 'inherit' });
