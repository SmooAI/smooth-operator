using System.Text.Json.Nodes;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// A delivery target: an opaque id under a kind (<c>connection</c> / <c>session</c> / <c>user</c> /
/// <c>org</c> / <c>agent</c>). A record, so it works as a dictionary key by value.
/// </summary>
public sealed record Target(string Kind, string Id);

/// <summary>
/// The connection-registry seam: every connection attaches its outbound sink under a connection id,
/// so events published from anywhere — this process, or another pod once a Redis/NATS implementation
/// exists — can reach it, and detaches when its read loop exits. The C# analog of the Go
/// <c>Backplane</c> interface, the Python <c>Backplane</c> ABC and the Rust <c>Backplane</c> trait.
/// <para>
/// Deliberately synchronous: every operation is a dictionary access plus a channel write. The
/// TS/Python <c>attach</c>/<c>detach</c> are async only because those ecosystems default to it. A
/// cross-pod implementation that needs to await real I/O should change this interface rather than have
/// every caller pay for a Task today.
/// </para>
/// </summary>
public interface IBackplane
{
    /// <summary>
    /// Register a connection's outbound sink, reachable as <c>Target("connection", connectionId)</c>.
    /// <paramref name="sink"/> delivers an already-built event frame to the connection's writer.
    /// </summary>
    void Attach(string connectionId, Action<JsonObject> sink);

    /// <summary>
    /// Fan an event out to every connection associated with <paramref name="target"/>, returning how
    /// many sinks it reached. This count is what <c>POST /admin/publish</c> reports as
    /// <c>delivered</c>, so it must never claim a delivery that did not happen.
    /// </summary>
    int Publish(Target target, JsonObject @event);

    /// <summary>Remove a connection's sink and every association to it. ALWAYS run on teardown.</summary>
    void Detach(string connectionId);
}

/// <summary>
/// Single-process <see cref="IBackplane"/>: connection sinks plus a target → connections index. No
/// cross-pod fan-out — that is the Redis/NATS seam.
/// <para>
/// ponytail: <see cref="Publish"/> is already generic over all target kinds. Only
/// <c>Target("connection", …)</c> has entries today, so the other kinds resolve to zero connections
/// and return 0 — associating a session/user/org/agent with its connections is the fan-out work, and
/// it plugs in by seeding <see cref="_byTarget"/> without touching Publish.
/// </para>
/// Safe for concurrent use.
/// </summary>
public sealed class InMemoryBackplane : IBackplane
{
    private readonly object _gate = new();
    private readonly Dictionary<string, Action<JsonObject>> _sinks = new(StringComparer.Ordinal);
    private readonly Dictionary<Target, HashSet<string>> _byTarget = new();

    // Reverse index so Detach tears down every association without scanning all targets. One entry per
    // connection today; the fan-out work adds more per connection, which is what makes this worth it.
    private readonly Dictionary<string, HashSet<Target>> _byConnection = new(StringComparer.Ordinal);

    public void Attach(string connectionId, Action<JsonObject> sink)
    {
        lock (_gate)
        {
            _sinks[connectionId] = sink;
            Associate(connectionId, new Target("connection", connectionId));
        }
    }

    public int Publish(Target target, JsonObject @event)
    {
        List<Action<JsonObject>> sinks = [];
        lock (_gate)
        {
            // Snapshot under the lock, invoke OUTSIDE it: a host's sink is arbitrary code, and one bad
            // one held under the registry lock would deadlock every attach, detach and publish.
            if (_byTarget.TryGetValue(target, out var connections))
            {
                sinks.AddRange(connections.Select(id => _sinks.GetValueOrDefault(id)).OfType<Action<JsonObject>>());
            }
        }
        foreach (var sink in sinks)
        {
            sink(@event);
        }
        return sinks.Count;
    }

    public void Detach(string connectionId)
    {
        lock (_gate)
        {
            _sinks.Remove(connectionId);
            if (!_byConnection.Remove(connectionId, out var targets))
            {
                return;
            }
            foreach (var target in targets)
            {
                if (_byTarget.TryGetValue(target, out var connections) && connections.Remove(connectionId) && connections.Count == 0)
                {
                    _byTarget.Remove(target);
                }
            }
        }
    }

    /// <summary>Index both directions. Caller holds <see cref="_gate"/>.</summary>
    private void Associate(string connectionId, Target target)
    {
        (_byTarget.TryGetValue(target, out var connections) ? connections : _byTarget[target] = []).Add(connectionId);
        (_byConnection.TryGetValue(connectionId, out var targets) ? targets : _byConnection[connectionId] = []).Add(target);
    }

    /// <summary>Whether a connection currently has a sink. Used by tests to assert detach ran.</summary>
    public bool IsAttached(string connectionId)
    {
        lock (_gate)
        {
            return _sinks.ContainsKey(connectionId);
        }
    }

    /// <summary>How many connections are currently attached. Used by tests to assert detach ran.</summary>
    public int Count
    {
        get
        {
            lock (_gate)
            {
                return _sinks.Count;
            }
        }
    }
}
