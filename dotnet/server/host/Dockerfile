# Build from the repo root:  docker build -f dotnet/server/host/Dockerfile -t smooth-operator-server .
FROM mcr.microsoft.com/dotnet/sdk:8.0 AS build
WORKDIR /src
COPY dotnet/ ./dotnet/
RUN dotnet publish dotnet/server/host/SmooAI.SmoothOperator.Server.Host.csproj -c Release -o /app --no-self-contained

FROM mcr.microsoft.com/dotnet/aspnet:8.0
WORKDIR /app
COPY --from=build /app .
EXPOSE 8080
ENV ASPNETCORE_URLS=http://+:8080
# Configure via env: SMOOAI_GATEWAY_KEY, SMOOTH_DATABASE_URL, SMOOTH_AUTH_MODE,
# SMOOTH_JWT_HS256_SECRET, SMOOTH_GITHUB_REPOS, SMOOTH_GITHUB_TOKEN (see README).
ENTRYPOINT ["dotnet", "SmooAI.SmoothOperator.Server.Host.dll"]
