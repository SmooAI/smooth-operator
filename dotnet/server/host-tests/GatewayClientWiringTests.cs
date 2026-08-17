using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.Extensions.AI;
using Microsoft.Extensions.DependencyInjection;
using SmooAI.SmoothOperator.Core;

namespace SmooAI.SmoothOperator.Server.Host.Tests;

/// <summary>
/// The host injects a chat client that can SEE the gateway cost header.
///
/// <see cref="HostBootTests"/> overrides <see cref="IChatClient"/> with a mock, so it cannot catch
/// this: the host could go back to the MEAI OpenAI adapter — whose parsed response drops HTTP
/// headers, which is why every turn reported <c>costUsd: 0</c> — and every other test would stay
/// green. This resolves the REAL registration and fails if it does.
/// </summary>
public class GatewayClientWiringTests : IClassFixture<WebApplicationFactory<Program>>
{
    private readonly WebApplicationFactory<Program> _factory;

    public GatewayClientWiringTests(WebApplicationFactory<Program> factory) => _factory = factory;

    [Fact]
    public void HostRegistersCoresHeaderReadingGatewayClient()
    {
        using var scope = _factory.Services.CreateScope();

        var client = scope.ServiceProvider.GetRequiredService<IChatClient>();

        Assert.IsType<GatewayChatClient>(client);
    }
}
