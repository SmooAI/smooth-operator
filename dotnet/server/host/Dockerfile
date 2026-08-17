# Build from the repo root:  docker build -f dotnet/server/host/Dockerfile -t smooth-operator-server .
FROM mcr.microsoft.com/dotnet/sdk:8.0 AS build
WORKDIR /src
COPY dotnet/ ./dotnet/
RUN dotnet publish dotnet/server/host/SmooAI.SmoothOperator.Server.Host.csproj -c Release -o /app --no-self-contained

FROM mcr.microsoft.com/dotnet/aspnet:8.0
WORKDIR /app
COPY --from=build /app .
# Bind + port (both overridable), the canonical names every server implementation
# reads (see dotnet/server/src/ServerEnv.cs). This host used to have no bind env of
# its own and took ASP.NET's :5000 default; it now serves 8787 like its four
# siblings. The PROCESS default host is 127.0.0.1, unreachable from outside a
# container — so the IMAGE defaults it to 0.0.0.0 (translated to ASP.NET's `+`).
ENV SMOOTH_AGENT_BIND=0.0.0.0 \
    SMOOTH_AGENT_PORT=8787
EXPOSE 8787
# Configure via env: SMOOAI_GATEWAY_KEY, SMOOTH_AGENT_DATABASE_URL,
# SMOOTH_AGENT_AUTH_MODE, SMOOTH_JWT_HS256_SECRET, SMOOTH_GITHUB_REPOS,
# SMOOTH_GITHUB_TOKEN (see README).
ENTRYPOINT ["dotnet", "SmooAI.SmoothOperator.Server.Host.dll"]
