#!/usr/bin/env bash
#
# Codegen: emit `go/protocol/types_gen.go` from the language-neutral JSON Schemas
# in `spec/`.
#
# Uses go-jsonschema (pure Go, offline). Notes on the flags:
#
#   --only-models   plain structs, no generated enum validation, so the client
#                   tolerates forward-compatible wire values and the conformance
#                   fixtures round-trip cleanly.
#   -t              take the struct name from each schema's `title`. Every `$def`
#                   in the spec carries a stable title (`GetMessagesRequest`, …),
#                   so feeding all files at once does NOT collide on the shared
#                   `$defs/Request` / `$defs/Response` keys.
#   --tags json     the wire format is JSON only; yaml/mapstructure tags are noise.
#   --capitalization  Go initialisms. Without these you get Id/Url/Otp/Json.
#
# Schema set matches the TypeScript generator (typescript/scripts/generate.ts):
# the root envelope plus actions/, events/, domain/, interactions/. The
# extension/ (SEP) and conformance/ trees are deliberately excluded — they are
# not part of the client wire protocol.
#
# Usage: scripts/generate-go.sh
set -euo pipefail
shopt -s nullglob

cd "$(dirname "$0")/.."

BIN="${GO_JSONSCHEMA:-$(command -v go-jsonschema || echo "$HOME/go/bin/go-jsonschema")}"
if [[ ! -x "$BIN" ]]; then
	echo "go-jsonschema not found. Install it with:" >&2
	echo "  go install github.com/atombender/go-jsonschema@latest" >&2
	exit 1
fi

"$BIN" \
	--only-models \
	--struct-name-from-title \
	--tags json \
	--capitalization ID \
	--capitalization URL \
	--capitalization OTP \
	--capitalization JSON \
	-p protocol \
	-o go/protocol/types_gen.go \
	spec/envelope.schema.json \
	spec/actions/*.schema.json \
	spec/events/*.schema.json \
	spec/domain/*.schema.json \
	spec/interactions/*.schema.json

gofmt -w go/protocol/types_gen.go
echo "Wrote go/protocol/types_gen.go"
