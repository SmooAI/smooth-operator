/**
 * @smooai/smooth-operator-core — a native, in-process agent engine for TypeScript.
 *
 * The Phase-0 TypeScript sibling of the Rust reference engine, the C# core, and
 * the Python core: an agentic tool-calling loop over any OpenAI-compatible chat
 * client, with in-memory knowledge grounding. See `docs/Architecture/TypeScript Core.md`.
 */

export { SmoothAgent } from './agent.js';
export type { AgentOptions, AgentRunResponse, ChatClientLike, Tool } from './agent.js';
export { InMemoryCheckpointStore } from './checkpoint.js';
export type { Checkpoint, CheckpointStore } from './checkpoint.js';
export { CostTracker, DEFAULT_PRICING } from './cost.js';
export type { CostBudget, ModelPricing, Usage } from './cost.js';
export { InMemoryKnowledge } from './knowledge.js';
export type { KnowledgeHit } from './knowledge.js';
