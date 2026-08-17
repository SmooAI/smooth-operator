using System.Text.Json.Nodes;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// The connection-registry seam: every connection attaches its outbound sink under a connection id,
/// so events published from anywhere — this process, or another pod once a Redis/NATS implementation
/// exists — can reach it, and detaches when its read loop exits. The C# analog of the Go
/// <c>Backplane</c> interface, the TypeScript <c>Backplane</c> and the Rust <c>Backplane</c> trait.
/// <para>
/// Deliberately synchronous, matching Go's shape and TypeScript's <c>publish</c>: every operation is
/// a dictionary access plus a channel write. The TS/Python <c>attach</c>/<c>detach</c> are async only
/// because those ecosystems default to it, and a <c>Task</c> around a <c>Dictionary</c> insert buys
/// nothing here. A cross-pod implementation that genuinely needs to await I/O should change this
/// interface rather than have every caller pay for a Task today.
/// </para>
/// </summary>
public interface IBackplane
{
    /// <summary>
    /// Register a connection's outbound sink. <paramref name="sink"/> delivers an already-built event
    /// frame to the connection's writer.
    /// </summary>
    void Attach(string connectionId, Action<JsonObject> sink);

    /// <summary>
    /// Fan an event out to a connection's attached sink, returning how many sinks it reached: 1 when
    /// the connection is attached, 0 when it is not. This count is what <c>POST /admin/publish</c>
    /// reports as <c>delivered</c>, so it must never claim a delivery that did not happen.
    /// </summary>
    int Publish(string connectionId, JsonObject @event);

    /// <summary>Remove a connection's sink. ALWAYS run on connection teardown.</summary>
    void Detach(string connectionId);
}

/// <summary>
/// Single-process <see cref="IBackplane"/>: a connection-id → sink map. No cross-pod fan-out — that
/// is the Redis/NATS seam, and it is why <c>POST /admin/publish</c> answers 501 for session / user /
/// org / agent targets, which a connection-id registry cannot route. Safe for concurrent use.
/// </summary>
public sealed class InMemoryBackplane : IBackplane
{
    private readonly object _gate = new();
    private readonly Dictionary<string, Action<JsonObject>> _sinks = new(StringComparer.Ordinal);

    public void Attach(string connectionId, Action<JsonObject> sink)
    {
        lock (_gate)
        {
            _sinks[connectionId] = sink;
        }
    }

    public int Publish(string connectionId, JsonObject @event)
    {
        Action<JsonObject>? sink;
        lock (_gate)
        {
            // Read under the lock but invoke OUTSIDE it: a sink writes to the connection's channel,
            // and holding the registry lock across that would let one slow connection block every
            // other attach, detach and publish in the process.
            _sinks.TryGetValue(connectionId, out sink);
        }
        if (sink is null)
        {
            return 0;
        }
        sink(@event);
        return 1;
    }

    public void Detach(string connectionId)
    {
        lock (_gate)
        {
            _sinks.Remove(connectionId);
        }
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
