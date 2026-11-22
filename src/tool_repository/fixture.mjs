// Source input for the existing Node protocol fixtures. No helper code executes here.
import { execFileSync } from 'node:child_process';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createHash } from 'node:crypto';

let cached;
export function officialSources() {
  if (cached) return cached;
  const root = mkdtempSync(join(tmpdir(), 'ssmcp-tool-source-fixture-'));
  const bundle = fileURLToPath(new URL('./official.bundle', import.meta.url));
  const env = Object.fromEntries(Object.entries(process.env).filter(([key]) => !key.startsWith('GIT_')));
  Object.assign(env, {
    GIT_CONFIG_NOSYSTEM:'1',
    GIT_CONFIG_GLOBAL:process.platform === 'win32' ? 'NUL' : '/dev/null',
    GIT_TERMINAL_PROMPT:'0',
  });
  const git = args => execFileSync('git', [
    '-c', 'core.hooksPath=' + (process.platform === 'win32' ? 'NUL' : '/dev/null'),
    '-c', 'protocol.allow=never', '-c', 'protocol.file.allow=always', ...args,
  ], {cwd:root, env, encoding:'utf8', timeout:10000, maxBuffer:512 * 1024,
    stdio:['ignore','pipe','pipe']});
  try {
    git(['init','--bare','.']);
    git(['fetch','--no-tags','--no-recurse-submodules','--no-write-fetch-head',
      bundle,'refs/heads/main:refs/heads/fixture']);
    const head = git(['rev-parse','refs/heads/fixture']).trim();
    const manifest = JSON.parse(git(['show', head + ':manifest.json']));
    if (manifest.schema !== 'ssmcp-tools-v1' || manifest.entry !== 'tools.js') {
      throw new Error('unexpected official tool-source fixture manifest');
    }
    const code = git(['show', head + ':tools.js']);
    cached = Object.freeze([Object.freeze({
      name:'official_seed', code, sha256:createHash('sha256').update(code).digest('hex'),
    })]);
    return cached;
  } finally {
    rmSync(root, {recursive:true, force:true});
  }
}
