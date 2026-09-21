import assert from 'node:assert/strict';
import fs from 'node:fs';

const html = fs.readFileSync(new URL('../starlink-dimension-router/static/index.html', import.meta.url), 'utf8');
assert.match(html, /setInterval\(/, 'management page must auto-refresh after connection');
assert.match(html, /admin\/v1\/login/, 'management page must expose account login');
assert.match(html, /admin\/v1\/session/, 'management page must restore the current session');
assert.match(html, /admin\/v1\/logout/, 'management page must expose logout');
assert.match(html, /修改管理员密码/, 'management page must expose password change');
assert.match(html, /credentials:'same-origin'/, 'management page must use same-origin cookies');
assert.doesNotMatch(html, /localStorage/, 'management page must not persist admin credentials in localStorage');
assert.doesNotMatch(html, /Core 管理员 API Key（可选择保存到本机浏览器）/, 'management page must not show the Core admin key field');
assert.doesNotMatch(html, /ADMIN_KEY_STORAGE/, 'management page must not contain the old admin key storage constant');
assert.match(html, /max_concurrency/, 'management page must submit a per-key concurrency limit');
assert.match(html, /文字处理/, 'scope labels must be translated to Chinese');
assert.match(html, /视频生成/, 'video scope label must be translated to Chinese');
console.log('starlink router UI contract: PASS');
