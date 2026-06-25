using System.Collections.Concurrent;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Tracks the in-flight write-confirmation each parked turn is waiting on.
///
/// When an agent turn calls a tool that requires human approval, the turn <b>parks</b> inside the
/// engine's <c>IHumanGate</c> and the runner registers a resolver here, keyed by <c>sessionId</c>.
/// A subsequent <c>confirm_tool_action</c> frame on the same connection looks the session up,
/// resolves the verdict, and the parked turn resumes (runs the tool on approve; skips it with a
/// rejection result on deny).
///
/// The C# analog of the Python <c>ConfirmationRegistry</c> and the Rust <c>AppState</c>
/// pending-confirmation map (register / take / clear). Keyed by session so each session has at most
/// one outstanding confirmation; an empty registry means no turn is parked (the default — behavior
/// identical to before HITL). One registry per connection (a <c>confirm_tool_action</c> frame and
/// the parked turn it resumes are always on the same connection).
/// </summary>
public sealed class ConfirmationRegistry
{
    // sessionId → the TaskCompletionSource a parked turn awaits. true = approved, false = rejected.
    // At most one per session. ConcurrentDictionary because the parked turn (a background Task) and
    // the read loop resolving the confirmation run on different threads.
    private readonly ConcurrentDictionary<string, TaskCompletionSource<bool>> _pending = new();

    /// <summary>
    /// Register (and return) a fresh approval task for <paramref name="sessionId"/>. Any prior
    /// pending confirmation for the session is rejected (resolved <c>false</c>) first, so a stale
    /// parked turn can never be left dangling and the newest confirmation always wins — mirrors the
    /// Python <c>register</c> / Rust <c>register_confirmation</c> taking over a prior sender.
    /// </summary>
    public Task<bool> Register(string sessionId)
    {
        // RunContinuationsAsynchronously: the awaiting turn's continuation must not run inline on the
        // thread that calls Resolve (the read loop) — keep the read loop free.
        var tcs = new TaskCompletionSource<bool>(TaskCreationOptions.RunContinuationsAsynchronously);
        if (_pending.TryRemove(sessionId, out var prior))
        {
            prior.TrySetResult(false);
        }
        _pending[sessionId] = tcs;
        return tcs.Task;
    }

    /// <summary>
    /// Resolve the parked turn for <paramref name="sessionId"/> with the verdict. Returns
    /// <c>true</c> if a pending confirmation was resolved, <c>false</c> if none was awaiting (a
    /// duplicate / stale <c>confirm_tool_action</c> → <c>NO_PENDING_CONFIRMATION</c>). Taking the
    /// task out makes a duplicate confirm a clean no-op (mirrors the Rust <c>take_confirmation</c>).
    /// </summary>
    public bool Resolve(string sessionId, bool approved)
    {
        if (!_pending.TryRemove(sessionId, out var tcs))
        {
            return false;
        }
        return tcs.TrySetResult(approved);
    }

    /// <summary>
    /// Drop any registered confirmation for <paramref name="sessionId"/> (turn ended), so a stale
    /// entry can't mis-route a later confirmation. Idempotent.
    /// </summary>
    public void Clear(string sessionId) => _pending.TryRemove(sessionId, out _);

    /// <summary>
    /// Resolve every outstanding confirmation as <b>rejected</b> (deny). Called when a connection is
    /// torn down (close / graceful drain) so any turn parked on a confirmation unparks and finishes
    /// cleanly — fail closed (a write is never auto-approved on disconnect) and never leave a turn
    /// hung forever.
    /// </summary>
    public void RejectAll()
    {
        foreach (var key in _pending.Keys.ToArray())
        {
            if (_pending.TryRemove(key, out var tcs))
            {
                tcs.TrySetResult(false);
            }
        }
    }
}
