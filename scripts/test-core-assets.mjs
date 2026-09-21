import assert from 'node:assert/strict';
import http from 'node:http';

const png = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x66, 0x69, 0x78, 0x74, 0x75, 0x72, 0x65]);
const videoRequests = [];
let bridgeFailure = false;

function send(response, status, value, contentType = 'application/json') {
  response.writeHead(status, { 'content-type': contentType });
  response.end(contentType === 'application/json' ? JSON.stringify(value) : value);
}

async function readJson(request) {
  const chunks = [];
  for await (const chunk of request) chunks.push(chunk);
  return JSON.parse(Buffer.concat(chunks).toString('utf8'));
}

async function fakeBridgeUpload(assetId) {
  if (bridgeFailure) return { status: 502, id: null };
  return { status: 200, id: `bridge-${assetId}` };
}

const server = http.createServer(async (request, response) => {
  try {
    const url = new URL(request.url, 'http://127.0.0.1');
    const authorization = request.headers.authorization || '';

    if (request.method === 'POST' && url.pathname === '/v1/assets') {
      if (!authorization) return send(response, 401, { error: { type: 'authentication_error' } });
      if (authorization === 'Bearer no-assets') return send(response, 403, { error: { type: 'permission_error' } });
      const body = await readJson(request);
      assert.equal(body.filename, 'ref.png');
      assert.equal(body.mime_type, 'image/png');
      assert.equal(body.data_base64, png.toString('base64'));
      return send(response, 200, {
        object: 'asset',
        id: 'core-asset-1',
        content_url: `http://127.0.0.1:${server.address().port}/v1/assets/core-asset-1/content?token=t1`,
      });
    }

    if (request.method === 'GET' && url.pathname === '/v1/assets/core-asset-1/content') {
      if (url.searchParams.get('token') !== 't1') return send(response, 404, { error: { type: 'asset_not_found' } });
      return send(response, 200, png, 'image/png');
    }

    if (request.method === 'POST' && url.pathname === '/v1/videos/generations') {
      if (!authorization) return send(response, 401, { error: { type: 'authentication_error' } });
      const body = await readJson(request);
      if (authorization === 'Bearer user-2' && body.image_asset_ids?.includes('core-asset-1')) {
        return send(response, 404, { error: { type: 'asset_not_found' } });
      }
      const bridge = await fakeBridgeUpload(body.image_asset_ids?.[0] || 'missing');
      if (bridge.status !== 200) return send(response, 502, { error: { type: 'bridge_error' } });
      const forwarded = { ...body, image_asset_ids: [bridge.id] };
      videoRequests.push(forwarded);
      return send(response, 202, { task: { id: 'video-task-1', status: 'queued' } });
    }

    return send(response, 404, { error: { type: 'not_found' } });
  } catch (error) {
    return send(response, 500, { error: { type: 'test_harness_error', message: String(error) } });
  }
});

await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
const base = `http://127.0.0.1:${server.address().port}`;
const call = (path, options = {}) => fetch(`${base}${path}`, options);

try {
  let response = await call('/v1/assets', { method: 'POST', body: JSON.stringify({ filename: 'ref.png', mime_type: 'image/png', data_base64: png.toString('base64') }) });
  assert.equal(response.status, 401);

  response = await call('/v1/assets', {
    method: 'POST',
    headers: { authorization: 'Bearer no-assets', 'content-type': 'application/json' },
    body: JSON.stringify({ filename: 'ref.png', mime_type: 'image/png', data_base64: png.toString('base64') }),
  });
  assert.equal(response.status, 403);

  response = await call('/v1/assets', {
    method: 'POST',
    headers: { authorization: 'Bearer user-1', 'content-type': 'application/json' },
    body: JSON.stringify({ filename: 'ref.png', mime_type: 'image/png', data_base64: png.toString('base64') }),
  });
  assert.equal(response.status, 200);
  const asset = await response.json();
  assert.equal(asset.id, 'core-asset-1');
  assert.match(asset.content_url, /token=t1$/);

  response = await fetch(asset.content_url);
  assert.equal(response.status, 200);
  assert.deepEqual(Buffer.from(await response.arrayBuffer()), png);
  response = await call('/v1/assets/core-asset-1/content?token=wrong');
  assert.equal(response.status, 404);

  response = await call('/v1/videos/generations', {
    method: 'POST',
    headers: { authorization: 'Bearer user-2', 'content-type': 'application/json' },
    body: JSON.stringify({ model: 'seedance', prompt: 'test', image_asset_ids: ['core-asset-1'] }),
  });
  assert.equal(response.status, 404);
  assert.equal(videoRequests.length, 0);

  response = await call('/v1/videos/generations', {
    method: 'POST',
    headers: { authorization: 'Bearer user-1', 'content-type': 'application/json' },
    body: JSON.stringify({ model: 'seedance', prompt: 'test', image_asset_ids: ['core-asset-1'] }),
  });
  assert.equal(response.status, 202);
  assert.equal(videoRequests[0].image_asset_ids[0], 'bridge-core-asset-1');
  assert.notEqual(videoRequests[0].image_asset_ids[0], 'core-asset-1');

  bridgeFailure = true;
  response = await call('/v1/videos/generations', {
    method: 'POST',
    headers: { authorization: 'Bearer user-1', 'content-type': 'application/json' },
    body: JSON.stringify({ model: 'seedance', prompt: 'test', image_asset_ids: ['core-asset-1'] }),
  });
  assert.equal(response.status, 502);
  assert.equal(videoRequests.length, 1);
  console.log('core asset public contract smoke: PASS');
} finally {
  await new Promise(resolve => server.close(resolve));
}
