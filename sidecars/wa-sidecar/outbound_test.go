package main

import (
	"bufio"
	"encoding/json"
	"go.mau.fi/whatsmeow"
	waStore "go.mau.fi/whatsmeow/store"
	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
	"net"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestOutboundIdentityPersistsBeforeSendAndIsAccountScoped(t *testing.T) {
	path := filepath.Join(t.TempDir(), "outbound.db")
	ledger, err := openOutboundLedger(path)
	if err != nil {
		t.Fatal(err)
	}
	if err := ledger.reserve("owner", "self", "message-one"); err != nil {
		t.Fatal(err)
	}
	since := ledger.since
	// A provider failure or process crash after reservation cannot erase identity.
	ledger.Close()
	ledger, err = openOutboundLedger(path)
	if err != nil {
		t.Fatal(err)
	}
	defer ledger.Close()
	if ledger.since != since {
		t.Fatal("classification epoch changed after restart")
	}
	for _, tc := range []struct {
		account, chat, id string
		want              bool
	}{
		{"owner", "self", "message-one", true},
		{"other", "self", "message-one", false},
		{"owner", "other", "message-one", false},
		{"owner", "self", "human-message", false},
	} {
		got, err := ledger.contains(tc.account, tc.chat, tc.id)
		if err != nil || got != tc.want {
			t.Fatalf("classification = %v, %v; want %v", got, err, tc.want)
		}
	}
	if err := ledger.reserve("owner", "self", "message-one"); err == nil {
		t.Fatal("duplicate ID must not authorize a new send")
	}
	if err := ledger.reserve("", "self", "bad"); err == nil {
		t.Fatal("empty account accepted")
	}
	info, err := os.Stat(path)
	if err != nil || info.Mode().Perm() != 0600 {
		t.Fatalf("ledger must be private: %v, %v", info, err)
	}
}

func TestOutboundClassificationFailsClosedWhenUnavailable(t *testing.T) {
	s := &sidecar{}
	if got := s.agentOutbound("owner", "self", "id"); got != nil {
		t.Fatal("missing ledger must return unknown")
	}
	ledger, err := openOutboundLedger(filepath.Join(t.TempDir(), "outbound.db"))
	if err != nil {
		t.Fatal(err)
	}
	s.outbound = ledger
	if got := s.agentOutbound("owner", "self", "id"); got == nil || *got {
		t.Fatal("healthy ledger must identify an unseen ID")
	}
	ledger.Close()
	if got := s.agentOutbound("owner", "self", "id"); got != nil {
		t.Fatal("failed ledger must return unknown")
	}
}

func TestSelfChatEchoClassificationReachesWire(t *testing.T) {
	ledger, err := openOutboundLedger(filepath.Join(t.TempDir(), "outbound.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer ledger.Close()
	owner, _ := types.ParseJID("12025550111:3@s.whatsapp.net")
	chat := owner.ToNonAD()
	if err := ledger.reserve(chat.String(), chat.String(), "agent-id"); err != nil {
		t.Fatal(err)
	}
	s := &sidecar{outbound: ledger, client: &whatsmeow.Client{Store: &waStore.Device{ID: &owner}}}
	server, client := net.Pipe()
	defer server.Close()
	defer client.Close()
	s.registerConn(server)
	defer s.unregisterConn(server)
	reader := bufio.NewReader(client)
	for _, tc := range []struct {
		id   string
		at   int64
		want interface{}
	}{
		{"agent-id", ledger.since + 1, true}, {"human-id", ledger.since + 1, false},
		{"old-id", ledger.since, nil},
	} {
		go s.handleWAEvent(&events.Message{Info: types.MessageInfo{
			MessageSource: types.MessageSource{Chat: chat, Sender: owner, IsFromMe: true},
			ID:            types.MessageID(tc.id), Timestamp: time.Unix(tc.at, 0),
		}})
		client.SetReadDeadline(time.Now().Add(2 * time.Second))
		line, err := reader.ReadBytes('\n')
		if err != nil {
			t.Fatal(err)
		}
		var frame map[string]interface{}
		if err := json.Unmarshal(line, &frame); err != nil {
			t.Fatal(err)
		}
		value, present := frame["agent_outbound"]
		if !present || value != tc.want {
			t.Fatalf("wire classification = %v, want %v", value, tc.want)
		}
	}
}
