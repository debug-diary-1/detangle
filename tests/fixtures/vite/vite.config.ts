import { defineConfig } from "vite";
import { fileURLToPath, URL } from "node:url";

type Mode = { mode: string; command: string };

export default defineConfig(({ mode }: Mode) => ({
  resolve: {
    alias: [
      { find: "@", replacement: fileURLToPath(new URL("./src", import.meta.url)) },
      { find: /^~(.*)$/, replacement: "/src/shared$1" }, // regex + root-relative
      { find: "#utils", replacement: "/src/utils" },     // root-relative
      { find: "rel", replacement: "./local" },           // relative: left as-is
    ],
  },
  define: { __MODE__: JSON.stringify(mode) },
}));
