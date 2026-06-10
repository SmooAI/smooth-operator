import { defineConfig } from 'tsdown';

// Builds ONLY the widget's standalone IIFE bundle — for a plain
// `<script src="…/widget/standalone">` embed — into
// `dist/widget/chat-widget.global.js`. Everything (the protocol client included)
// is bundled in; on load it registers `<smooth-agent-chat>` and exposes the API
// on `window.SmoothAgentChat`.
//
// The ESM library entry points (`.`, `./validate`, `./react`, `./widget`) are
// emitted by `tsc` (see `build` in package.json). This config only adds the one
// bundled artifact tsc can't produce, so `clean: false` — it must never wipe
// tsc's output. The widget imports the client via `../client.js` (not the `.`
// barrel), so the bundle stays clean of the Node-only validator + `ajv`.
export default defineConfig({
    entry: { 'widget/chat-widget': 'src/widget/standalone.ts' },
    format: ['iife'],
    platform: 'browser',
    globalName: 'SmoothAgentChat',
    dts: false,
    sourcemap: true,
    clean: false,
    outDir: 'dist',
});
