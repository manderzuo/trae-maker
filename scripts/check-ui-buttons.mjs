/**
 * Fast static smoke check for React button wiring.
 * It intentionally checks only invariants that are safe to infer from TSX:
 * every non-disabled button has an event/submit handler and no empty handler
 * is left behind. Runtime behavior still belongs to Vitest/Tauri smoke tests.
 */
import fs from 'node:fs';
import path from 'node:path';

const files = [];
function walk(dir) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const file = path.join(dir, entry.name);
    if (entry.isDirectory()) walk(file);
    else if (/\.(tsx|ts)$/.test(entry.name)) files.push(file);
  }
}

walk(path.resolve('src'));
let emptyHandlers = 0;
let unhandled = 0;
for (const file of files) {
  const source = fs.readFileSync(file, 'utf8');
  if (/onClick\s*=\s*\{\s*\(\)\s*=>\s*\{\s*\}\s*\}/s.test(source)) {
    console.error(`empty onClick handler: ${path.relative(process.cwd(), file)}`);
    emptyHandlers += 1;
  }
  for (const match of source.matchAll(/<button\b[^>]*>/g)) {
    const tag = match[0];
    if (!/onClick\s*=|onMouseDown\s*=|type\s*=\s*["']submit|disabled/.test(tag)) {
      console.error(`button without handler: ${path.relative(process.cwd(), file)}`);
      unhandled += 1;
    }
  }
}

console.log(`button smoke scan: empty=${emptyHandlers}, unhandled=${unhandled}`);
if (emptyHandlers || unhandled) process.exitCode = 1;
