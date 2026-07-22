module github.com/SmooAI/smooth-operator/go/server

go 1.26

require (
	github.com/SmooAI/smooth-operator-core/go v0.0.0-20260715223950-4e4f7fac250c
	github.com/SmooAI/smooth-operator/go v0.0.0-00010101000000-000000000000
	github.com/coder/websocket v1.8.14
	github.com/google/uuid v1.6.0
)

require (
	github.com/BurntSushi/toml v1.4.0 // indirect
	github.com/santhosh-tekuri/jsonschema/v6 v6.0.2 // indirect
	golang.org/x/text v0.14.0 // indirect
)

// The protocol/types live in the sibling client module (kept dependency-light and
// published on its own); the server consumes it locally rather than via a tag.
replace github.com/SmooAI/smooth-operator/go => ../
