package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"go.mau.fi/whatsmeow"
	waStore "go.mau.fi/whatsmeow/store"
	"go.mau.fi/whatsmeow/types"
	"net"
	"path/filepath"
	"testing"
	"time"
)

func TestDeliveryKeyRetainsUnknownAndConfirmedOutcomes(t *testing.T) {
	path := filepath.Join(t.TempDir(), "ledger.db")
	l, err := openOutboundLedger(path)
	if err != nil {
		t.Fatal(err)
	}
	rec, fresh, err := l.reserveDelivery("owner", "chat", "key", "text", "id-one")
	if err != nil || !fresh || rec.Sent {
		t.Fatal(rec, fresh, err)
	}
	l.Close()
	l, err = openOutboundLedger(path)
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	rec, fresh, err = l.reserveDelivery("owner", "chat", "key", "text", "id-two")
	if err != nil || fresh || rec.ID != "id-one" || rec.Sent {
		t.Fatal(rec, fresh, err)
	}
	if _, _, err = l.reserveDelivery("owner", "chat", "key", "changed", "id-three"); err == nil {
		t.Fatal("changed content accepted")
	}
	if err = l.confirmDelivery("owner", "chat", "key", "id-wrong"); err == nil {
		t.Fatal("wrong provider ID confirmed")
	}
	if err = l.confirmDelivery("owner", "chat", "key", "id-one"); err != nil {
		t.Fatal(err)
	}
	rec, err = l.lookupDelivery("owner", "chat", "key")
	if err != nil || rec == nil || !rec.Sent || rec.ID != "id-one" {
		t.Fatal(rec, err)
	}
	rec, err = l.lookupDelivery("other", "chat", "key")
	if err != nil || rec != nil {
		t.Fatal("account scope leaked", rec, err)
	}
	if known, err := l.contains("owner", "chat", "id-one"); err != nil || !known {
		t.Fatal("echo reservation missing", err)
	}
}

func TestLookupDeliveryWireSurvivesRestartWithoutConnection(t *testing.T) {
	path := filepath.Join(t.TempDir(), "ledger.db")
	ledger, err := openOutboundLedger(path)
	if err != nil {
		t.Fatal(err)
	}
	owner, _ := types.ParseJID("12025550111:3@s.whatsapp.net")
	account := owner.ToNonAD().String()
	if _, _, err = ledger.reserveDelivery(account, account, "key", "private content", "id"); err != nil {
		t.Fatal(err)
	}
	if err = ledger.confirmDelivery(account, account, "key", "id"); err != nil {
		t.Fatal(err)
	}
	ledger.Close()
	ledger, err = openOutboundLedger(path)
	if err != nil {
		t.Fatal(err)
	}
	defer ledger.Close()
	s := &sidecar{outbound: ledger, client: &whatsmeow.Client{Store: &waStore.Device{ID: &owner}}}
	server, client := net.Pipe()
	defer server.Close()
	defer client.Close()
	s.registerConn(server)
	defer s.unregisterConn(server)
	req := rpcRequest{RequestID: "lookup", Op: "lookup_delivery", Version: 1, Params: json.RawMessage(`{"account":"12025550111@s.whatsapp.net","chat_jid":"12025550111@s.whatsapp.net","delivery_key":"key"}`)}
	go s.dispatch(server, req)
	client.SetReadDeadline(time.Now().Add(2 * time.Second))
	line, err := bufio.NewReader(client).ReadBytes('\n')
	if err != nil {
		t.Fatal(err)
	}
	var result struct {
		OK     bool           `json:"ok"`
		Result deliveryRecord `json:"result"`
	}
	if err = json.Unmarshal(line, &result); err != nil {
		t.Fatal(err)
	}
	if !result.OK || !result.Result.Sent || result.Result.ID != "id" {
		t.Fatal("missing positive lookup", string(line))
	}
}

func TestConcurrentDeliveryReservationAllowsOneSender(t *testing.T) {
	l, err := openOutboundLedger(filepath.Join(t.TempDir(), "ledger.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer l.Close()
	results := make(chan bool, 8)
	errors := make(chan error, 8)
	for i := 0; i < 8; i++ {
		go func(i int) {
			_, fresh, err := l.reserveDelivery("owner", "chat", "same-key", "same content", fmt.Sprintf("id-%d", i))
			results <- fresh
			errors <- err
		}(i)
	}
	fresh := 0
	for i := 0; i < 8; i++ {
		if <-results {
			fresh++
		}
		if err := <-errors; err != nil {
			t.Fatal(err)
		}
	}
	if fresh != 1 {
		t.Fatalf("got %d permitted sends", fresh)
	}
}
