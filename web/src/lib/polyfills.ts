// @ton/core / @ton/crypto use Node's Buffer; ensure it exists in the browser.
import { Buffer as BufferPolyfill } from "buffer";

const g = globalThis as unknown as { Buffer?: unknown };
if (typeof g.Buffer === "undefined") {
  g.Buffer = BufferPolyfill;
}

export {};
