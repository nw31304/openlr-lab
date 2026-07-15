export async function initWasm() {
  const mod = await import('./wasm/openlr_wasm.js');
  await mod.default();
  return { decoder: new mod.Decoder(), encoder: new mod.Encoder() };
}
