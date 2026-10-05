package main

import (
	"bufio"
	"encoding/json"
	"net"
	"path/filepath"
	"testing"
	"time"
)

func TestEventJournalSurvivesRestartAndAcknowledgesOnlyCommittedEvents(t *testing.T) {
	path := filepath.Join(t.TempDir(), "Mac Home ü with spaces", "events.db")
	journal, err := openEventJournal(path)
	if err != nil {
		t.Fatal(err)
	}
	first := json.RawMessage(`{"version":1,"event":"received-message","id":"m1","chat":"1@s.whatsapp.net"}`)
	second := json.RawMessage(`{"version":1,"event":"received-message","id":"m2","chat":"1@s.whatsapp.net"}`)
	seq1, fresh, err := journal.appendMessage("account", "1@s.whatsapp.net", "m1", first)
	if err != nil {
		t.Fatal(err)
	}
	if !fresh {
		t.Fatal("first message was not inserted")
	}
	if seq, fresh, err := journal.appendMessage("account", "1@s.whatsapp.net", "m1", first); err != nil || seq != seq1 || fresh {
		t.Fatalf("duplicate message should keep original sequence: seq=%d err=%v", seq, err)
	}
	seq2, fresh, err := journal.appendMessage("account", "1@s.whatsapp.net", "m2", second)
	if err != nil || seq2 <= seq1 || !fresh {
		t.Fatalf("second message sequence: seq=%d err=%v", seq2, err)
	}
	if err := journal.Close(); err != nil {
		t.Fatal(err)
	}

	journal, err = openEventJournal(path)
	if err != nil {
		t.Fatal(err)
	}
	defer journal.Close()
	rows, err := journal.readAfter(0, 10)
	if err != nil || len(rows) != 2 || rows[0].Seq != seq1 || rows[1].Seq != seq2 {
		t.Fatalf("restart replay: rows=%+v err=%v", rows, err)
	}
	if err := journal.ackThrough(seq2); err == nil {
		t.Fatal("cannot acknowledge a later event before the first is committed")
	}
	if err := journal.ackThrough(seq1); err != nil {
		t.Fatal(err)
	}
	if pending, err := journal.pendingCount(); err != nil || pending != 1 {
		t.Fatalf("pending count after one ack: pending=%d err=%v", pending, err)
	}
	cursor, err := journal.acked()
	if err != nil {
		t.Fatal(err)
	}
	rows, err = journal.readAfter(cursor, 10)
	if err != nil || len(rows) != 1 || rows[0].Seq != seq2 {
		t.Fatalf("unacknowledged message must replay: rows=%+v err=%v", rows, err)
	}
	if err := journal.ackThrough(seq2 + 1); err == nil {
		t.Fatal("cannot acknowledge a sequence that was never journaled")
	}
}

func TestEventReplayRPCRequiresExplicitAck(t *testing.T) {
	journal, err := openEventJournal(filepath.Join(t.TempDir(), "events.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer journal.Close()
	seq, _, err := journal.appendMessage("account", "1@s.whatsapp.net", "m1", json.RawMessage(`{"version":1,"event":"received-message","id":"m1","chat":"1@s.whatsapp.net"}`))
	if err != nil {
		t.Fatal(err)
	}
	s := &sidecar{journal: journal}
	server, client := net.Pipe()
	defer client.Close()
	go s.serveConn(server)
	client.SetDeadline(time.Now().Add(2 * time.Second))
	reader := bufio.NewReader(client)
	call := func(request string) rpcResponse {
		t.Helper()
		if _, err := client.Write([]byte(request + "\n")); err != nil {
			t.Fatal(err)
		}
		line, err := reader.ReadBytes('\n')
		if err != nil {
			t.Fatal(err)
		}
		var response rpcResponse
		if err := json.Unmarshal(line, &response); err != nil {
			t.Fatal(err)
		}
		return response
	}
	replay := call(`{"version":1,"request_id":"r1","op":"replay_events","params":{"after":0,"limit":10}}`)
	if !replay.OK {
		t.Fatalf("replay failed: %+v", replay)
	}
	events := replay.Result.(map[string]interface{})["events"].([]interface{})
	if len(events) != 1 || events[0].(map[string]interface{})["seq"] != float64(seq) {
		t.Fatalf("replay lost stored event: %+v", events)
	}
	status := call(`{"version":1,"request_id":"s1","op":"status","params":{}}`)
	if !status.OK || status.Result.(map[string]interface{})["events_pending"] != float64(1) {
		t.Fatalf("pending event is not observable: %+v", status)
	}
	if cursor, err := journal.acked(); err != nil || cursor != 0 {
		t.Fatalf("reading must not acknowledge: cursor=%d err=%v", cursor, err)
	}
	ack := call(`{"version":1,"request_id":"a1","op":"ack_events","params":{"through":1}}`)
	if !ack.OK {
		t.Fatalf("ack failed: %+v", ack)
	}
	if cursor, err := journal.acked(); err != nil || cursor != seq {
		t.Fatalf("ack not persisted: cursor=%d err=%v", cursor, err)
	}
	status = call(`{"version":1,"request_id":"s2","op":"status","params":{}}`)
	if !status.OK || status.Result.(map[string]interface{})["events_pending"] != float64(0) {
		t.Fatalf("ack did not clear pending count: %+v", status)
	}
	replayedAgain := call(`{"version":1,"request_id":"r2","op":"replay_events","params":{"after":0,"limit":10}}`)
	if !replayedAgain.OK || len(replayedAgain.Result.(map[string]interface{})["events"].([]interface{})) != 0 {
		t.Fatalf("acknowledged event replayed: %+v", replayedAgain)
	}
}
