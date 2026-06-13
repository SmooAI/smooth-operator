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
        var resolver = services.GetService<TokenAccessResolver>();
        var token = context.Request.Query["token"].FirstOrDefault();
        var access = resolver?.Resolve(token) ?? AccessContext.Anonymous;

        return new FrameDispatcher(
            services.GetRequiredService<ISessionStore>(),
            services.GetRequiredService<IChatClient>(),
            services.GetService<IAccessKnowledge>(),
            access,
            reranker: services.GetService<IReranker>()); // null unless the host registered one (rerank is opt-in)
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
