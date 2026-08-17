/**
 * The `/admin/*` management API — what the console (`console/`) drives.
 *
 * Wire contract is the Rust server's `rust/smooth-operator-server/src/admin.rs`:
 * same paths, same **camelCase** JSON, the same `{"error":{"code","message"}}`
 * envelope, and the same role gate (Bearer token → verify → rank check; 401
 * missing/invalid, 403 insufficient). Rank: basic=0, curator=1, admin=2.
 *
 * Shapes are built against `console/lib/types.ts`, not Rust's field names: Rust's
 * structs read snake_case in source but carry `#[serde(rename_all = "camelCase")]`,
 * so copying the field names would produce a server that passes its own tests and
 * renders nothing.
 *
 * ponytail: connector configs, settings and indexing runs live in memory
 * (`AdminStores`) because this server is memory-only today. The durable storage
 * adapter is a separate workstream — swap those three maps; nothing outside this
 * file reads them.
 */

import { randomUUID } from 'node:crypto';
import type { IncomingMessage, ServerResponse } from 'node:http';

import type { AuthVerifier, Principal } from './auth.js';
import type { SessionStore } from './sessionStore.js';

/** Role ranks, mirroring Rust's `role_rank`. */
const ROLE_BASIC = 0;
const ROLE_CURATOR = 1;
const ROLE_ADMIN = 2;

/** Unknown/empty roles are basic — fail closed on privilege, not open. */
function roleRank(role: string): number {
    switch (role.trim().toLowerCase()) {
        case 'admin':
            return ROLE_ADMIN;
        case 'curator':
            return ROLE_CURATOR;
        default:
            return ROLE_BASIC;
    }
}

function rankName(rank: number): string {
    return rank === ROLE_ADMIN ? 'admin' : rank === ROLE_CURATOR ? 'curator' : 'basic';
}

// ── in-memory admin state ───────────────────────────────────────────────────

interface ConnectorConfig {
    id: string;
    name: string;
    kind: string;
    config: Record<string, unknown>;
    enabled: boolean;
    createdAt: string;
    updatedAt: string;
    /** Not serialized — the org that owns this row. */
    orgId?: string;
}

interface AgentSettings {
    orgId: string;
    model: string;
    systemPrompt: string;
    defaultTools: string[];
    updatedAt: string;
}

interface IndexingRun {
    id: string;
    connectorName: string;
    status: string;
    startedAt: string;
    finishedAt: string | null;
    documentsSeen: number;
    chunksIndexed: number;
    documentsSkipped: number;
    error: string | null;
    orgId?: string;
}

/**
 * Org-scoped admin state. Every read and write filters by org, so one org can
 * never see or mutate another's rows.
 */
export class AdminStores {
    readonly connectors = new Map<string, ConnectorConfig>();
    readonly settings = new Map<string, AgentSettings>();
    readonly runs: IndexingRun[] = [];
}

/** Rust's "defaults when unset" settings read. */
function defaultSettings(orgId: string): AgentSettings {
    return { orgId, model: 'claude-haiku-4-5', systemPrompt: '', defaultTools: [], updatedAt: new Date().toISOString() };
}

/** Strip the internal owner key before serializing. */
function publicRow<T extends { orgId?: string }>(row: T): Omit<T, 'orgId'> {
    const { orgId: _orgId, ...rest } = row;
    return rest;
}

// ── responses ───────────────────────────────────────────────────────────────

function sendJson(res: ServerResponse, status: number, body: unknown): void {
    res.writeHead(status, { 'content-type': 'application/json' });
    res.end(JSON.stringify(body));
}

function sendError(res: ServerResponse, status: number, code: string, message: string): void {
    sendJson(res, status, { error: { code, message } });
}

async function readJsonBody(req: IncomingMessage): Promise<unknown> {
    const chunks: Buffer[] = [];
    for await (const chunk of req) chunks.push(chunk as Buffer);
    const raw = Buffer.concat(chunks).toString('utf8');
    return raw.trim() === '' ? {} : JSON.parse(raw);
}

// ── auth gate ───────────────────────────────────────────────────────────────

/** The raw token from `Authorization: Bearer <token>`, or undefined. */
function bearerToken(req: IncomingMessage): string | undefined {
    const value = req.headers.authorization;
    if (!value) return undefined;
    const lower = value.toLowerCase();
    if (!lower.startsWith('bearer ')) return undefined;
    const token = value.slice('bearer '.length).trim();
    return token === '' ? undefined : token;
}

export interface AdminDeps {
    auth: AuthVerifier;
    store: SessionStore;
    stores: AdminStores;
}

/**
 * Authenticate and enforce a minimum role. Returns the principal, or writes the
 * rejection and returns undefined. Fails CLOSED: no token is 401 even on a
 * no-auth server.
 */
function requireRole(deps: AdminDeps, req: IncomingMessage, res: ServerResponse, min: number): Principal | undefined {
    const token = bearerToken(req);
    if (token === undefined) {
        sendError(res, 401, 'UNAUTHENTICATED', 'missing bearer token');
        return undefined;
    }
    const access = deps.auth.resolve(token);
    // An auth-enabled server that could not verify the token yields an anonymous
    // context, which must never satisfy an admin route.
    if (access.authEnabled && access.isAnonymous) {
        sendError(res, 401, 'INVALID_TOKEN', 'invalid bearer token');
        return undefined;
    }
    const principal = { ...access.principal };
    // AUTH_MODE=none (dev) grants Admin, exactly as Rust's NoAuthVerifier does —
    // otherwise the console 403-walls against a local server, which is as useless
    // as the 404s this API exists to fix. Only the explicit dev verifier takes
    // this path; an auth-enabled server is unaffected.
    if (deps.auth.mode === 'none') principal.role = 'admin';

    if (roleRank(principal.role) < min) {
        sendError(res, 403, 'FORBIDDEN', `requires role ${rankName(min)}, principal has ${rankName(roleRank(principal.role))}`);
        return undefined;
    }
    return principal;
}

// ── the handler ─────────────────────────────────────────────────────────────

/**
 * Serve one `/admin/*` request. Returns false when the path is not an admin
 * route, so the caller can fall through to its own response.
 */
export async function handleAdminRequest(deps: AdminDeps, req: IncomingMessage, res: ServerResponse): Promise<boolean> {
    const url = new URL(req.url ?? '/', 'http://localhost');
    const path = url.pathname;
    if (!path.startsWith('/admin/')) return false;
    const method = (req.method ?? 'GET').toUpperCase();

    try {
        await route(deps, req, res, method, path, url);
    } catch (err) {
        if (err instanceof SyntaxError) {
            sendError(res, 400, 'INVALID_BODY', 'malformed JSON body');
        } else {
            sendError(res, 500, 'INTERNAL', 'admin request failed');
        }
    }
    return true;
}

async function route(
    deps: AdminDeps,
    req: IncomingMessage,
    res: ServerResponse,
    method: string,
    path: string,
    url: URL,
): Promise<void> {
    // Ungated, exactly as in Rust: the console probes health before it has a token.
    if (method === 'GET' && path === '/admin/health') {
        sendJson(res, 200, { status: 'ok' });
        return;
    }

    if (method === 'GET' && path === '/admin/me') {
        const p = requireRole(deps, req, res, ROLE_BASIC);
        if (!p) return;
        sendJson(res, 200, { userId: p.sub, orgId: p.org, role: rankName(roleRank(p.role)) });
        return;
    }

    if (method === 'GET' && path === '/admin/conversations') {
        const p = requireRole(deps, req, res, ROLE_BASIC);
        if (!p) return;
        const limit = Number(url.searchParams.get('limit')) || 50;
        const cursor = Number(url.searchParams.get('cursor')) || 0;
        const all = await deps.store.listConversations(p.email);
        all.sort((a, b) => b.updatedAt.localeCompare(a.updatedAt));
        const page = all.slice(cursor, cursor + limit);
        const end = cursor + page.length;
        sendJson(res, 200, {
            conversations: page.map((c) => ({
                id: c.conversationId,
                name: c.firstInboundText || 'Conversation',
                platform: 'web',
                createdAt: c.updatedAt,
                updatedAt: c.updatedAt,
            })),
            nextCursor: end < all.length ? end : null,
        });
        return;
    }

    const messagesMatch = /^\/admin\/conversations\/([^/]+)\/messages$/.exec(path);
    if (method === 'GET' && messagesMatch) {
        if (!requireRole(deps, req, res, ROLE_BASIC)) return;
        const id = decodeURIComponent(messagesMatch[1] ?? '');
        const stored = await deps.store.listMessages(id, 500);
        sendJson(res, 200, {
            conversationId: id,
            messages: stored.map((m) => ({
                id: m.id,
                conversationId: m.conversationId,
                direction: m.direction,
                content: { items: [{ type: 'text', text: m.text }], text: m.text },
                createdAt: m.createdAt,
            })),
            nextCursor: null,
        });
        return;
    }

    if (method === 'GET' && path === '/admin/indexing/runs') {
        const p = requireRole(deps, req, res, ROLE_CURATOR);
        if (!p) return;
        sendJson(res, 200, { runs: deps.stores.runs.filter((r) => r.orgId === p.org).map(publicRow) });
        return;
    }

    if (method === 'GET' && path === '/admin/document-sets') {
        if (!requireRole(deps, req, res, ROLE_CURATOR)) return;
        // ponytail: no knowledge store on this server yet, so there are no document
        // sets to count. An empty list is the honest answer and renders fine.
        sendJson(res, 200, { documentSets: [] });
        return;
    }

    if (path === '/admin/connectors') {
        if (method === 'GET') {
            const p = requireRole(deps, req, res, ROLE_CURATOR);
            if (!p) return;
            const rows = [...deps.stores.connectors.values()]
                .filter((c) => c.orgId === p.org)
                .sort((a, b) => a.name.localeCompare(b.name))
                .map(publicRow);
            sendJson(res, 200, { connectors: rows });
            return;
        }
        if (method === 'POST') {
            const p = requireRole(deps, req, res, ROLE_ADMIN);
            if (!p) return;
            const body = await readJsonBody(req);
            const write = validateConnector(body, res);
            if (!write) return;
            const now = new Date().toISOString();
            const row: ConnectorConfig = { id: randomUUID(), ...write, createdAt: now, updatedAt: now, orgId: p.org };
            deps.stores.connectors.set(row.id, row);
            sendJson(res, 200, { connector: publicRow(row) });
            return;
        }
    }

    const indexMatch = /^\/admin\/connectors\/([^/]+)\/index$/.exec(path);
    if (method === 'POST' && indexMatch) {
        const p = requireRole(deps, req, res, ROLE_CURATOR);
        if (!p) return;
        const row = ownedConnector(deps, decodeURIComponent(indexMatch[1] ?? ''), p.org, res);
        if (!row) return;
        // ponytail: no ingestion pipeline on this server yet, so the run is recorded
        // as succeeded with zero documents rather than faked with invented counts.
        const now = new Date().toISOString();
        const run: IndexingRun = {
            id: randomUUID(),
            connectorName: row.name,
            status: 'succeeded',
            startedAt: now,
            finishedAt: now,
            documentsSeen: 0,
            chunksIndexed: 0,
            documentsSkipped: 0,
            error: null,
            orgId: p.org,
        };
        deps.stores.runs.push(run);
        sendJson(res, 200, { run: publicRow(run) });
        return;
    }

    const connectorMatch = /^\/admin\/connectors\/([^/]+)$/.exec(path);
    if (connectorMatch) {
        const id = decodeURIComponent(connectorMatch[1] ?? '');
        if (method === 'GET') {
            const p = requireRole(deps, req, res, ROLE_CURATOR);
            if (!p) return;
            const row = ownedConnector(deps, id, p.org, res);
            if (!row) return;
            sendJson(res, 200, { connector: publicRow(row) });
            return;
        }
        if (method === 'PUT') {
            const p = requireRole(deps, req, res, ROLE_ADMIN);
            if (!p) return;
            const body = await readJsonBody(req);
            const write = validateConnector(body, res);
            if (!write) return;
            const row = ownedConnector(deps, id, p.org, res);
            if (!row) return;
            Object.assign(row, write, { updatedAt: new Date().toISOString() });
            sendJson(res, 200, { connector: publicRow(row) });
            return;
        }
        if (method === 'DELETE') {
            const p = requireRole(deps, req, res, ROLE_ADMIN);
            if (!p) return;
            if (!ownedConnector(deps, id, p.org, res)) return;
            deps.stores.connectors.delete(id);
            res.writeHead(204);
            res.end();
            return;
        }
    }

    if (path === '/admin/settings') {
        if (method === 'GET') {
            const p = requireRole(deps, req, res, ROLE_CURATOR);
            if (!p) return;
            sendJson(res, 200, { settings: deps.stores.settings.get(p.org) ?? defaultSettings(p.org) });
            return;
        }
        if (method === 'PUT') {
            const p = requireRole(deps, req, res, ROLE_ADMIN);
            if (!p) return;
            const body = (await readJsonBody(req)) as Partial<AgentSettings>;
            if (typeof body.model !== 'string' || body.model.trim() === '') {
                sendError(res, 400, 'INVALID_BODY', 'model is required');
                return;
            }
            const settings: AgentSettings = {
                orgId: p.org,
                model: body.model,
                systemPrompt: typeof body.systemPrompt === 'string' ? body.systemPrompt : '',
                defaultTools: Array.isArray(body.defaultTools) ? body.defaultTools : [],
                updatedAt: new Date().toISOString(),
            };
            deps.stores.settings.set(p.org, settings);
            sendJson(res, 200, { settings });
            return;
        }
    }

    sendError(res, 404, 'NOT_FOUND', `no admin route for ${method} ${path}`);
}

/** An org-owned connector, or a 404. A cross-org id is deliberately indistinguishable from an unknown one. */
function ownedConnector(deps: AdminDeps, id: string, orgId: string, res: ServerResponse): ConnectorConfig | undefined {
    const row = deps.stores.connectors.get(id);
    if (!row || row.orgId !== orgId) {
        sendError(res, 404, 'NOT_FOUND', 'connector not found');
        return undefined;
    }
    return row;
}

/** Validate a connector write body, or 400. */
function validateConnector(
    body: unknown,
    res: ServerResponse,
): { name: string; kind: string; config: Record<string, unknown>; enabled: boolean } | undefined {
    const b = body as Partial<ConnectorConfig> | null;
    if (!b || typeof b.name !== 'string' || b.name.trim() === '' || typeof b.kind !== 'string' || b.kind.trim() === '') {
        sendError(res, 400, 'INVALID_BODY', 'name and kind are required');
        return undefined;
    }
    return {
        name: b.name,
        kind: b.kind,
        config: (b.config as Record<string, unknown>) ?? {},
        enabled: b.enabled === true,
    };
}
