import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const skillDir = path.join(root, 'skills', 'aiwork-seedance');
const skillFile = path.join(skillDir, 'SKILL.md');
const runFile = path.join(skillDir, 'scripts', 'aiwork-seedance.ps1');
const installFile = path.join(skillDir, 'scripts', 'install.ps1');
const cmdFile = path.join(skillDir, 'install.cmd');

function mustExist(file) {
  assert.equal(fs.existsSync(file), true, `missing skill artifact: ${path.relative(root, file)}`);
}

mustExist(skillFile);
mustExist(runFile);
mustExist(installFile);
mustExist(cmdFile);

const skill = fs.readFileSync(skillFile, 'utf8');
assert.match(skill, /^---\r?\nname:\s*aiwork-seedance\r?\ndescription:\s*Use when/m);
for (const term of ['seedance_submit', 'seedance_status', 'seedance_download', 'image_paths', 'task_id']) {
  assert.match(skill, new RegExp(term), `SKILL.md must describe ${term}`);
}

for (const script of [runFile, installFile]) {
  const result = spawnSync('powershell.exe', [
    '-NoProfile', '-NonInteractive', '-Command',
    `[scriptblock]::Create((Get-Content -LiteralPath '${script.replaceAll("'", "''")}' -Encoding UTF8 -Raw)) | Out-Null`,
  ], { encoding: 'utf8' });
  assert.equal(result.status, 0, `${path.basename(script)} has PowerShell syntax errors:\n${result.stderr}`);
}

const runner = fs.readFileSync(runFile, 'utf8');
assert.match(runner, /AIWORK_GATEWAY_BASE_URL/);
assert.match(runner, /AIWORK_API_KEY/);
assert.match(runner, /Idempotency-Key/);
assert.match(runner, /\/videos\/\$Id/);
assert.match(runner, /\/assets/);
assert.match(runner, /\.part/);

console.log('aiwork skill package smoke test: passed');
