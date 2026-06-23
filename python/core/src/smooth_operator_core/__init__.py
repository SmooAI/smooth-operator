"""smooth-operator-core (Python): a native, in-process agent engine.

The Phase-0 Python sibling of the Rust reference engine and the C# core — an
agentic tool-calling loop over any OpenAI-compatible chat client, with in-memory
knowledge grounding. See ``docs/Architecture/Python Core.md``.
"""

from .agent import AgentOptions, AgentRunResponse, FunctionTool, SmoothAgent, Tool, delegate_tool
from .cast import Cast, Clearance, OperatorRole, RoleKind
from .checkpoint import Checkpoint, CheckpointStore, InMemoryCheckpointStore
from .cost import CostBudget, CostTracker, ModelPricing, Usage
from .knowledge import InMemoryKnowledge, Knowledge, KnowledgeHit
from .memory import InMemoryMemory, Memory, MemoryEntry
from .rerank import LexicalReranker, NoopReranker, Reranker
from .thread import SmoothAgentThread
from .vector import Embedder, HashEmbedder, VectorKnowledge

__all__ = [
    "AgentOptions",
    "AgentRunResponse",
    "Cast",
    "Checkpoint",
    "CheckpointStore",
    "Clearance",
    "CostBudget",
    "CostTracker",
    "Embedder",
    "FunctionTool",
    "delegate_tool",
    "HashEmbedder",
    "InMemoryCheckpointStore",
    "InMemoryKnowledge",
    "InMemoryMemory",
    "Knowledge",
    "KnowledgeHit",
    "LexicalReranker",
    "Memory",
    "MemoryEntry",
    "ModelPricing",
    "NoopReranker",
    "OperatorRole",
    "Reranker",
    "RoleKind",
    "SmoothAgent",
    "SmoothAgentThread",
    "Tool",
    "Usage",
    "VectorKnowledge",
]
