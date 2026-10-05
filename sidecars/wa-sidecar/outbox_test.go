package main

import (
	"errors"
	"path/filepath"
	"sync"
	"sync/atomic"
	"testing"
)

func TestOutboxRestartConflictAndUncertainDelivery(t *testing.T) {
	path := filepath.Join(t.TempDir(), "outbox.db")
	j, err := openEventJournal(path)
	if err != nil {
		t.Fatal(err)
	}
	calls := 0
	send := func(id string) error { calls++; return nil }
	id, err := j.sendOnce("account", "turn:1", "owner", "hello", "message1", send)
	if err != nil || id != "message1" {
		t.Fatalf("first send: %s %v", id, err)
	}
	j.Close()
	j, err = openEventJournal(path)
	if err != nil {
		t.Fatal(err)
	}
	defer j.Close()
	id, err = j.sendOnce("account", "turn:1", "owner", "hello", "message2", send)
	if err != nil || id != "message1" || calls != 1 {
		t.Fatalf("replay sent twice: %s %v %d", id, err, calls)
	}
	for _, pair := range [][2]string{{"someone-else", "hello"}, {"owner", "different"}} {
		_, err = j.sendOnce("account", "turn:1", pair[0], pair[1], "message3", send)
		if !errors.Is(err, errSendKeyConflict) {
			t.Fatalf("conflicting key accepted: %v", err)
		}
	}
	_, err = j.sendOnce("account", "turn:2", "owner", "maybe delivered", "message4", func(string) error { calls++; return errors.New("timeout") })
	if !errors.Is(err, errUncertainDelivery) {
		t.Fatal(err)
	}
	_, err = j.sendOnce("account", "turn:2", "owner", "maybe delivered", "message5", send)
	if !errors.Is(err, errUncertainDelivery) || calls != 2 {
		t.Fatalf("uncertain send retried: %v %d", err, calls)
	}
	for _, id := range []string{"message1", "message4"} {
		owned, err := j.isAgentMessage("account", "owner", id)
		if !owned || err != nil {
			t.Fatalf("lost echo identity: %v %v", owned, err)
		}
	}
	for _, scope := range [][3]string{{"account", "owner", "human"}, {"other", "owner", "message1"}, {"account", "other", "message1"}} {
		owned, err := j.isAgentMessage(scope[0], scope[1], scope[2])
		if owned || err != nil {
			t.Fatalf("cross-scope echo identity: %v %v", owned, err)
		}
	}
	if err := j.confirmSend("account", "owner", "message4"); err != nil {
		t.Fatal(err)
	}
	id, err = j.sendOnce("account", "turn:2", "owner", "maybe delivered", "message6", send)
	if err != nil || id != "message4" || calls != 2 {
		t.Fatalf("receipt did not reconcile: %s %v %d", id, err, calls)
	}
}

func TestConcurrentOutboxOnlyOneNetworkAttempt(t *testing.T) {
	j, err := openEventJournal(filepath.Join(t.TempDir(), "outbox.db"))
	if err != nil {
		t.Fatal(err)
	}
	defer j.Close()
	var calls atomic.Int32
	var group sync.WaitGroup
	for n := 0; n < 20; n++ {
		group.Add(1)
		go func() {
			defer group.Done()
			_, err := j.sendOnce("account", "same", "owner", "text", "same-id", func(string) error { calls.Add(1); return nil })
			if err != nil && !errors.Is(err, errUncertainDelivery) {
				t.Error(err)
			}
		}()
	}
	group.Wait()
	if calls.Load() != 1 {
		t.Fatalf("network attempts=%d", calls.Load())
	}
}

func TestReservationSurvivesCrashBeforeNetwork(t *testing.T) {
	path := filepath.Join(t.TempDir(), "outbox.db")
	j, err := openEventJournal(path)
	if err != nil {
		t.Fatal(err)
	}
	_, fresh, err := j.prepareSend("a", "key", "owner", "text", "id")
	if !fresh || err != nil {
		t.Fatal(err)
	}
	j.Close()
	j, err = openEventJournal(path)
	if err != nil {
		t.Fatal(err)
	}
	defer j.Close()
	_, err = j.sendOnce("a", "key", "owner", "text", "another-id", func(string) error { t.Fatal("must not retry ambiguous reservation"); return nil })
	if !errors.Is(err, errUncertainDelivery) {
		t.Fatal(err)
	}
}
