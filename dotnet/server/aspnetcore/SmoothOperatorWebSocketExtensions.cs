using System.Net.WebSockets;
using System.Text;
using System.Text.Json.Nodes;
using System.Threading.Channels;
using Microsoft.AspNetCore.Builder;
using Microsoft.AspNetCore.Http;
using Microsoft.Extensions.AI;
using Microsoft.Extensions.DependencyInjection;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server.AspNetCore;

/// <summary>
/// DI carrier for the write-confirmation HITL tool-name patterns — the C# analog of Python's
/// <c>ServerState.confirm_tools</c>. Register one (<c>AddSingleton(new ConfirmTools("delete_record"))</c>)
/// to gate matching tools behind a <c>confirm_tool_action</c> round-trip. Absent ⇒ HITL off.
/// A distinct wrapper type (not a bare <c>IReadOnlyList&lt;string&gt;</c>) so it can't collide with
/// other string lists in the container.
/// </summary>
public sealed record ConfirmTools(IReadOnlyList<string> Patterns)
{
    public ConfirmTools(params string[] patterns) : this((IReadOnlyList<string>)patterns) { }
}

/// <summary>
/// Maps the smooth-operator protocol onto a WebSocket endpoint — the deployable surface of the
/// C# service, and the analog of the Rust server's axum <c>/ws</c> upgrade + connection loop.
/// </summary>
public static class SmoothOperatorWebSocketExtensions
{
    /// <summary>
    /// Map the protocol onto <paramref name="path"/> (default <c>/ws</c>). Each connection reads
    /// frames, dispatches them via a <see cref="FrameDispatcher"/> (resolved from DI by default),
    /// and writes the resulting events back over the socket.
    /// </summary>
    public static WebApplication MapSmoothOperatorWebSocket(this WebApplication app, string path = "/ws", Func<HttpContext, FrameDispatcher>? dispatcherFor = null)
    {
        app.UseWebSockets();
        dispatcherFor ??= BuildDispatcher;

        app.Map(path, async (HttpContext context) =>
        {
            if (!context.WebSockets.IsWebSocketRequest)
            {
                context.Response.StatusCode = StatusCodes.Status400BadRequest;
                return;
            }

            using var socket = await context.WebSockets.AcceptWebSocketAsync();
            await PumpAsync(socket, dispatcherFor(context), context.RequestAborted).ConfigureAwait(false);
        });

        return app;
    }

    /// <summary>
    /// Build a per-connection dispatcher: resolve the <c>?token=</c> slot into an
    /// <see cref="AccessContext"/> (browsers can't set WebSocket headers), then bind the dispatcher
    /// to it so retrieval is ACL-scoped to this connection. Shared services come from DI.
    /// </summary>
    private static FrameDispatcher BuildDispatcher(HttpContext context)
    {
        var services = context.RequestServices;
        // Resolve through the configured auth-verifier seam (IAuthVerifier): a host that registered a
        // verifier (NoAuthVerifier / LocalTokenVerifier / a TokenAccessResolver, which also implements
        // IAuthVerifier) drives resolution; absent one, default to NoAuthVerifier — every connection
        // anonymous (org-public), the unchanged default. Fail-closed to anonymous on any token problem.
        var verifier = services.GetService<IAuthVerifier>()
            ?? (IAuthVerifier?)services.GetService<TokenAccessResolver>()
            ?? NoAuthVerifier.Instance;
        var token = context.Request.Query["token"].FirstOrDefault();
        var access = verifier.Resolve(token);

        return new FrameDispatcher(
            services.GetRequiredService<ISessionStore>(),
            services.GetRequiredService<IChatClient>(),
            services.GetService<IAccessKnowledge>(),
            access,
            reranker: services.GetService<IReranker>(), // null unless the host registered one (rerank is opt-in)
            tools: services.GetService<IReadOnlyList<AITool>>(), // the tools the agent may call (default none — the DI analog of Python's ServerState.tools)
            // Tool-name patterns gated behind write-confirmation HITL (default none — the DI analog of
            // Python's ServerState.confirm_tools). Each connection gets its own ConfirmationRegistry
            // (a confirm_tool_action frame and the parked turn it resumes are always on the same one).
            confirmTools: services.GetService<ConfirmTools>()?.Patterns);
    }

    private static async Task PumpAsync(WebSocket socket, FrameDispatcher dispatcher, CancellationToken cancellationToken)
    {
        // Outbound events go through a channel to a SINGLE writer task — WebSocket.SendAsync isn't
        // safe to call concurrently. Mirrors the Rust server's sink_tx + writer split.
        var channel = Channel.CreateUnbounded<JsonObject>(new UnboundedChannelOptions { SingleReader = true });

        var writer = Task.Run(async () =>
        {
            try
            {
                await foreach (var ev in channel.Reader.ReadAllAsync(cancellationToken).ConfigureAwait(false))
                {
                    var bytes = Encoding.UTF8.GetBytes(ev.ToJsonString());
                    await socket.SendAsync(bytes, WebSocketMessageType.Text, endOfMessage: true, cancellationToken).ConfigureAwait(false);
                }
            }
            catch (OperationCanceledException)
            {
            }
        }, cancellationToken);

        try
        {
            while (socket.State == WebSocketState.Open && !cancellationToken.IsCancellationRequested)
            {
                var frame = await ReceiveTextAsync(socket, cancellationToken).ConfigureAwait(false);
                if (frame is null)
                {
                    break; // close frame
                }
                await dispatcher.DispatchAsync(frame, ev => channel.Writer.TryWrite(ev), cancellationToken).ConfigureAwait(false);
            }
        }
        catch (OperationCanceledException)
        {
        }
        catch (WebSocketException)
        {
        }
        finally
        {
            // Any turn parked on a write-confirmation must unpark before we can finish: reject
            // outstanding confirmations (fail closed — a write is never auto-approved on disconnect),
            // then await every in-flight spawned turn so its eventual_response is enqueued before the
            // writer stops (preserves the graceful-drain "in-flight turn finishes" contract now that
            // turns run as background tasks rather than inline). No-op when HITL is off and no turn
            // is in flight.
            dispatcher.RejectPendingConfirmations();
            await dispatcher.WaitForTurnsAsync().ConfigureAwait(false);

            channel.Writer.TryComplete();
            try
            {
                await writer.ConfigureAwait(false);
            }
            catch
            {
                // writer cancellation on connection teardown
            }

            // Send a close (one-way; don't wait for the peer's ack — the client may already be
            // waiting in its own CloseAsync, which a blocking server CloseAsync would deadlock).
            if (socket.State is WebSocketState.Open or WebSocketState.CloseReceived)
            {
                try
                {
                    await socket.CloseOutputAsync(WebSocketCloseStatus.NormalClosure, "bye", CancellationToken.None).ConfigureAwait(false);
                }
                catch
                {
                    // socket already gone
                }
            }
        }
    }

    private static async Task<string?> ReceiveTextAsync(WebSocket socket, CancellationToken cancellationToken)
    {
        var buffer = new byte[16 * 1024];
        using var stream = new MemoryStream();
        WebSocketReceiveResult result;
        do
        {
            result = await socket.ReceiveAsync(buffer, cancellationToken).ConfigureAwait(false);
            if (result.MessageType == WebSocketMessageType.Close)
            {
                return null;
            }
            stream.Write(buffer, 0, result.Count);
        }
        while (!result.EndOfMessage);

        return Encoding.UTF8.GetString(stream.ToArray());
    }
}
