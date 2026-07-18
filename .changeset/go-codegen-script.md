---
'@smooai/smooth-operator': patch
---

Go: commit the type-generation command as `scripts/generate-go.sh` and regenerate `go/protocol/types_gen.go`.

The command that produced `go/protocol/types_gen.go` was never committed — `go/README.md` deferred to "the original spec" — so Go was the one language whose wire types could not be regenerated. It is now a runnable script, verified to reproduce the previously committed file byte-for-byte from the spec at the commit that last generated it.

Regenerating picked up everything Go had missed since: `get_messages` now takes an opaque `Cursor *string` (replacing `Before *time.Time`) and returns `NextCursor`, plus the `stream_reasoning` / `stream_preamble` / `cancel` events and the rich-interaction types.
