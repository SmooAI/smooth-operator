using System.Text.Json.Nodes;

namespace SmooAI.SmoothOperator.Server;

/// <summary>
/// Type-tolerant readers for untrusted JSON (inbound protocol frames, model-produced tool arguments,
/// host-supplied agent config).
///
/// <para><see cref="JsonNode.GetValue{T}()"/> <b>throws</b> <see cref="InvalidOperationException"/> on a
/// type mismatch ("An element of type 'Number' cannot be converted to a 'System.String'"). That is not
/// an <see cref="OperationCanceledException"/> nor a <c>WebSocketException</c>, so a read done OUTSIDE
/// the dispatch <c>try</c> escapes the connection pump entirely and drops the socket with no error event
/// — a client sending <c>{"action":123}</c> kills the connection. Reads done INSIDE it degrade to a
/// generic <c>INTERNAL_ERROR</c> where the client deserves a <c>VALIDATION_ERROR</c>.</para>
///
/// <para>These readers return <c>null</c> for missing OR wrong-typed, mirroring the Rust reference's
/// <c>Value::as_str()</c> / <c>as_bool()</c> (<c>frame_action_and_request_id</c>, <c>handler.rs</c>), so
/// every protocol-level failure is surfaced as an <c>error</c> event and never as a hard error that
/// drops the connection. th-acf8ea.</para>
/// </summary>
internal static class JsonFrame
{
    /// <summary>The node as a string, or null when absent or not a JSON string.</summary>
    public static string? Str(this JsonNode? node) =>
        node is JsonValue value && value.TryGetValue<string>(out var s) ? s : null;

    /// <summary>The node as a bool, or null when absent or not a JSON boolean.</summary>
    public static bool? Bool(this JsonNode? node) =>
        node is JsonValue value && value.TryGetValue<bool>(out var b) ? b : null;
}
