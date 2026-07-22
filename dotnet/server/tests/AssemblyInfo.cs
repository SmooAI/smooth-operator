using Xunit;

// Some behavior in this assembly is driven by PROCESS-WIDE environment variables
// (SMOOTH_AGENT_PREAMBLE_MODEL, the SEP extension host's vars). xUnit runs test classes in parallel
// by default, so a test that sets one of those would leak it into an unrelated class's turn — e.g.
// a preamble call landing in another test's captured chat messages. Serialize the assembly: this
// suite runs in a couple of seconds, which is a cheap price for determinism.
[assembly: CollectionBehavior(DisableTestParallelization = true)]
