package main

import (
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"errors"
)

var errUncertainDelivery = errors.New("previous send is unresolved; reconcile its message ID before retrying")
var errSendKeyConflict = errors.New("idempotency key already belongs to a different message")

// A pending row is committed BEFORE network I/O. After a crash or timeout it
// never grants another send, even if the remote server may have accepted it.
// Delivery receipts or an authenticated own-message echo can resolve it.
type outboundMessage struct{ ID, State string }

func (j *eventJournal) prepareSend(account, key, chat, text, proposedID string) (outboundMessage, bool, error) {
	if account == "" || key == "" || len(key) > 256 || chat == "" || text == "" || len(text) > 65536 || proposedID == "" {
		return outboundMessage{}, false, errors.New("invalid outgoing message")
	}
	payload, _ := json.Marshal([]string{chat, text})
	digest := sha256.Sum256(payload)
	hash := hex.EncodeToString(digest[:])
	tx, err := j.db.Begin()
	if err != nil {
		return outboundMessage{}, false, err
	}
	defer tx.Rollback()
	result, err := tx.Exec(`INSERT OR IGNORE INTO outbound_messages(account, request_key, chat, payload_hash, message_id, state) VALUES(?, ?, ?, ?, ?, 'pending')`, account, key, chat, hash, proposedID)
	if err != nil {
		return outboundMessage{}, false, err
	}
	changed, err := result.RowsAffected()
	if err != nil {
		return outboundMessage{}, false, err
	}
	var row outboundMessage
	var oldHash string
	err = tx.QueryRow(`SELECT message_id, state, payload_hash FROM outbound_messages WHERE account=? AND request_key=?`, account, key).Scan(&row.ID, &row.State, &oldHash)
	if err != nil {
		return row, false, err
	}
	if oldHash != hash {
		return row, false, errSendKeyConflict
	}
	if err = tx.Commit(); err != nil {
		return row, false, err
	}
	return row, changed == 1, nil
}

func (j *eventJournal) confirmSend(account, chat, id string) error {
	_, err := j.db.Exec(`UPDATE outbound_messages SET state='sent' WHERE account=? AND chat=? AND message_id=?`, account, chat, id)
	return err
}

func (j *eventJournal) isAgentMessage(account, chat, id string) (bool, error) {
	var found int
	err := j.db.QueryRow(`SELECT 1 FROM outbound_messages WHERE account=? AND chat=? AND message_id=?`, account, chat, id).Scan(&found)
	if errors.Is(err, sql.ErrNoRows) {
		return false, nil
	}
	return found == 1, err
}

func (j *eventJournal) sendOnce(account, key, chat, text, id string, send func(string) error) (string, error) {
	row, fresh, err := j.prepareSend(account, key, chat, text, id)
	if err != nil {
		return "", err
	}
	if !fresh {
		if row.State == "sent" {
			return row.ID, nil
		}
		return row.ID, errUncertainDelivery
	}
	if err := send(row.ID); err != nil {
		return row.ID, errUncertainDelivery
	}
	if err := j.confirmSend(account, chat, row.ID); err != nil {
		return row.ID, errUncertainDelivery
	}
	return row.ID, nil
}

func (j *eventJournal) deliveryStatus(account, key string) (outboundMessage, error) {
	var row outboundMessage
	err := j.db.QueryRow(`SELECT message_id,state FROM outbound_messages WHERE account=? AND request_key=?`, account, key).Scan(&row.ID, &row.State)
	return row, err
}
