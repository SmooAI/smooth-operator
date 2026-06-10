/**
 * Minimal static file server for the Playwright live e2e.
 *
 * Serves the repo root so the demo page (`/e2e/fixtures/demo.html`) can load
 * the built IIFE bundle at `/dist/widget/chat-widget.iife.js`. Kept dependency-free
 * (node:http) so the e2e adds no runtime deps beyond @playwright/test.
 *
 * Port comes from $STATIC_PORT (default 4830). Playwright's `webServer` block
 * boots this and waits for the demo URL to respond.
 */
import { createServer } from 'node:http';
import { readFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import { extname, join, normalize } from 'node:path';

const ROOT = fileURLToPath(new URL('..', import.meta.url));
const PORT = Number(process.env.STATIC_PORT ?? 4830);

const MIME = {
    '.html': 'text/html; charset=utf-8',
    '.js': 'text/javascript; charset=utf-8',
    '.mjs': 'text/javascript; charset=utf-8',
    '.map': 'application/json; charset=utf-8',
    '.css': 'text/css; charset=utf-8',
    '.json': 'application/json; charset=utf-8',
};

const server = createServer(async (req, res) => {
    try {
        const url = new URL(req.url ?? '/', `http://localhost:${PORT}`);
        // Strip the leading slash and normalize to prevent path traversal.
        const rel = normalize(decodeURIComponent(url.pathname)).replace(/^(\.\.[/\\])+/, '').replace(/^[/\\]+/, '');
        const filePath = join(ROOT, rel || 'index.html');
        if (!filePath.startsWith(ROOT)) {
            res.writeHead(403).end('Forbidden');
            return;
        }
        const body = await readFile(filePath);
        res.writeHead(200, { 'Content-Type': MIME[extname(filePath)] ?? 'application/octet-stream' });
        res.end(body);
    } catch {
        res.writeHead(404).end('Not Found');
    }
});

server.listen(PORT, '127.0.0.1', () => {
    // eslint-disable-next-line no-console
    console.log(`static server listening on http://127.0.0.1:${PORT}`);
});
