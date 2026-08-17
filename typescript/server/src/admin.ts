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
 * Connector configs, settings and indexing runs sit behind the {@link AdminStore}
 * seam. {@link InMemoryAdminStore} is the default (this server is memory-only unless
 * told otherwise); `PostgresStore` (postgresStore.ts) is the durable implementation,
 * selected with `SMOOTH_AGENT_STORAGE=postgres`.
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

export interface ConnectorConfig {
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

export interface AgentSettings {
    orgId: string;
    model: string;
    systemPrompt: string;
    defaultTools: string[];
    updatedAt: string;
}

export interface IndexingRun {
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
 * The persistence seam for the three management-console stores. Every method takes
 * the caller's org and filters by it, so one org can never see or mutate another's
 * rows. A cross-org id is reported exactly like an unknown one (`undefined` /
 * `false`), so the handlers render an identical 404 and the id space cannot be probed.
 *
 * Two implementations: {@link InMemoryAdminStore} (default) and `PostgresStore`.
 */
export interface AdminStore {
    listConnectors(orgId: string): Promise<ConnectorConfig[]>;
    /** `undefined` when the org has no such connector — including "it's another org's". */
    getConnector(orgId: string, id: string): Promise<ConnectorConfig | undefined>;
    putConnector(connector: ConnectorConfig): Promise<void>;
    /** Whether the connector existed in that org. */
    deleteConnector(orgId: string, id: string): Promise<boolean>;
    /** `undefined` when the org has none; the caller substitutes defaults. */
    getSettings(orgId: string): Promise<AgentSettings | undefined>;
    putSettings(settings: AgentSettings): Promise<void>;
    listRuns(orgId: string): Promise<IndexingRun[]>;
    recordRun(run: IndexingRun): Promise<void>;
}

/** In-process {@link AdminStore} — the reference implementation. */
export class InMemoryAdminStore implements AdminStore {
    private readonly connectors = new Map<string, ConnectorConfig>();
    private readonly settings = new Map<string, AgentSettings>();
    private readonly runs: IndexingRun[] = [];

    async listConnectors(orgId: string): Promise<ConnectorConfig[]> {
        return [...this.connectors.values()].filter((c) => c.orgId === orgId).sort((a, b) => a.name.localeCompare(b.name));
    }

    async getConnector(orgId: string, id: string): Promise<ConnectorConfig | undefined> {
        // A cross-org id takes the same branch as an unknown one.
        const row = this.connectors.get(id);
        return row && row.orgId === orgId ? { ...row } : undefined;
    }

    async putConnector(connector: ConnectorConfig): Promise<void> {
        this.connectors.set(connector.id, { ...connector });
    }

    async deleteConnector(orgId: string, id: string): Promise<boolean> {
        const row = this.connectors.get(id);
        if (!row || row.orgId !== orgId) return false;
        this.connectors.delete(id);
        return true;
    }

    async getSettings(orgId: string): Promise<AgentSettings | undefined> {
        const row = this.settings.get(orgId);
        return row ? { ...row } : undefined;
    }

    async putSettings(settings: AgentSettings): Promise<void> {
        this.settings.set(settings.orgId, { ...settings });
    }

    async listRuns(orgId: string): Promise<IndexingRun[]> {
        return this.runs.filter((r) => r.orgId === orgId);
    }

    async recordRun(run: IndexingRun): Promise<void> {
        const index = this.runs.findIndex((r) => r.id === run.id);
        if (index >= 0) this.runs[index] = { ...run };
        else this.runs.push({ ...run });
    }
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
    stores: AdminStore;
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

// ── model costs ─────────────────────────────────────────────────────────────

/**
 * The mapped `/model/info` payload for the process. Gateway pricing is stable,
 * so one fetch per process is enough (matching Rust's `OnceCell`). Only a
 * SUCCESS is cached — an error leaves it unset so the next request retries,
 * rather than pinning an empty map for the process lifetime.
 */
let modelCostsCache: Record<string, unknown> | undefined;

/** Reset the process-wide cache. Test seam. */
export function resetModelCostsCache(): void {
    modelCostsCache = undefined;
}

/**
 * Map the gateway's `/model/info` payload into the shape the console reads.
 * Pure, so it is unit-testable without a gateway. Entries without a `model_name`
 * are skipped, and every field is optional — **null when the gateway omits it**
 * rather than defaulted, since a $0 price would render a free-model badge on a
 * paid model.
 */
export function mapModelInfo(payload: unknown): Record<string, unknown> {
    const out: Record<string, unknown> = {};
    const entries = (payload as { data?: unknown })?.data;
    if (!Array.isArray(entries)) return out;
    for (const entry of entries) {
        const name = (entry as { model_name?: unknown })?.model_name;
        if (typeof name !== 'string' || name === '') continue;
        const info = ((entry as { model_info?: unknown }).model_info ?? {}) as Record<string, unknown>;
        const num = (k: string) => (typeof info[k] === 'number' ? (info[k] as number) : null);
        out[name] = {
            inputCostPerToken: num('input_cost_per_token'),
            outputCostPerToken: num('output_cost_per_token'),
            tier: typeof info.model_tier === 'string' ? info.model_tier : null,
            useCases: Array.isArray(info.use_cases) ? info.use_cases : [],
            maxOutputTokens: num('max_output_tokens'),
        };
    }
    return out;
}

/** GET the gateway's `/model/info` with the server's configured credentials. */
async function fetchModelCosts(): Promise<Record<string, unknown>> {
    const base = (process.env.SMOOAI_GATEWAY_URL?.trim() || 'https://llm.smoo.ai/v1').replace(/\/+$/, '');
    const key = process.env.SMOOAI_GATEWAY_KEY?.trim();
    const res = await fetch(`${base}/model/info`, {
        headers: key ? { authorization: `Bearer ${key}` } : {},
        signal: AbortSignal.timeout(10_000),
    });
    if (!res.ok) throw new Error(`model/info: ${res.status}`);
    return mapModelInfo(await res.json());
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

    // Ungated too: gateway pricing is not org-sensitive and the console's cost
    // badges must render on a tokenless local connection. Any gateway failure
    // degrades to {} with a 200 — a missing badge beats a broken page.
    if (method === 'GET' && path === '/admin/model-costs') {
        if (modelCostsCache) {
            sendJson(res, 200, modelCostsCache);
            return;
        }
        try {
            modelCostsCache = await fetchModelCosts();
            sendJson(res, 200, modelCostsCache);
        } catch {
            sendJson(res, 200, {});
        }
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
        sendJson(res, 200, { runs: (await deps.stores.listRuns(p.org)).map(publicRow) });
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
            sendJson(res, 200, { connectors: (await deps.stores.listConnectors(p.org)).map(publicRow) });
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
            await deps.stores.putConnector(row);
            sendJson(res, 200, { connector: publicRow(row) });
            return;
        }
    }

    const indexMatch = /^\/admin\/connectors\/([^/]+)\/index$/.exec(path);
    if (method === 'POST' && indexMatch) {
        const p = requireRole(deps, req, res, ROLE_CURATOR);
        if (!p) return;
        const row = await ownedConnector(deps, decodeURIComponent(indexMatch[1] ?? ''), p.org, res);
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
        await deps.stores.recordRun(run);
        sendJson(res, 200, { run: publicRow(run) });
        return;
    }

    const connectorMatch = /^\/admin\/connectors\/([^/]+)$/.exec(path);
    if (connectorMatch) {
        const id = decodeURIComponent(connectorMatch[1] ?? '');
        if (method === 'GET') {
            const p = requireRole(deps, req, res, ROLE_CURATOR);
            if (!p) return;
            const row = await ownedConnector(deps, id, p.org, res);
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
            // ponytail: read-modify-write without a lock across the two calls. Concurrent
            // PUTs to the SAME connector are last-write-wins, which is what the durable
            // store's upsert does anyway; add row locking if a real conflict shows up.
            const row = await ownedConnector(deps, id, p.org, res);
            if (!row) return;
            Object.assign(row, write, { updatedAt: new Date().toISOString() });
            await deps.stores.putConnector(row);
            sendJson(res, 200, { connector: publicRow(row) });
            return;
        }
        if (method === 'DELETE') {
            const p = requireRole(deps, req, res, ROLE_ADMIN);
            if (!p) return;
            if (!(await deps.stores.deleteConnector(p.org, id))) {
                // Unknown and cross-org are the same 404 — no existence oracle.
                sendError(res, 404, 'NOT_FOUND', 'connector not found');
                return;
            }
            res.writeHead(204);
            res.end();
            return;
        }
    }

    if (path === '/admin/settings') {
        if (method === 'GET') {
            const p = requireRole(deps, req, res, ROLE_CURATOR);
            if (!p) return;
            sendJson(res, 200, { settings: (await deps.stores.getSettings(p.org)) ?? defaultSettings(p.org) });
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
            await deps.stores.putSettings(settings);
            sendJson(res, 200, { settings });
            return;
        }
    }

    sendError(res, 404, 'NOT_FOUND', `no admin route for ${method} ${path}`);
}

/** An org-owned connector, or a 404. A cross-org id is deliberately indistinguishable from an unknown one. */
async function ownedConnector(deps: AdminDeps, id: string, orgId: string, res: ServerResponse): Promise<ConnectorConfig | undefined> {
    const row = await deps.stores.getConnector(orgId, id);
    if (!row) {
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
