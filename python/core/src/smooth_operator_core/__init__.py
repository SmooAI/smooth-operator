"""smooth-operator-core (Python): a native, in-process agent engine.

The Phase-0 Python sibling of the Rust reference engine and the C# core — an
agentic tool-calling loop over any OpenAI-compatible chat client, with in-memory
knowledge grounding. See ``docs/Architecture/Python Core.md``.
"""

from .agent import AgentOptions, AgentRunResponse, FunctionTool, SmoothAgent, Tool
from .checkpoint import Checkpoint, CheckpointStore, InMemoryCheckpointStore
from .cost import CostBudget, CostTracker, ModelPricing, Usage
from .knowledge import InMemoryKnowledge, KnowledgeHit

__all__ = [
    "AgentOptions",
    "AgentRunResponse",
    "Checkpoint",
    "CheckpointStore",
    "CostBudget",
    "CostTracker",
    "FunctionTool",
    "InMemoryCheckpointStore",
    "InMemoryKnowledge",
    "KnowledgeHit",
    "ModelPricing",
    "SmoothAgent",
    "Tool",
    "Usage",
]
