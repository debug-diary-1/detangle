// "types" maps types.js to dist/esm/types.js.d.ts, which doesn't exist;
// "import" maps it to dist/esm/types.js, which does.
import type { Result } from "sdk/types.js";

export const result: Result = { ok: true };
