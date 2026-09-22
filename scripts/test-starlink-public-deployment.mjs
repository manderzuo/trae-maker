import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const read = (relativePath) => fs.readFileSync(path.join(repoRoot, relativePath), 'utf8');
const args = process.argv.slice(2);
const coreFrpc = read('deploy/frp/frpc.starlink-core.toml.example');
const nginx = read('deploy/frp/nginx.starlink-core.conf.example');
const readme = read('deploy/frp/README.md');
const routerHtml = read('starlink-dimension-router/static/index.html');

assert.match(coreFrpc, /name\s*=\s*"starlink-core-gateway"/);
assert.match(coreFrpc, /localPort\s*=\s*7865/);
assert.match(coreFrpc, /remotePort\s*=\s*17865/);
assert.doesNotMatch(coreFrpc, /localPort\s*=\s*7864/);
assert.match(nginx, /server_name\s+api\.gemstory\.cn/);
assert.match(nginx, /proxy_pass\s+http:\/\/starlink_core_backend/);
assert.match(nginx, /server\s+127\.0\.0\.1:17865/);
assert.match(nginx, /listen\s+443\s+ssl/);
assert.doesNotMatch(nginx, /proxy_pass\s+http:\/\/127\.0\.0\.1:7864/);
assert.match(readme, /Core.*7865/);
assert.match(readme, /https:\/\/api\.gemstory\.cn\/v1/);
assert.match(routerHtml, /videoBilling/);
assert.match(routerHtml, /登记一次性验收/);

if (args.includes('--health-only')) {
  const urlArg = args.find((value) => value.startsWith('--url='));
  const healthUrl = urlArg?.slice('--url='.length)
    || process.env.STARLINK_HEALTH_URL
    || 'http://127.0.0.1:7865/healthz';
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 5000);
  try {
    const response = await fetch(healthUrl, { signal: controller.signal });
    assert.equal(response.status, 200, `健康检查返回 HTTP ${response.status}`);
    const body = await response.json();
    assert.equal(body.status, 'ok', '健康检查 status 不是 ok');
    assert.match(String(body.service ?? ''), /星链|starlink/i, '健康检查服务名不匹配');
    console.log(`starlink local health: PASS (${healthUrl})`);
  } finally {
    clearTimeout(timer);
  }
} else {
  console.log('starlink public deployment templates: PASS');
}
