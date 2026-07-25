using Xunit;

// The host reads PROCESS-WIDE environment variables at boot (SMOOTH_NO_TOOLS, SMOOTH_WORKSPACE, …),
// and xUnit runs test classes in parallel by default — a test that sets one would leak it into
// another class's freshly booted host. Serialize the assembly; it runs in seconds either way.
[assembly: CollectionBehavior(DisableTestParallelization = true)]
