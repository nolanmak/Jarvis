package main

import (
	"database/sql"
	"fmt"
	"os"
	"syscall"
	"time"
)

// Only transport identities are retained, never message bodies. Reservation is
// committed before network I/O so an early echo, timeout or restart cannot turn
// an agent-generated message into an owner command.
type outboundLedger struct {
	db    *sql.DB
	since int64
}

func openOutboundLedger(path string) (*outboundLedger, error) {
	file, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR|syscall.O_NOFOLLOW, 0600)
	if err != nil {
		return nil, err
	}
	if err = file.Chmod(0600); err != nil {
		file.Close()
		return nil, err
	}
	file.Close()
	db, err := sql.Open("sqlite", path)
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1)
	if _, err = db.Exec(`PRAGMA busy_timeout=5000; CREATE TABLE IF NOT EXISTS agent_outbound (
 account TEXT NOT NULL, chat TEXT NOT NULL, message_id TEXT NOT NULL,
 PRIMARY KEY(account,chat,message_id));
 CREATE TABLE IF NOT EXISTS agent_deliveries(account TEXT NOT NULL, chat TEXT NOT NULL, delivery_key TEXT NOT NULL, message_id TEXT NOT NULL, content_hash TEXT NOT NULL, sent INTEGER NOT NULL CHECK(sent IN (0,1)), PRIMARY KEY(account,chat,delivery_key))`); err != nil {
		db.Close()
		return nil, err
	}
	if _, err = db.Exec(`CREATE TABLE IF NOT EXISTS agent_outbound_epoch (id INTEGER PRIMARY KEY CHECK(id=1), started_at INTEGER NOT NULL);
 INSERT OR IGNORE INTO agent_outbound_epoch(id,started_at) VALUES(1,?)`, time.Now().Unix()); err != nil {
		db.Close()
		return nil, err
	}
	var since int64
	if err = db.QueryRow("SELECT started_at FROM agent_outbound_epoch WHERE id=1").Scan(&since); err != nil {
		db.Close()
		return nil, err
	}
	return &outboundLedger{db: db, since: since}, nil
}
func (l *outboundLedger) Close() error { return l.db.Close() }
func (l *outboundLedger) reserve(account, chat, id string) error {
	if account == "" || chat == "" || id == "" {
		return fmt.Errorf("outbound identity unavailable")
	}
	_, err := l.db.Exec("INSERT INTO agent_outbound(account,chat,message_id) VALUES(?,?,?)", account, chat, id)
	return err
}
func (l *outboundLedger) contains(account, chat, id string) (bool, error) {
	if account == "" || chat == "" || id == "" {
		return false, fmt.Errorf("outbound identity unavailable")
	}
	var found bool
	err := l.db.QueryRow("SELECT EXISTS(SELECT 1 FROM agent_outbound WHERE account=? AND chat=? AND message_id=?)", account, chat, id).Scan(&found)
	return found, err
}

// nil means unknown and must never be interpreted as an owner-authored message.
func (s *sidecar) agentOutbound(account, chat, id string) *bool {
	if s.outbound == nil {
		return nil
	}
	found, err := s.outbound.contains(account, chat, id)
	if err != nil {
		return nil
	}
	return &found
}
