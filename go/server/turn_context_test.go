package server

import (
	"bytes"
	"context"
	"encoding/json"
	"sync"
	"testing"

	core "github.com/SmooAI/smooth-operator-core/go/core"
	"github.com/SmooAI/smooth-operator/go/protocol"
)

// captureTool is a host tool that records the per-turn TurnContext it saw (the turn's
// image + file attachments) and optionally writes a directive onto the sink. It stands
// in for a real host tool (send_file, a workspace-ingest tool, …) so the tests can assert
// what reaches a tool and what a tool can emit.
type captureTool struct {
	mu        sync.Mutex
	sawImages []protocol.RequestImagesElem
	sawFiles  []protocol.RequestFilesElem
	sawCtx    bool
	directive any // when non-nil, written onto the turn's directive sink
}

func (c *captureTool) Name() string        { return "capture" }
func (c *captureTool) Description() string { return "records the turn's attachments" }
func (c *captureTool) Parameters() map[string]any {
	return map[string]any{"type": "object", "properties": map[string]any{}}
}

func (c *captureTool) Execute(ctx context.Context, _ map[string]any) (string, error) {
	tc := TurnContextFrom(ctx)
	c.mu.Lock()
	defer c.mu.Unlock()
	if tc != nil {
		c.sawCtx = true
		c.sawImages = tc.Images
		c.sawFiles = tc.Files
		if c.directive != nil {
			tc.SetDirective(c.directive)
		}
	}
	return "ok", nil
}

// safeSink is a mutex-guarded event collector: the turn streams events from a goroutine,
// so the sink must be safe to call concurrently with the test reading it after
// WaitForTurns.
type safeSink struct {
	mu     sync.Mutex
	events []map[string]any
}

func (s *safeSink) sink(ev map[string]any) {
	s.mu.Lock()
	s.events = append(s.events, ev)
	s.mu.Unlock()
}

func (s *safeSink) find(eventType string) map[string]any {
	s.mu.Lock()
	defer s.mu.Unlock()
	for _, ev := range s.events {
		if ev["type"] == eventType {
			return ev
		}
	}
	return nil
}

// createSessionForTest drives create_conversation_session and returns the new session id.
func createSessionForTest(t *testing.T, d *FrameDispatcher) string {
	t.Helper()
	var sink safeSink
	d.Dispatch(context.Background(), []byte(`{"action":"create_conversation_session","requestId":"c1","agentId":"a1"}`), sink.sink)
	resp := sink.find("immediate_response")
	if resp == nil {
		t.Fatal("no immediate_response from create_conversation_session")
	}
	data, _ := resp["data"].(map[string]any)
	id, _ := data["sessionId"].(string)
	if id == "" {
		t.Fatalf("no sessionId in create response: %+v", resp)
	}
	return id
}

// sendAndWait dispatches a raw send_message frame and blocks until the turn completes.
func sendAndWait(d *FrameDispatcher, frame string) *safeSink {
	sink := &safeSink{}
	d.Dispatch(context.Background(), []byte(frame), sink.sink)
	d.WaitForTurns()
	return sink
}

// newToolDispatcher builds a dispatcher whose engine is a mock scripted to call `capture`
// then reply with final text, with the capture tool registered. Unscoped access so the
// anonymous test session owns its conversation.
func newToolDispatcher(tool core.Tool) *FrameDispatcher {
	store := NewInMemorySessionStore()
	mock := core.NewMockLlmProvider().PushToolCall("t-1", "capture", "{}").PushText("done")
	// Zero-value AccessContext ⇒ AuthEnabled false ⇒ unscoped, so the anonymous test
	// session owns its conversation.
	return NewFrameDispatcher(store, mock, AccessContext{}, "", nil, []core.Tool{tool}, nil, nil, nil, "", nil, nil, nil, nil)
}

// TestSendMessageAttachesImagesAndFilesToToolContext asserts a send_message's images[]
// and files[] are parsed and surfaced on the per-turn TurnContext a host tool reads
// (the Go analog of the Rust ToolProviderContext.images / files).
func TestSendMessageAttachesImagesAndFilesToToolContext(t *testing.T) {
	tool := &captureTool{}
	d := newToolDispatcher(tool)
	sid := createSessionForTest(t, d)

	frame := `{"action":"send_message","requestId":"r-1","sessionId":"` + sid + `","message":"look at these",` +
		`"images":[{"url":"data:image/png;base64,AAAA","detail":"high"}],` +
		`"files":[{"name":"data.csv","mimeType":"text/csv","url":"data:text/csv;base64,Zm9v"}]}`
	sink := sendAndWait(d, frame)

	if sink.find("eventual_response") == nil {
		t.Fatal("turn did not complete with an eventual_response")
	}
	tool.mu.Lock()
	defer tool.mu.Unlock()
	if !tool.sawCtx {
		t.Fatal("tool did not see a TurnContext on ctx")
	}
	if len(tool.sawImages) != 1 || tool.sawImages[0].URL != "data:image/png;base64,AAAA" {
		t.Fatalf("tool image attachment = %+v, want the one data: image", tool.sawImages)
	}
	if tool.sawImages[0].Detail == nil || *tool.sawImages[0].Detail != protocol.RequestImagesElemDetailHigh {
		t.Fatalf("tool image detail = %+v, want high", tool.sawImages[0].Detail)
	}
	if len(tool.sawFiles) != 1 || tool.sawFiles[0].Name != "data.csv" || tool.sawFiles[0].URL != "data:text/csv;base64,Zm9v" {
		t.Fatalf("tool file attachment = %+v, want the one csv file", tool.sawFiles)
	}
}

// TestToolDirectiveLandsOnEventualResponse asserts a host tool that writes a directive
// onto the turn's sink has it emitted on the terminal eventual_response under
// data.data.directive (the send_file convention), mirroring the Rust directive drain.
func TestToolDirectiveLandsOnEventualResponse(t *testing.T) {
	want := map[string]any{
		"type":  "send_file",
		"files": []any{map[string]any{"name": "report.pdf", "url": "data:application/pdf;base64,JVBERi0="}},
	}
	tool := &captureTool{directive: want}
	d := newToolDispatcher(tool)
	sid := createSessionForTest(t, d)

	frame := `{"action":"send_message","requestId":"r-2","sessionId":"` + sid + `","message":"send me the report"}`
	sink := sendAndWait(d, frame)

	ev := sink.find("eventual_response")
	if ev == nil {
		t.Fatal("no eventual_response")
	}
	// Directive is double-nested under data.data, next to messageId/response.
	raw, _ := json.Marshal(ev)
	var got struct {
		Data struct {
			Data struct {
				Directive map[string]any `json:"directive"`
			} `json:"data"`
		} `json:"data"`
	}
	if err := json.Unmarshal(raw, &got); err != nil {
		t.Fatalf("unmarshal event: %v", err)
	}
	if got.Data.Data.Directive == nil {
		t.Fatalf("eventual_response carried no directive; event: %s", raw)
	}
	if got.Data.Data.Directive["type"] != "send_file" {
		t.Fatalf("directive.type = %v, want send_file; event: %s", got.Data.Data.Directive["type"], raw)
	}
}

// TestEventualResponseOmitsDirectiveWhenToolWritesNone asserts a turn whose tools wrote
// no directive omits the directive field entirely (back-compat / byte-for-byte unchanged).
func TestEventualResponseOmitsDirectiveWhenToolWritesNone(t *testing.T) {
	tool := &captureTool{} // no directive
	d := newToolDispatcher(tool)
	sid := createSessionForTest(t, d)

	frame := `{"action":"send_message","requestId":"r-3","sessionId":"` + sid + `","message":"hi"}`
	sink := sendAndWait(d, frame)

	ev := sink.find("eventual_response")
	if ev == nil {
		t.Fatal("no eventual_response")
	}
	raw, _ := json.Marshal(ev)
	if bytes.Contains(raw, []byte(`"directive":`)) {
		t.Fatalf("directive must be absent when no tool wrote one; event: %s", raw)
	}
}

// TestMalformedAttachmentsFailSoft asserts a send_message with a malformed images/files
// value is NOT rejected: the bad attachments are dropped and the turn still runs to an
// eventual_response (mirroring the Rust from_value(...).ok().unwrap_or_default()).
func TestMalformedAttachmentsFailSoft(t *testing.T) {
	tool := &captureTool{}
	d := newToolDispatcher(tool)
	sid := createSessionForTest(t, d)

	// images is a string (not an array); files has an element with a wrong-typed url.
	frame := `{"action":"send_message","requestId":"r-4","sessionId":"` + sid + `","message":"still works",` +
		`"images":"not-an-array","files":[{"name":"x","url":123}]}`
	sink := sendAndWait(d, frame)

	if sink.find("error") != nil {
		t.Fatalf("malformed attachments must not produce an error event: %+v", sink.events)
	}
	if sink.find("eventual_response") == nil {
		t.Fatal("turn with malformed attachments did not complete")
	}
	tool.mu.Lock()
	defer tool.mu.Unlock()
	if len(tool.sawImages) != 0 || len(tool.sawFiles) != 0 {
		t.Fatalf("malformed attachments should be dropped, got images=%+v files=%+v", tool.sawImages, tool.sawFiles)
	}
}

// TestTurnContextDirectiveRoundTrip is the unit-level check on the sink: last-write-wins
// and the absent-vs-written distinction the drain relies on.
func TestTurnContextDirectiveRoundTrip(t *testing.T) {
	tc := &TurnContext{}
	if _, ok := tc.Directive(); ok {
		t.Fatal("fresh TurnContext must report no directive")
	}
	tc.SetDirective(map[string]any{"type": "a"})
	tc.SetDirective(map[string]any{"type": "b"}) // last-write-wins
	got, ok := tc.Directive()
	if !ok {
		t.Fatal("expected a directive after SetDirective")
	}
	if m, _ := got.(map[string]any); m["type"] != "b" {
		t.Fatalf("directive = %+v, want last write {type:b}", got)
	}
}
