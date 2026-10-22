package main

import (
	"crypto/sha256"
	"database/sql"
	"encoding/json"
	"fmt"
	"go.mau.fi/whatsmeow/types"
	"net"
)

type deliveryRecord struct {
	ID   string `json:"message_id"`
	Sent bool   `json:"sent"`
	hash string
}

func (l *outboundLedger) reserveDelivery(account, chat, key, text, id string) (*deliveryRecord, bool, error) {
	if account == "" || chat == "" || key == "" || len(key) > 256 || id == "" {
		return nil, false, fmt.Errorf("invalid delivery identity")
	}
	hash := fmt.Sprintf("%x", sha256.Sum256([]byte(text)))
	tx, err := l.db.Begin()
	if err != nil {
		return nil, false, err
	}
	defer tx.Rollback()
	result, err := tx.Exec("INSERT INTO agent_deliveries(account,chat,delivery_key,message_id,content_hash,sent) VALUES(?,?,?,?,?,0) ON CONFLICT(account,chat,delivery_key) DO NOTHING", account, chat, key, id, hash)
	if err != nil {
		return nil, false, err
	}
	n, err := result.RowsAffected()
	if err != nil {
		return nil, false, err
	}
	rec := &deliveryRecord{}
	if err = tx.QueryRow("SELECT message_id,content_hash,sent FROM agent_deliveries WHERE account=? AND chat=? AND delivery_key=?", account, chat, key).Scan(&rec.ID, &rec.hash, &rec.Sent); err != nil {
		return nil, false, err
	}
	if rec.hash != hash {
		return nil, false, fmt.Errorf("delivery content conflict")
	}
	if n == 1 {
		if _, err = tx.Exec("INSERT INTO agent_outbound(account,chat,message_id) VALUES(?,?,?)", account, chat, id); err != nil {
			return nil, false, err
		}
	}
	if err = tx.Commit(); err != nil {
		return nil, false, err
	}
	return rec, n == 1, nil
}
func (l *outboundLedger) confirmDelivery(account, chat, key, id string) error {
	result, err := l.db.Exec("UPDATE agent_deliveries SET sent=1 WHERE account=? AND chat=? AND delivery_key=? AND message_id=?", account, chat, key, id)
	if err != nil {
		return err
	}
	n, err := result.RowsAffected()
	if err != nil {
		return err
	}
	if n != 1 {
		return fmt.Errorf("delivery identity mismatch")
	}
	return nil
}
func (l *outboundLedger) lookupDelivery(account, chat, key string) (*deliveryRecord, error) {
	rec := &deliveryRecord{}
	err := l.db.QueryRow("SELECT message_id,content_hash,sent FROM agent_deliveries WHERE account=? AND chat=? AND delivery_key=?", account, chat, key).Scan(&rec.ID, &rec.hash, &rec.Sent)
	if err == sql.ErrNoRows {
		return nil, nil
	}
	return rec, err
}
func (s *sidecar) opLookupDelivery(conn net.Conn, req rpcRequest) {
	var p struct {
		Account string `json:"account"`
		Chat    string `json:"chat_jid"`
		Key     string `json:"delivery_key"`
	}
	if json.Unmarshal(req.Params, &p) != nil || p.Account == "" || p.Chat == "" || p.Key == "" || len(p.Key) > 256 {
		s.fail(conn, req.RequestID, "BadRequest", "invalid delivery identity")
		return
	}
	if s.client == nil || s.client.Store.ID == nil || s.client.Store.ID.ToNonAD().String() != p.Account {
		s.fail(conn, req.RequestID, "BadRequest", "linked account mismatch")
		return
	}
	chat, err := types.ParseJID(p.Chat)
	if err != nil {
		s.fail(conn, req.RequestID, "BadRequest", "invalid chat")
		return
	}
	if s.outbound == nil {
		s.fail(conn, req.RequestID, "Internal", "delivery store unavailable")
		return
	}
	rec, err := s.outbound.lookupDelivery(p.Account, chat.ToNonAD().String(), p.Key)
	if err != nil {
		s.fail(conn, req.RequestID, "Internal", "delivery lookup failed")
		return
	}
	if rec == nil {
		s.ok(conn, req.RequestID, map[string]interface{}{"sent": false, "message_id": ""})
		return
	}
	s.ok(conn, req.RequestID, rec)
}
