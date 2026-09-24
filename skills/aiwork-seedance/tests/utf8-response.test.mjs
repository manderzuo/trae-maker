import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createServer } from 'node:http';
import { once } from 'node:events';
import { fileURLToPath } from 'node:url';
import path from 'node:path';
import test from 'node:test';

const runner = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../scripts/aiwork-seedance.ps1');

test('status preserves UTF-8 task text when JSON response has no charset', async () => {
  const prompt = '你好，笔记本电脑';
  const server = createServer((request, response) => {
    assert.equal(request.url, '/v1/videos/video-test');
    response.writeHead(200, { 'Content-Type': 'application/json' });
    response.end(JSON.stringify({ task: { id: 'video-test', status: 'completed', prompt } }));
  });
  server.listen(0, '127.0.0.1');
  await once(server, 'listening');

  try {
    const port = server.address().port;
    const child = spawn('powershell.exe', [
      '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', runner, 'status', '-TaskId', 'video-test',
      '-GatewayBaseUrl', `http://127.0.0.1:${port}/v1`, '-ApiKey', 'test-key',
    ], { stdio: ['ignore', 'pipe', 'pipe'] });
    let stdout = '';
    let stderr = '';
    child.stdout.setEncoding('utf8').on('data', chunk => { stdout += chunk; });
    child.stderr.setEncoding('utf8').on('data', chunk => { stderr += chunk; });
    const [exitCode] = await once(child, 'close');
    assert.equal(exitCode, 0, stderr);
    assert.equal(JSON.parse(stdout).prompt, prompt);
  } finally {
    server.close();
  }
});
