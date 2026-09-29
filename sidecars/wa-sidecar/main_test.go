package main

import (
	"bufio"
	"encoding/json"
	"net"
	"os"
	"strings"
	"testing"
	"time"

	"go.mau.fi/whatsmeow/proto/waE2E"
	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
)

// These exercise the actual NDJSON dispatcher while the WhatsApp network is
// absent. A connected Rust client must get explicit failures, not a made-up
// empty history or a successful reply in a protocol it cannot understand.
func exchange(t *testing.T, request string) rpcResponse {
	t.Helper()
	server, client := net.Pipe()
	defer client.Close()
	s := &sidecar{conn: server}
	go s.serveConn(server)
	if err := client.SetDeadline(time.Now().Add(2 * time.Second)); err != nil {
		t.Fatal(err)
	}
	if _, err := client.Write([]byte(request + "\n")); err != nil {
		t.Fatal(err)
	}
	line, err := bufio.NewReader(client).ReadBytes('\n')
	if err != nil {
		t.Fatal(err)
	}
	var response rpcResponse
	if err := json.Unmarshal(line, &response); err != nil {
		t.Fatal(err)
	}
	return response
}

func TestUnsupportedProtocolVersionFailsExplicitly(t *testing.T) {
	response := exchange(t, `{"version":999,"request_id":"v1","op":"fetch_history","params":{"chat_jid":"1@s.whatsapp.net"}}`)
	if response.OK || response.Error == nil || response.Error.Kind != "BadRequest" {
		t.Fatalf("unknown protocol version must fail visibly: %+v", response)
	}
}

func TestHistoryWithoutStoredMessagesIsUnavailable(t *testing.T) {
	response := exchange(t, `{"version":1,"request_id":"h1","op":"fetch_history","params":{"chat_jid":"1@s.whatsapp.net"}}`)
	if response.OK || response.Error == nil || response.Error.Kind != "Unavailable" {
		t.Fatalf("history must not claim an empty successful result: %+v", response)
	}
}

func TestOfflineStatusAndSendAreHonest(t *testing.T) {
	status := exchange(t, `{"version":1,"request_id":"s1","op":"status","params":{}}`)
	if !status.OK {
		t.Fatalf("offline status failed: %+v", status)
	}
	state := status.Result.(map[string]interface{})
	if state["paired"] != false || state["connected"] != false {
		t.Fatalf("offline sidecar claimed a connection: %+v", state)
	}
	sent := exchange(t, `{"version":1,"request_id":"x1","op":"send_text","params":{"chat_jid":"1@s.whatsapp.net","text":"hi"}}`)
	if sent.OK || sent.Error == nil || sent.Error.Kind != "NotPaired" {
		t.Fatalf("offline send must fail: %+v", sent)
	}
}

func TestBadFrameDoesNotPoisonNextRequest(t *testing.T) {
	server, client := net.Pipe()
	defer client.Close()
	go (&sidecar{conn: server}).serveConn(server)
	client.SetDeadline(time.Now().Add(2 * time.Second))
	if _, err := client.Write([]byte("not-json\n" +
		`{"version":1,"request_id":"good","op":"status","params":{}}` + "\n")); err != nil {
		t.Fatal(err)
	}
	reader := bufio.NewReader(client)
	seen := map[string]bool{}
	for range 2 {
		line, err := reader.ReadBytes('\n')
		if err != nil {
			t.Fatal(err)
		}
		var response rpcResponse
		if err := json.Unmarshal(line, &response); err != nil {
			t.Fatal(err)
		}
		seen[response.RequestID] = true
	}
	if !seen[""] || !seen["good"] {
		t.Fatalf("malformed and valid frames did not both receive responses: %+v", seen)
	}
}

func TestFragmentedRequestIsReassembled(t *testing.T) {
	server, client := net.Pipe()
	defer client.Close()
	go (&sidecar{conn: server}).serveConn(server)
	client.SetDeadline(time.Now().Add(2 * time.Second))
	go func() {
		for _, part := range []string{`{"version":1,"request_id":`, `"split","op":"status",`, "\"params\":{}}\n"} {
			_, _ = client.Write([]byte(part))
			time.Sleep(time.Millisecond)
		}
	}()
	line, err := bufio.NewReader(client).ReadBytes('\n')
	if err != nil {
		t.Fatal(err)
	}
	var response rpcResponse
	if err := json.Unmarshal(line, &response); err != nil {
		t.Fatal(err)
	}
	if !response.OK || response.RequestID != "split" {
		t.Fatalf("fragmented frame was not reassembled: %+v", response)
	}
}

func TestOversizedFrameFailsVisibly(t *testing.T) {
	server, client := net.Pipe()
	defer client.Close()
	go (&sidecar{conn: server}).serveConn(server)
	client.SetDeadline(time.Now().Add(2 * time.Second))
	request := `{"version":1,"request_id":"large","op":"status","params":{"padding":"` + strings.Repeat("x", 4*1024*1024) + `"}}` + "\n"
	go func() { _, _ = client.Write([]byte(request)) }()
	line, err := bufio.NewReader(client).ReadBytes('\n')
	if err != nil {
		t.Fatal(err)
	}
	var response rpcResponse
	if err := json.Unmarshal(line, &response); err != nil {
		t.Fatal(err)
	}
	if response.OK || response.Error == nil || response.Error.Kind != "BadRequest" {
		t.Fatalf("oversized frame must report an error: %+v", response)
	}
}

func TestQRCodeIsBufferedForLateClient(t *testing.T) {
	s := &sidecar{}
	s.emitEvent(map[string]interface{}{"event": "qr", "code": "test-secret"})
	server, client := net.Pipe()
	defer client.Close()
	go s.serveConn(server)
	client.SetReadDeadline(time.Now().Add(2 * time.Second))
	line, err := bufio.NewReader(client).ReadBytes('\n')
	if err != nil {
		t.Fatal(err)
	}
	var event map[string]interface{}
	if err := json.Unmarshal(line, &event); err != nil {
		t.Fatal(err)
	}
	if event["event"] != "qr" || event["code"] != "test-secret" || event["version"] != float64(1) {
		t.Fatalf("late client did not receive current QR: %+v", event)
	}
}

func TestMediaAndQuoteMetadataReachTheWire(t *testing.T) {
	server, client := net.Pipe()
	defer server.Close()
	defer client.Close()
	s := &sidecar{conn: server}
	chat, _ := types.ParseJID("15551234567@s.whatsapp.net")
	event := &events.Message{
		Info: types.MessageInfo{
			MessageSource: types.MessageSource{Chat: chat, Sender: chat},
			ID:            "image-1", Timestamp: time.Unix(100, 0),
		},
		Message: &waE2E.Message{ImageMessage: &waE2E.ImageMessage{
			Mimetype:   strptr("image/png"),
			Caption:    strptr("look at this"),
			FileLength: uint64ptr(123),
			ContextInfo: &waE2E.ContextInfo{
				StanzaID: strptr("quoted-1"), MentionedJID: []string{"2@s.whatsapp.net"},
			},
		}},
	}
	go s.handleWAEvent(event)
	client.SetReadDeadline(time.Now().Add(2 * time.Second))
	line, err := bufio.NewReader(client).ReadBytes('\n')
	if err != nil {
		t.Fatal(err)
	}
	var frame map[string]interface{}
	if err := json.Unmarshal(line, &frame); err != nil {
		t.Fatal(err)
	}
	if frame["quoted_message_id"] != "quoted-1" || frame["text"] != "look at this" {
		t.Fatalf("quote or caption lost: %+v", frame)
	}
	media, ok := frame["media"].(map[string]interface{})
	if !ok || media["kind"] != "image" || media["mime_type"] != "image/png" || media["size"] != float64(123) {
		t.Fatalf("media descriptor lost: %+v", frame)
	}
	mentions, ok := frame["mentioned_jids"].([]interface{})
	if !ok || len(mentions) != 1 || mentions[0] != "2@s.whatsapp.net" {
		t.Fatalf("mentions lost: %+v", frame)
	}
}

func TestDeliveryReceiptAndDisconnectReachTheWire(t *testing.T) {
	server, client := net.Pipe()
	defer server.Close()
	defer client.Close()
	s := &sidecar{conn: server}
	chat, _ := types.ParseJID("15551234567@s.whatsapp.net")
	go func() {
		s.handleWAEvent(&events.Receipt{
			MessageSource: types.MessageSource{Chat: chat, Sender: chat},
			MessageIDs:    []types.MessageID{"m1", "m2"},
			Type:          types.ReceiptTypeDelivered,
			Timestamp:     time.Unix(101, 0),
		})
		s.handleWAEvent(&events.Disconnected{})
	}()
	client.SetReadDeadline(time.Now().Add(2 * time.Second))
	reader := bufio.NewReader(client)
	line, err := reader.ReadBytes('\n')
	if err != nil {
		t.Fatal(err)
	}
	var receipt map[string]interface{}
	if err := json.Unmarshal(line, &receipt); err != nil {
		t.Fatal(err)
	}
	if receipt["event"] != "receipt" || receipt["receipt_type"] != "delivered" || len(receipt["message_ids"].([]interface{})) != 2 {
		t.Fatalf("incorrect receipt: %+v", receipt)
	}
	line, err = reader.ReadBytes('\n')
	if err != nil {
		t.Fatal(err)
	}
	var disconnected map[string]interface{}
	if err := json.Unmarshal(line, &disconnected); err != nil {
		t.Fatal(err)
	}
	if disconnected["event"] != "disconnected" || disconnected["version"] != float64(1) {
		t.Fatalf("incorrect disconnect event: %+v", disconnected)
	}
}

func strptr(value string) *string    { return &value }
func uint64ptr(value uint64) *uint64 { return &value }

func TestSharedGoldenFrames(t *testing.T) {
	bytes, err := os.ReadFile("../../docs/fixtures/whatsapp/wire-v1.ndjson")
	if err != nil {
		t.Fatal(err)
	}
	lines := strings.Split(strings.TrimSpace(string(bytes)), "\n")
	if len(lines) != 3 {
		t.Fatalf("got %d frames, want 3", len(lines))
	}
	response := exchange(t, lines[0])
	got, err := json.Marshal(response)
	if err != nil {
		t.Fatal(err)
	}
	var actual, expected interface{}
	if err := json.Unmarshal(got, &actual); err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal([]byte(lines[1]), &expected); err != nil {
		t.Fatal(err)
	}
	if !jsonEqual(actual, expected) {
		t.Fatalf("Go status frame drifted from shared fixture: %s", got)
	}
	var event map[string]interface{}
	if err := json.Unmarshal([]byte(lines[2]), &event); err != nil {
		t.Fatal(err)
	}
	if event["version"] != float64(1) || event["media"].(map[string]interface{})["kind"] != "image" {
		t.Fatalf("shared media event is invalid: %+v", event)
	}
}

func jsonEqual(a, b interface{}) bool {
	aa, _ := json.Marshal(a)
	bb, _ := json.Marshal(b)
	return string(aa) == string(bb)
}

func TestPrivateDirectoryRejectsSymlinkAndTightensPermissions(t *testing.T) {
	root := t.TempDir()
	open := root + "/open"
	if err := os.Mkdir(open, 0755); err != nil {
		t.Fatal(err)
	}
	if err := privateDirectory(open); err != nil {
		t.Fatal(err)
	}
	info, err := os.Stat(open)
	if err != nil {
		t.Fatal(err)
	}
	if info.Mode().Perm() != 0700 {
		t.Fatalf("directory is %o, want 0700", info.Mode().Perm())
	}
	link := root + "/link"
	if err := os.Symlink(open, link); err != nil {
		t.Fatal(err)
	}
	if err := privateDirectory(link); err == nil {
		t.Fatal("a symlinked state/socket directory must be rejected")
	}
}
