using System.Text.Json.Nodes;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Backplane fan-out — ports the Rust reference's <c>backplane.rs</c> unit tests.
///
/// <para>The point is that a routed target ACTUALLY RECEIVES: a registry that accepts an
/// association but delivers nothing would pass a shape-only test while being useless, and
/// <c>delivered</c> would be a lying 0.</para>
/// </summary>
public class BackplaneFanoutTests
{
    private static (Action<JsonObject> Sink, List<JsonObject> Got) Collector()
    {
        var got = new List<JsonObject>();
        return (got.Add, got);
    }

    [Fact]
    public void PublishesToASessionAcrossItsConnections()
    {
        var bp = new InMemoryBackplane();
        var (sinkA, gotA) = Collector();
        var (sinkB, gotB) = Collector();
        bp.Attach("conn-a", sinkA);
        bp.Attach("conn-b", sinkB);
        bp.Associate("conn-a", new Target("session", "s1"));
        bp.Associate("conn-b", new Target("session", "s1"));

        Assert.Equal(2, bp.Publish(new Target("session", "s1"), new JsonObject { ["hi"] = 1 }));
        Assert.Equal(1, gotA.Single()["hi"]!.GetValue<int>());
        Assert.Equal(1, gotB.Single()["hi"]!.GetValue<int>());
    }

    [Fact]
    public void UnknownTargetDeliversToNobody()
    {
        Assert.Equal(0, new InMemoryBackplane().Publish(new Target("session", "nope"), new JsonObject()));
    }

    [Fact]
    public void DetachRemovesEveryAssociationNotJustTheSink()
    {
        var bp = new InMemoryBackplane();
        var (sink, _) = Collector();
        bp.Attach("conn-x", sink);
        bp.Associate("conn-x", new Target("user", "u1"));

        bp.Detach("conn-x");
        Assert.False(bp.IsAttached("conn-x"));
        // A leaked association would resolve to a dead socket and inflate `delivered` forever.
        Assert.Equal(0, bp.Publish(new Target("user", "u1"), new JsonObject()));
        Assert.Equal(0, bp.Publish(new Target("connection", "conn-x"), new JsonObject()));
    }

    [Fact]
    public void AConnectionCanServeMultipleTargets()
    {
        var bp = new InMemoryBackplane();
        var (sink, got) = Collector();
        bp.Attach("c", sink);
        bp.Associate("c", new Target("session", "s"));
        bp.Associate("c", new Target("org", "o"));

        Assert.Equal(1, bp.Publish(new Target("org", "o"), new JsonObject { ["e"] = "org" }));
        Assert.Equal(1, bp.Publish(new Target("session", "s"), new JsonObject { ["e"] = "sess" }));
        Assert.Equal(["org", "sess"], got.Select(g => g["e"]!.GetValue<string>()).ToArray());
    }

    [Fact]
    public void AssociateIsIdempotent()
    {
        // Re-resolving the same session on the same connection must not double-count deliveries —
        // ScopedSessionAsync associates on EVERY sessionId-bearing frame, so this path is hot.
        var bp = new InMemoryBackplane();
        var (sink, got) = Collector();
        bp.Attach("c", sink);
        bp.Associate("c", new Target("session", "s"));
        bp.Associate("c", new Target("session", "s"));

        Assert.Equal(1, bp.Publish(new Target("session", "s"), new JsonObject()));
        Assert.Single(got);
    }

    [Fact]
    public void EachSinkGetsItsOwnCopyOfTheEvent()
    {
        // Fan-out means one event reaches MANY sinks. JsonObject is mutable, so a shared instance
        // would let one connection's sink corrupt every other connection's frame.
        var bp = new InMemoryBackplane();
        var (sinkA, gotA) = Collector();
        var (sinkB, gotB) = Collector();
        bp.Attach("a", sinkA);
        bp.Attach("b", sinkB);
        bp.Associate("a", new Target("org", "o"));
        bp.Associate("b", new Target("org", "o"));

        Assert.Equal(2, bp.Publish(new Target("org", "o"), new JsonObject { ["n"] = 1 }));
        Assert.NotSame(gotA.Single(), gotB.Single());

        // Mutating one delivery leaves the other intact.
        gotA.Single()["n"] = 99;
        Assert.Equal(1, gotB.Single()["n"]!.GetValue<int>());
    }
}
