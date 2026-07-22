using Microsoft.Extensions.AI;
using Microsoft.Extensions.DependencyInjection;
using Microsoft.Extensions.DependencyInjection.Extensions;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server.AspNetCore;

/// <summary>DI wiring for the smooth-operator server.</summary>
public static class ServiceCollectionExtensions
{
    /// <summary>
    /// Register the server's session store. The host must also register an <see cref="IChatClient"/>
    /// (the model), and may register an <see cref="IAccessKnowledge"/> (for ACL-scoped RAG grounding —
    /// e.g. an <see cref="AclKnowledgeStore"/>, or a <see cref="StaticAccessKnowledge"/> wrapping a
    /// plain knowledge base) and a <see cref="TokenAccessResolver"/> (to authenticate connections).
    /// The frame dispatcher itself is built per-connection by the WebSocket host (it's bound to that
    /// connection's resolved <see cref="AccessContext"/>).
    /// </summary>
    public static IServiceCollection AddSmoothOperatorServer(this IServiceCollection services)
    {
        services.TryAddSingleton<ISessionStore, InMemorySessionStore>();

        // Host-callable seam to start a turn server-side (e.g. a webhook → "investigate this alert")
        // instead of from a client send_message frame. Resolves the SAME shared collaborators
        // BuildDispatcher hands the client path (persona via SMOOTH_PERSONA, the LLM workflow judge
        // defaulted when a resolver is present) so a server-initiated turn persists identically. HITL
        // and OTP are omitted — they are per-connection interactive concerns.
        services.TryAddSingleton<IServerInitiatedTurns>(sp =>
        {
            var persona = Environment.GetEnvironmentVariable("SMOOTH_PERSONA");
            var agentConfigResolver = sp.GetService<IAgentConfigResolver>();
            var judge = sp.GetService<IWorkflowJudge>()
                ?? (agentConfigResolver is not null ? new LlmWorkflowJudge(sp.GetRequiredService<IChatClient>()) : null);
            return new ServerInitiatedTurns(
                sp.GetRequiredService<IChatClient>(),
                sp.GetRequiredService<ISessionStore>(),
                sp.GetService<IAccessKnowledge>(),
                systemPrompt: string.IsNullOrEmpty(persona) ? null : persona,
                reranker: sp.GetService<IReranker>(),
                tools: sp.GetService<IReadOnlyList<AITool>>(),
                agentConfigResolver: agentConfigResolver,
                judge: judge,
                limits: sp.GetService<TurnLimits>());
        });

        return services;
    }
}
