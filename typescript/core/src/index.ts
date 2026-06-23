/**
 * @smooai/smooth-operator-core — a native, in-process agent engine for TypeScript.
 *
 * The Phase-0 TypeScript sibling of the Rust reference engine, the C# core, and
 * the Python core: an agentic tool-calling loop over any OpenAI-compatible chat
 * client, with in-memory knowledge grounding. See `docs/Architecture/TypeScript Core.md`.
 */

export { delegateTool, SmoothAgent } from './agent.js';
export type { AgentOptions, AgentRunResponse, ChatClientLike, Tool } from './agent.js';
export { Cast, Clearance, makeRole, RoleKind } from './cast.js';
export type { OperatorRole } from './cast.js';
export { InMemoryCheckpointStore } from './checkpoint.js';
export type { Checkpoint, CheckpointStore } from './checkpoint.js';
export { CostTracker, DEFAULT_PRICING } from './cost.js';
export type { CostBudget, ModelPricing, Usage } from './cost.js';
export { InMemoryKnowledge } from './knowledge.js';
export type { Knowledge, KnowledgeHit } from './knowledge.js';
export { InMemoryMemory } from './memory.js';
export type { Memory, MemoryEntry } from './memory.js';
export { LexicalReranker, NoopReranker } from './rerank.js';
export type { Reranker } from './rerank.js';
export { SmoothAgentThread } from './thread.js';
export { HashEmbedder, hashToken, VectorKnowledge } from './vector.js';
export type { Embedder } from './vector.js';
