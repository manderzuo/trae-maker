import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { spawnSync } from 'node:child_process';

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const routerDir = path.join(repoRoot, 'starlink-dimension-router');
const cargo = 'C:\\Users\\StarLink\\.cargo\\bin\\cargo.exe';
const args = process.argv.slice(2);
const dataArg = args.find((value) => value.startsWith('--data-dir='));
const dataDir = dataArg ? path.resolve(dataArg.slice('--data-dir='.length)) : 'D:\\gpt\\starlink-video-billing-test';
if (!/^D:[\\/]/i.test(dataDir)) {
  throw new Error(`测试数据目录必须位于 D 盘: ${dataDir}`);
}
fs.mkdirSync(dataDir, { recursive: true });

const targetDir = path.join(dataDir, 'cargo-target');
const logDir = path.join(dataDir, 'logs');
fs.mkdirSync(logDir, { recursive: true });

function cleanEnvironment() {
  const env = { ...process.env };
  for (const key of Object.keys(env)) {
    if (key.startsWith('AIWORK_') || key === 'AGENT_HOST') delete env[key];
  }
  env.CARGO_TARGET_DIR = targetDir;
  env.TEMP = 'D:\\gpt';
  env.TMP = 'D:\\gpt';
  delete env.STARLINK_ROUTER_DATA_DIR;
  return env;
}

function runCargo(label, cargoArgs) {
  const result = spawnSync(cargo, cargoArgs, {
    cwd: routerDir,
    env: cleanEnvironment(),
    encoding: 'utf8',
    maxBuffer: 32 * 1024 * 1024,
  });
  const output = `${result.stdout ?? ''}${result.stderr ?? ''}`;
  fs.writeFileSync(path.join(logDir, `${label}.log`), output, 'utf8');
  assert.equal(result.error, undefined, `${label} 无法启动 Cargo: ${result.error?.message ?? ''}`);
  assert.equal(result.status, 0, `${label} 失败；完整日志已保存到 ${logDir}`);
  assert.match(output, /test result:\s+ok\./, `${label} 未返回通过结果`);
  return output;
}

const video = runCargo('video-billing', ['test', '--offline', '--locked', '--test', 'video_billing', '--', '--nocapture']);
for (const name of [
  'paused_video_gate_rejects_before_reservation_or_bridge',
  'paused_seedance_chat_rejects_before_reservation_or_bridge',
  'diagnostic_claim_allows_one_matching_request_then_pauses_again',
  'verified_receipt_commits_actual_credits_once',
  'completed_without_verified_receipt_is_reconciliation_required',
  'accepted_job_is_restored_from_disk_after_router_restart',
]) {
  assert.match(video, new RegExp(`test ${name} \.\.\. ok`), `缺少视频计费场景证据: ${name}`);
}

runCargo('admin-api', ['test', '--offline', '--locked', '--test', 'admin_api', '--', '--nocapture']);
runCargo('assets-api', ['test', '--offline', '--locked', '--test', 'assets_api', '--', '--nocapture']);
runCargo('router-lib', ['test', '--offline', '--locked', '--lib', '--', '--nocapture']);

const report = {
  status: 'ok',
  mode: 'local_fake_bridge_only',
  real_upstream_requests: 0,
  public_requests: 0,
  target_dir: targetDir,
  logs: logDir,
  scenarios: ['paused_direct_video', 'paused_seedance_chat', 'diagnostic_once', 'held_202', 'verified_settlement', 'unverified_completion', 'pre_accept_rejection', 'duplicate_poll', 'restart_restore'],
};
fs.writeFileSync(path.join(dataDir, 'video-billing-verification.json'), `${JSON.stringify(report, null, 2)}\n`, 'utf8');
console.log(`video billing local verification: PASS (${dataDir})`);
