using System.Net;
using System.Text;
using SmooAI.SmoothOperator.Core;
using SmooAI.SmoothOperator.Server;

namespace SmooAI.SmoothOperator.Server.Tests;

/// <summary>
/// Slack connector tests — run against a fake HTTP handler so the connector logic (per-day grouping,
/// stable ids, the incremental <c>oldest</c> cursor, and user-name resolution) is exercised in CI
/// without hitting Slack. Timestamps below are real Unix seconds: 1609459200 = 2021-01-01 00:00:00Z,
/// 1609545600 = 2021-01-02 00:00:00Z.
/// </summary>
public class SlackConnectorTests
{
    private const string UsersJson = """
        {"ok":true,"members":[
          {"id":"U1","name":"alice","real_name":"Alice Anderson"},
          {"id":"U2","name":"bob","profile":{"display_name":"Bobby"}}
        ]}
        """;

    private const string ChannelsJson = """
        {"ok":true,"channels":[{"id":"C123","name":"general"}]}
        """;

    // Two messages on 2021-01-01 (00:00 and 03:00) + one on 2021-01-02 → two day-documents.
    private const string HistoryJson = """
        {"ok":true,"messages":[
          {"type":"message","user":"U2","text":"day two message","ts":"1609545600.000100"},
          {"type":"message","user":"U2","text":"second message","ts":"1609470000.000200"},
          {"type":"message","user":"U1","text":"happy new year","ts":"1609459200.000100"}
        ]}
        """;

    [Fact]
    public async Task Pull_GroupsPerChannelPerDay_WithStableIds_SourcePermalink_AndResolvedNames()
    {
        var connector = new SlackConnector(new HttpClient(new FakeHttpHandler(DefaultResponder)));

        var docs = await Collect(connector);

        // One document per channel per day.
        Assert.Equal(2, docs.Count);

        var jan1 = docs.Single(d => d.Id == "slack:C123:2021-01-01");
        var jan2 = docs.Single(d => d.Id == "slack:C123:2021-01-02");

        // Author names resolved from users.list: real_name wins, then profile.display_name.
        Assert.Contains("Alice Anderson: happy new year", jan1.Content);
        Assert.Contains("Bobby: second message", jan1.Content);
        Assert.Contains("Bobby: day two message", jan2.Content);

        // Source = permalink of the day's FIRST (earliest) message.
        Assert.Equal("https://acme.slack.com/archives/C123/p1609459200000100", jan1.Source);
        Assert.Equal("https://acme.slack.com/archives/C123/p1609545600000100", jan2.Source);

        // Per-channel ACL label on every document.
        Assert.Equal(new[] { "slack:channel:C123" }, jan1.Acl);
        Assert.Equal(new[] { "slack:channel:C123" }, jan2.Acl);
    }

    [Fact]
    public async Task Pull_UnknownUser_FallsBackToUserId()
    {
        var history = """
            {"ok":true,"messages":[{"type":"message","user":"UZZZ","text":"who am i","ts":"1609459200.000100"}]}
            """;
        var connector = new SlackConnector(new HttpClient(new FakeHttpHandler(Route(history))));

        var docs = await Collect(connector);

        Assert.Contains("UZZZ: who am i", docs.Single().Content);
    }

    [Fact]
    public async Task Pull_WithoutOldest_OmitsOldestParam()
    {
        var seen = new List<string>();
        var connector = new SlackConnector(new HttpClient(new FakeHttpHandler(DefaultResponder, seen)));

        await Collect(connector);

        var history = seen.Single(u => u.Contains("conversations.history"));
        Assert.DoesNotContain("oldest=", history);
    }

    [Fact]
    public async Task Pull_WithOldest_SendsOldestCursor_AsUnixSeconds()
    {
        var seen = new List<string>();
        // Incremental pull from 2021-01-02 00:00:00Z → oldest=1609545600.
        var oldest = DateTimeOffset.FromUnixTimeSeconds(1609545600);
        var connector = new SlackConnector(new HttpClient(new FakeHttpHandler(DefaultResponder, seen)), oldest);

        await Collect(connector);

        var history = seen.Single(u => u.Contains("conversations.history"));
        Assert.Contains("oldest=1609545600", history);
    }

    [Fact]
    public async Task Pull_SlackErrorResponse_FailsLoud()
    {
        // ok:false must throw rather than silently ingesting a partial workspace (cf. GitHub truncated tree).
        var connector = new SlackConnector(new HttpClient(new FakeHttpHandler(_ => Json("""{"ok":false,"error":"invalid_auth"}"""))));

        var ex = await Assert.ThrowsAsync<InvalidOperationException>(() => Collect(connector));
        Assert.Contains("invalid_auth", ex.Message);
    }

    private static async Task<List<SourceDocument>> Collect(SlackConnector connector)
    {
        var docs = new List<SourceDocument>();
        await foreach (var doc in connector.PullAsync())
        {
            docs.Add(doc);
        }
        return docs;
    }

    private static HttpResponseMessage DefaultResponder(HttpRequestMessage request) => Route(HistoryJson)(request);

    private static Func<HttpRequestMessage, HttpResponseMessage> Route(string historyJson) => request =>
    {
        var url = request.RequestUri!.ToString();
        if (url.Contains("users.list"))
        {
            return Json(UsersJson);
        }
        if (url.Contains("conversations.list"))
        {
            return Json(ChannelsJson);
        }
        if (url.Contains("conversations.history"))
        {
            return Json(historyJson);
        }
        if (url.Contains("chat.getPermalink"))
        {
            // Slack permalinks encode the ts as p{ts-without-dot}. Pull message_ts straight from the query.
            var marker = "message_ts=";
            var start = url.IndexOf(marker, StringComparison.Ordinal) + marker.Length;
            var end = url.IndexOf('&', start);
            var ts = (end < 0 ? url[start..] : url[start..end]).Replace(".", "");
            return Json($$"""{"ok":true,"permalink":"https://acme.slack.com/archives/C123/p{{ts}}"}""");
        }
        return new HttpResponseMessage(HttpStatusCode.NotFound);
    };

    private static HttpResponseMessage Json(string body) =>
        new(HttpStatusCode.OK) { Content = new StringContent(body, Encoding.UTF8, "application/json") };

    private sealed class FakeHttpHandler : HttpMessageHandler
    {
        private readonly Func<HttpRequestMessage, HttpResponseMessage> _responder;
        private readonly List<string>? _seen;

        public FakeHttpHandler(Func<HttpRequestMessage, HttpResponseMessage> responder, List<string>? seen = null)
        {
            _responder = responder;
            _seen = seen;
        }

        protected override Task<HttpResponseMessage> SendAsync(HttpRequestMessage request, CancellationToken cancellationToken)
        {
            _seen?.Add(request.RequestUri!.ToString());
            return Task.FromResult(_responder(request));
        }
    }
}
