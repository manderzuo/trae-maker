import assert from 'node:assert/strict';
import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const runner = path.join(root, 'skills', 'aiwork-seedance', 'scripts', 'aiwork-seedance.ps1');
const tempDir = fs.mkdtempSync(path.join(os.tmpdir(), 'aiwork-skill-'));
const source = path.join(tempDir, 'first-frame.png');
const output = path.join(tempDir, 'result.mp4');
fs.writeFileSync(source, Buffer.from('png-test-fixture'));

let submitCount = 0;
let uploadCount = 0;
const server = http.createServer((request, response) => {
  const auth = request.headers.authorization;
  if (auth !== 'Bearer test-key') {
    response.writeHead(401, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ error: { message: 'unauthorized' } }));
    return;
  }
  if (request.url === '/health' && request.method === 'GET') {
    response.writeHead(200, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ status: 'ok', pool: { available: 1 } }));
    return;
  }
  const body = [];
  request.on('data', (chunk) => body.push(chunk));
  request.on('end', () => {
    if (request.url === '/v1/assets' && request.method === 'POST') {
      uploadCount += 1;
      response.writeHead(201, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ id: 'asset-test' }));
      return;
    }
    if (request.url === '/v1/videos/generations' && request.method === 'POST') {
      assert.ok(request.headers['idempotency-key']);
      const payload = JSON.parse(Buffer.concat(body).toString('utf8'));
      assert.equal(payload.image_asset_ids[0], 'asset-test');
      submitCount += 1;
      response.writeHead(202, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ task: { id: 'video-test' } }));
      return;
    }
    if (request.url === '/v1/videos/video-test' && request.method === 'GET') {
      response.writeHead(200, { 'content-type': 'application/json' });
      response.end(JSON.stringify({ task: { id: 'video-test', status: 'completed', content_url: '/v1/videos/video-test/content' } }));
      return;
    }
    if (request.url === '/v1/videos/video-test/content' && request.method === 'GET') {
      response.writeHead(200, { 'content-type': 'video/mp4' });
      response.end(Buffer.from('fake-mp4'));
      return;
    }
    response.writeHead(404, { 'content-type': 'application/json' });
    response.end(JSON.stringify({ error: { message: 'not found' } }));
  });
});

function run(action, args = []) {
  return new Promise((resolve, reject) => {
    const child = spawn('powershell.exe', [
      '-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', runner,
      action, ...args,
    ], {
      env: {
        ...process.env,
        AIWORK_GATEWAY_BASE_URL: `http://127.0.0.1:${server.address().port}/v1`,
        AIWORK_API_KEY: 'test-key',
      },
    });
    let stdout = '';
    let stderr = '';
    child.stdout.on('data', (chunk) => { stdout += chunk; });
    child.stderr.on('data', (chunk) => { stderr += chunk; });
    child.on('error', reject);
    child.on('close', (status) => {
      try {
        assert.equal(status, 0, `${action} failed:\nstdout=${stdout}\nstderr=${stderr}`);
        assert.equal(stdout.includes('test-key'), false, `${action} leaked the API key`);
        resolve(JSON.parse(stdout));
      } catch (error) {
        reject(error);
      }
    });
  });
}

server.listen(0, '127.0.0.1', async () => {
  try {
    assert.equal((await run('doctor')).health.status, 'ok');
    const submitted = await run('submit', ['-Prompt', 'test prompt', '-ImagePath', source]);
    assert.equal(submitted.task_id, 'video-test');
    assert.equal(submitCount, 1);
    assert.equal(uploadCount, 1);
    assert.equal((await run('status', ['-TaskId', 'video-test'])).status, 'completed');
    assert.equal((await run('wait', ['-TaskId', 'video-test'])).status, 'completed');
    const downloaded = await run('download', ['-TaskId', 'video-test', '-OutputPath', output]);
    assert.equal(downloaded.download_path, path.resolve(output));
    assert.equal(fs.readFileSync(output, 'utf8'), 'fake-mp4');
    console.log('aiwork skill integration smoke test: passed');
  } catch (error) {
    console.error(error);
    process.exitCode = 1;
  } finally {
    server.close();
    fs.rmSync(tempDir, { recursive: true, force: true });
  }
});
