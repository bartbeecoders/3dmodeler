// Shared instantiation glue for the Phase 0 spike wasm module.
// Works in both the browser (index.html) and node (run_node.mjs).
//
// Any import the module asks for is stubbed with a warning function; the two
// meaningful imports (env.host_log) are wired to the provided logger. The
// import list itself is a key Phase 0 diagnostic: ideally everything the
// module needs is env.host_log and nothing from wasi_snapshot_preview1.

export async function runSpike(wasmBytes, { log, warn }) {
  const module = await WebAssembly.compile(wasmBytes);

  const importList = WebAssembly.Module.imports(module);
  warn(`module imports: ${importList.length === 0 ? '(none)' : ''}`);
  for (const imp of importList) {
    warn(`  ${imp.module}.${imp.name} (${imp.kind})`);
  }

  let memory = null;
  const decoder = new TextDecoder();
  const imports = {};
  for (const imp of importList) {
    imports[imp.module] ??= {};
    imports[imp.module][imp.name] = (...args) => {
      warn(`stubbed import called: ${imp.module}.${imp.name}(${args.join(', ')})`);
      return 0;
    };
  }
  imports.env ??= {};
  imports.env.host_log = (ptr, len) => {
    log(decoder.decode(new Uint8Array(memory.buffer, ptr, len)));
  };

  const instance = await WebAssembly.instantiate(module, imports);
  memory = instance.exports.memory;

  const rc = instance.exports.phase0_run();
  log(`phase0_run() returned ${rc} (${rc === 0 ? 'PASS' : 'FAIL'})`);
  return rc;
}
