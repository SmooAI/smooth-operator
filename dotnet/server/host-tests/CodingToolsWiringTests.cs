using Microsoft.AspNetCore.Mvc.Testing;
using Microsoft.AspNetCore.TestHost;
using Microsoft.Extensions.AI;
using Microsoft.Extensions.DependencyInjection;

namespace SmooAI.SmoothOperator.Server.Host.Tests;

/// <summary>
/// The host must hand the agent a coding toolset by default (th-82ad57) — the registration the
/// FrameDispatcher reads tools from is <c>IReadOnlyList&lt;AITool&gt;</c>, so this asserts the
/// booted host resolves the six shared tools. Mirrors the Go host's env contract
/// (<c>SMOOTH_NO_TOOLS</c> opts out, <c>SMOOTH_WORKSPACE</c> sets the root).
/// </summary>
public class CodingToolsWiringTests
{
    private static WebApplicationFactory<Program> Factory() =>
        new WebApplicationFactory<Program>().WithWebHostBuilder(builder =>
            builder.ConfigureTestServices(services => services.AddSingleton<IChatClient>(new MockChatClient().PushText("ok"))));

    [Fact]
    public void Host_RegistersCodingToolsByDefault()
    {
        using var factory = Factory();
        var tools = factory.Services.GetService<IReadOnlyList<AITool>>();
        Assert.NotNull(tools);
        Assert.Equal(
            new[] { "bash", "edit_file", "grep", "list_files", "read_file", "write_file" },
            tools!.Select(t => t.Name).Order(StringComparer.Ordinal));
    }

    [Fact]
    public void Host_SmoothNoTools_ServesChatOnly()
    {
        Environment.SetEnvironmentVariable("SMOOTH_NO_TOOLS", "1");
        try
        {
            using var factory = Factory();
            Assert.Null(factory.Services.GetService<IReadOnlyList<AITool>>());
        }
        finally
        {
            Environment.SetEnvironmentVariable("SMOOTH_NO_TOOLS", null);
        }
    }

    [Fact]
    public async Task Host_ConfinesToolsToSmoothWorkspace()
    {
        var workspace = Directory.CreateTempSubdirectory("smooth-host-ws").FullName;
        Environment.SetEnvironmentVariable("SMOOTH_WORKSPACE", workspace);
        try
        {
            using var factory = Factory();
            var write = (AIFunction)factory.Services.GetRequiredService<IReadOnlyList<AITool>>().First(t => t.Name == "write_file");
            await write.InvokeAsync(new AIFunctionArguments { ["path"] = "wired.txt", ["content"] = "yes" });
            Assert.Equal("yes", File.ReadAllText(Path.Combine(workspace, "wired.txt")));
        }
        finally
        {
            Environment.SetEnvironmentVariable("SMOOTH_WORKSPACE", null);
            Directory.Delete(workspace, recursive: true);
        }
    }
}
