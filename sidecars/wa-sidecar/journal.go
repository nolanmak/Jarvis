package main

import (
	"database/sql"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
)

const maxUnackedEvents = 10000
const retainedAckedEvents = 10000

var errJournalFull = errors.New("WhatsApp event journal is full; daemon must replay and acknowledge events")

// eventJournal is the sidecar's durable handoff to the daemon. A received
// message is stored before the sidecar emits it over any socket. The daemon
// acknowledges its sequence only after committing the message to its store.
type eventJournal struct {
	db *sql.DB
}

type journalRow struct {
	Account string
	Seq     int64
	Payload json.RawMessage
}

func openEventJournal(path string) (*eventJournal, error) {
	if err := privateDirectory(filepath.Dir(path)); err != nil {
		return nil, err
	}
	db, err := sql.Open("sqlite", path)
	if err != nil {
		return nil, err
	}
	db.SetMaxOpenConns(1)
	if _, err := db.Exec(`PRAGMA journal_mode=WAL;
        PRAGMA busy_timeout=5000;
        CREATE TABLE IF NOT EXISTS inbound_events (
            seq INTEGER PRIMARY KEY AUTOINCREMENT,
            account TEXT NOT NULL,
            chat TEXT NOT NULL,
            message_id TEXT NOT NULL,
            payload BLOB NOT NULL,
            UNIQUE(account, chat, message_id)
        );
        CREATE TABLE IF NOT EXISTS outbound_messages (
            account TEXT NOT NULL,
            request_key TEXT NOT NULL,
            chat TEXT NOT NULL,
            payload_hash TEXT NOT NULL,
            message_id TEXT NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('pending', 'sent')),
            PRIMARY KEY(account, request_key),
            UNIQUE(account, chat, message_id)
        );
        CREATE TABLE IF NOT EXISTS inbound_cursor (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            seq INTEGER NOT NULL
        );
        INSERT OR IGNORE INTO inbound_cursor(singleton, seq) VALUES(1, 0);`); err != nil {
		db.Close()
		return nil, fmt.Errorf("initialize WhatsApp event journal: %w", err)
	}
	if err := os.Chmod(path, 0600); err != nil {
		db.Close()
		return nil, err
	}
	return &eventJournal{db: db}, nil
}

func (j *eventJournal) Close() error { return j.db.Close() }

func (j *eventJournal) acked() (int64, error) {
	var seq int64
	err := j.db.QueryRow(`SELECT seq FROM inbound_cursor WHERE singleton = 1`).Scan(&seq)
	return seq, err
}

func (j *eventJournal) pendingCount() (int64, error) {
	var pending int64
	err := j.db.QueryRow(`SELECT COUNT(*) FROM inbound_events WHERE seq > (SELECT seq FROM inbound_cursor WHERE singleton = 1)`).Scan(&pending)
	return pending, err
}

func (j *eventJournal) appendMessage(account, chat, messageID string, payload json.RawMessage) (int64, bool, error) {
	if account == "" || chat == "" || messageID == "" || !json.Valid(payload) {
		return 0, false, errors.New("invalid WhatsApp event journal identity or payload")
	}
	tx, err := j.db.Begin()
	if err != nil {
		return 0, false, err
	}
	defer tx.Rollback()
	var seq int64
	err = tx.QueryRow(`SELECT seq FROM inbound_events WHERE account = ? AND chat = ? AND message_id = ?`, account, chat, messageID).Scan(&seq)
	if err == nil {
		return seq, false, tx.Commit()
	}
	if !errors.Is(err, sql.ErrNoRows) {
		return 0, false, err
	}
	var pending int
	if err := tx.QueryRow(`SELECT COUNT(*) FROM inbound_events WHERE seq > (SELECT seq FROM inbound_cursor WHERE singleton = 1)`).Scan(&pending); err != nil {
		return 0, false, err
	}
	if pending >= maxUnackedEvents {
		return 0, false, errJournalFull
	}
	result, err := tx.Exec(`INSERT INTO inbound_events(account, chat, message_id, payload) VALUES(?, ?, ?, ?)`, account, chat, messageID, []byte(payload))
	if err != nil {
		return 0, false, err
	}
	seq, err = result.LastInsertId()
	if err != nil {
		return 0, false, err
	}
	return seq, true, tx.Commit()
}

func (j *eventJournal) readAfter(after int64, limit int) ([]journalRow, error) {
	if after < 0 || limit <= 0 || limit > 1000 {
		return nil, errors.New("invalid WhatsApp event replay cursor or limit")
	}
	rows, err := j.db.Query(`SELECT seq, account, payload FROM inbound_events WHERE seq > ? ORDER BY seq LIMIT ?`, after, limit)
	if err != nil {
		return nil, err
	}
	defer rows.Close()
	result := make([]journalRow, 0)
	for rows.Next() {
		var row journalRow
		if err := rows.Scan(&row.Seq, &row.Account, &row.Payload); err != nil {
			return nil, err
		}
		result = append(result, row)
	}
	return result, rows.Err()
}

func (j *eventJournal) ackThrough(seq int64) error {
	if seq <= 0 {
		return errors.New("ack sequence must be positive")
	}
	tx, err := j.db.Begin()
	if err != nil {
		return err
	}
	defer tx.Rollback()
	var current int64
	if err := tx.QueryRow(`SELECT seq FROM inbound_cursor WHERE singleton = 1`).Scan(&current); err != nil {
		return err
	}
	if seq <= current {
		return tx.Commit()
	}
	var next int64
	if err := tx.QueryRow(`SELECT seq FROM inbound_events WHERE seq > ? ORDER BY seq LIMIT 1`, current).Scan(&next); err != nil {
		if errors.Is(err, sql.ErrNoRows) {
			return errors.New("cannot acknowledge an event that was not journaled")
		}
		return err
	}
	if seq != next {
		return errors.New("acknowledgements must follow journal sequence")
	}
	if _, err := tx.Exec(`UPDATE inbound_cursor SET seq = ? WHERE singleton = 1`, seq); err != nil {
		return err
	}
	if seq > retainedAckedEvents {
		if _, err := tx.Exec(`DELETE FROM inbound_events WHERE seq <= ?`, seq-retainedAckedEvents); err != nil {
			return err
		}
	}
	return tx.Commit()
}
