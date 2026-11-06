#!/usr/bin/env node
// Regenerate src/vendor/react-big-calendar.mjs — a self-contained ESM bundle
// of react-big-calendar for the plugin frontend.
//
//   node scripts/build-vendor.mjs
//
// Why this exists: the plugin build (app/scripts/build-plugin.mjs) aliases
// bare "react"/"react-dom" imports to the SDK host shims, which predate
// react-big-calendar's needs (createRef; createPortal via react-overlays).
// The SDK lives outside this plugin, so instead the vendor bundle is
// pre-built here against the local host-compat shims in src/vendor/:
// react/react-dom resolve to the HOST's instances at runtime, while
// everything else (dayjs, react-overlays, lodash, ...) is inlined.
// date-fns is left external so the section's own imports share one copy.
//
// Re-run after upgrading the react-big-calendar dependency.

import { createRequire } from "node:module";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const pluginDir = path.resolve(here, "..");
const require = createRequire(import.meta.url);

let esbuild;
try {
  esbuild = require(path.join(pluginDir, "..", "..", "app", "node_modules", "esbuild"));
} catch {
  esbuild = require("esbuild"); // fallback: esbuild on PATH/node_modules
}

const rbcEntry = require.resolve("react-big-calendar/dist/react-big-calendar.esm.js", {
  paths: [pluginDir],
});

await esbuild.build({
  entryPoints: [rbcEntry],
  bundle: true,
  format: "esm",
  target: ["chrome120", "safari17"],
  outfile: path.join(pluginDir, "src", "vendor", "react-big-calendar.mjs"),
  alias: {
    react: path.join(pluginDir, "src", "vendor", "react-compat.js"),
    "react-dom": path.join(pluginDir, "src", "vendor", "react-dom-compat.js"),
  },
  external: ["date-fns"],
  logLevel: "info",
});
