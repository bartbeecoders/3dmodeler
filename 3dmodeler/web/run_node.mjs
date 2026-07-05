// Headless check of the Phase 0 wasm module under node.
// Usage: node web/run_node.mjs
import { readFile } from 'node:fs/promises';
import { runSpike } from './spike-glue.mjs';

const wasmPath = new URL(
  '../target/wasm32-unknown-unknown/release/phase0_spike.wasm',
  import.meta.url
);
const bytes = await readFile(wasmPath);
const rc = await runSpike(bytes, {
  log: (m) => console.log(m),
  warn: (m) => console.warn(m),
});
process.exit(rc);
