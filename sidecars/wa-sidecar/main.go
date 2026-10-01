// AugmentAgent WhatsApp sidecar.
//
// Owns the whatsmeow linked-device session: QR pairing, persisted Noise
// store, and the long-lived WhatsApp websocket. Fronts it to the Rust daemon
// via NDJSON over a Unix-domain socket — same wire shape as
// sidecars/browser/sidecar.py (#75 §6), adapted for WhatsApp.
//
// Wire protocol — see crates/augmentagent-channel-whatsapp/src/api.rs:
//
//	Request  : {"version":1,"request_id":"<uuid>","op":"<name>","params":{...}}
//	Success  : {"version":1,"request_id":"...","ok":true,"result":{...}}
//	Failure  : {"version":1,"request_id":"...","ok":false,
//	            "error":{"kind":"NotPaired"|"NotConnected"|"SendFailed"
//	                           |"BadRequest"|"Unavailable"|"Internal","message":"..."}}
//
//	Events (sidecar-initiated, no request_id):
//	  {"version":1,"event":"qr","code":"2@..."}
//	  {"version":1,"event":"pair-success","device_jid":"...","user_jid":"..."}
//	  {"version":1,"event":"connected"}
//	  {"version":1,"event":"disconnected"}
//	  {"version":1,"event":"logged-out","reason":"..."}
//	  {"version":1,"event":"received-message","id":"...","chat":"...","sender":"...",
//	   "push_name":"...","text":"...","timestamp":1700000000,"from_me":false}
//	  {"version":1,"event":"receipt","chat":"...","message_ids":["..."],...}
//
// Ops: status, start_pairing, logout, list_chats, fetch_history, send_text,
// replay_events, ack_events.
//
// Lifecycle: on first run with no stored session the sidecar emits `qr`
// events and retains the latest one for the pairing CLI (#1228). It never
// logs a QR. On pairing, whatsmeow persists the session and emits
// `pair-success`; subsequent starts reconnect. Server logout emits `logged-out`.
//
// Concurrency: the daemon and CLI may connect at the same time. Responses
// return only to their requesting connection; lifecycle events reach both.
// Received messages are journaled before live fan-out. The daemon must replay
// and acknowledge them after its own durable commit; consumer wiring and the
// outbound delivery ledger are tracked in #1229.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"os/signal"
	"path/filepath"
	"runtime"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/proto/waE2E"
	"go.mau.fi/whatsmeow/store/sqlstore"
	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
	waLog "go.mau.fi/whatsmeow/util/log"
	_ "modernc.org/sqlite"
)

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

func socketPath() string {
	if p := os.Getenv("AUGMENTAGENT_WA_SOCK"); p != "" {
		return p
	}
	if runtime.GOOS == "darwin" {
		return socketPathFor("darwin", os.Getuid(), "", false)
	}
	xdgRuntime := os.Getenv("XDG_RUNTIME_DIR")
	linuxRuntime := fmt.Sprintf("/run/user/%d", os.Getuid())
	info, err := os.Stat(linuxRuntime)
	return socketPathFor(runtime.GOOS, os.Getuid(), xdgRuntime, err == nil && info.IsDir())
}

func socketPathFor(goos string, uid int, xdgRuntime string, linuxRuntimeExists bool) string {
	// Darwin's Unix-socket path limit is shorter than Linux's. A short,
	// per-user private directory also works when HOME contains spaces/Unicode.
	if goos == "darwin" {
		return filepath.Join(fmt.Sprintf("/tmp/augmentagent-%d", uid), "wa.sock")
	}
	if xdgRuntime != "" {
		return filepath.Join(xdgRuntime, "augmentagent", "wa.sock")
	}
	if linuxRuntimeExists {
		return filepath.Join(fmt.Sprintf("/run/user/%d", uid), "augmentagent", "wa.sock")
	}
	return filepath.Join(fmt.Sprintf("/tmp/augmentagent-%d", uid), "wa.sock")
}

// The WhatsApp session and socket must be inaccessible to other local users.
// Reject a symlink at the leaf rather than chmoding its target.
func privateDirectory(path string) error {
	if err := os.MkdirAll(path, 0700); err != nil {
		return err
	}
	info, err := os.Lstat(path)
	if err != nil {
		return err
	}
	if !info.IsDir() || info.Mode()&os.ModeSymlink != 0 {
		return fmt.Errorf("%s is not a private directory", path)
	}
	if stat, ok := info.Sys().(*syscall.Stat_t); ok && stat.Uid != uint32(os.Getuid()) {
		return fmt.Errorf("%s is owned by another user", path)
	}
	return os.Chmod(path, 0700)
}

// Session store path. whatsmeow persists the Noise device keys here; this is
// the source of truth for "is a device paired" — the keyring bundle on the
// Rust side is just an index.
func storePath() string {
	if p := os.Getenv("AUGMENTAGENT_WA_STORE"); p != "" {
		return p
	}
	state := os.Getenv("XDG_STATE_HOME")
	if state == "" {
		home, _ := os.UserHomeDir()
		state = filepath.Join(home, ".local", "state")
	}
	return filepath.Join(state, "augmentagent", "whatsmeow.db")
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

type rpcRequest struct {
	Version   int             `json:"version"`
	RequestID string          `json:"request_id"`
	Op        string          `json:"op"`
	Params    json.RawMessage `json:"params"`
}

type rpcError struct {
	Kind    string `json:"kind"`
	Message string `json:"message"`
}

type rpcResponse struct {
	Version   int         `json:"version"`
	RequestID string      `json:"request_id"`
	OK        bool        `json:"ok"`
	Result    interface{} `json:"result,omitempty"`
	Error     *rpcError   `json:"error,omitempty"`
}

// ---------------------------------------------------------------------------
// Sidecar state
// ---------------------------------------------------------------------------

type sidecar struct {
	client          *whatsmeow.Client
	journal         *eventJournal
	journalFailures atomic.Uint64
	pairMu          sync.Mutex
	pairing         bool
	// writeMu protects clients/lastQR and serializes writes to each socket.
	writeMu sync.Mutex
	clients map[net.Conn]struct{}
	lastQR  string
	logger  waLog.Logger
}

func (s *sidecar) marshalFrame(v interface{}) []byte {
	b, err := json.Marshal(v)
	if err != nil {
		if s.logger != nil {
			s.logger.Errorf("marshal frame: %v", err)
		}
		return nil
	}
	return append(b, '\n')
}

// Caller holds writeMu. A failed client cannot hold up later events.
func (s *sidecar) writeLocked(conn net.Conn, frame []byte) {
	if frame == nil {
		return
	}
	_ = conn.SetWriteDeadline(time.Now().Add(5 * time.Second))
	if _, err := conn.Write(frame); err != nil {
		if s.logger != nil {
			s.logger.Warnf("write frame: %v", err)
		}
		delete(s.clients, conn)
		_ = conn.Close()
	}
}

func (s *sidecar) writeFrameTo(conn net.Conn, v interface{}) {
	frame := s.marshalFrame(v)
	s.writeMu.Lock()
	defer s.writeMu.Unlock()
	if _, connected := s.clients[conn]; connected {
		s.writeLocked(conn, frame)
	}
}

func (s *sidecar) registerConn(conn net.Conn) {
	s.writeMu.Lock()
	defer s.writeMu.Unlock()
	if s.clients == nil {
		s.clients = make(map[net.Conn]struct{})
	}
	s.clients[conn] = struct{}{}
	if s.lastQR != "" {
		s.writeLocked(conn, s.marshalFrame(map[string]interface{}{
			"version": 1, "event": "qr", "code": s.lastQR,
		}))
	}
}

func (s *sidecar) unregisterConn(conn net.Conn) {
	s.writeMu.Lock()
	delete(s.clients, conn)
	s.writeMu.Unlock()
	_ = conn.Close()
}

func (s *sidecar) emitEvent(ev map[string]interface{}) {
	ev["version"] = 1
	if ev["event"] == "received-message" && s.journal != nil {
		account := ""
		if s.client != nil && s.client.Store.ID != nil {
			account = s.client.Store.ID.String()
		}
		chat, _ := ev["chat"].(string)
		messageID, _ := ev["id"].(string)
		payload, err := json.Marshal(ev)
		if err != nil {
			s.journalFailures.Add(1)
			s.logger.Errorf("marshal inbound WhatsApp event: %v", err)
			return
		}
		seq, fresh, err := s.journal.appendMessage(account, chat, messageID, payload)
		if err != nil {
			s.journalFailures.Add(1)
			s.logger.Errorf("persist inbound WhatsApp event before delivery: %v", err)
			return
		}
		if !fresh {
			return
		}
		ev["seq"] = seq
	}
	frame := s.marshalFrame(ev)
	s.writeMu.Lock()
	defer s.writeMu.Unlock()
	if ev["event"] == "qr" {
		if code, ok := ev["code"].(string); ok {
			s.lastQR = code
		}
	}
	if ev["event"] == "pair-success" || ev["event"] == "logged-out" {
		s.lastQR = ""
	}
	for conn := range s.clients {
		s.writeLocked(conn, frame)
	}
}

func (s *sidecar) ok(conn net.Conn, reqID string, result interface{}) {
	s.writeFrameTo(conn, rpcResponse{Version: 1, RequestID: reqID, OK: true, Result: result})
}

func (s *sidecar) fail(conn net.Conn, reqID, kind, msg string) {
	s.writeFrameTo(conn, rpcResponse{
		Version:   1,
		RequestID: reqID,
		OK:        false,
		Error:     &rpcError{Kind: kind, Message: msg},
	})
}

// ---------------------------------------------------------------------------
// whatsmeow event handler — translates inbound messages + lifecycle into the
// NDJSON event frames the Rust channel/control surface consume.
// ---------------------------------------------------------------------------

func (s *sidecar) handleWAEvent(rawEvt interface{}) {
	switch evt := rawEvt.(type) {
	case *events.Message:
		text := extractText(evt)
		contextInfo := messageContext(evt.Message)
		var quotedID string
		var mentions []string
		if contextInfo != nil {
			quotedID = contextInfo.GetStanzaID()
			mentions = contextInfo.GetMentionedJID()
		}
		if mentions == nil {
			mentions = []string{}
		}
		protocol := evt.Message.GetProtocolMessage()
		isRevoke := protocol != nil && protocol.GetType() == waE2E.ProtocolMessage_REVOKE
		if isRevoke {
			quotedID = protocol.GetKey().GetID()
		}
		s.emitEvent(map[string]interface{}{
			"event":             "received-message",
			"id":                evt.Info.ID,
			"chat":              evt.Info.Chat.String(),
			"sender":            evt.Info.Sender.String(),
			"push_name":         evt.Info.PushName,
			"text":              text,
			"timestamp":         evt.Info.Timestamp.Unix(),
			"from_me":           evt.Info.IsFromMe,
			"quoted_message_id": quotedID,
			"mentioned_jids":    mentions,
			"media":             mediaDescriptor(evt.Message),
			"is_edit":           evt.IsEdit,
			"is_revoke":         isRevoke,
			"is_view_once":      evt.IsViewOnce || evt.IsViewOnceV2 || evt.IsViewOnceV2Extension,
			"is_ephemeral":      evt.IsEphemeral,
		})
	case *events.Receipt:
		ids := make([]string, len(evt.MessageIDs))
		for i, id := range evt.MessageIDs {
			ids[i] = string(id)
		}
		s.emitEvent(map[string]interface{}{
			"event":        "receipt",
			"chat":         evt.Chat.String(),
			"sender":       evt.Sender.String(),
			"message_ids":  ids,
			"receipt_type": receiptTypeName(evt.Type),
			"timestamp":    evt.Timestamp.Unix(),
		})
	case *events.Connected:
		s.emitEvent(map[string]interface{}{"event": "connected"})
	case *events.Disconnected:
		s.emitEvent(map[string]interface{}{"event": "disconnected"})
	case *events.PairSuccess:
		s.emitEvent(map[string]interface{}{
			"event":      "pair-success",
			"device_jid": evt.ID.String(),
			"user_jid":   evt.ID.ToNonAD().String(),
		})
	case *events.LoggedOut:
		s.emitEvent(map[string]interface{}{
			"event":  "logged-out",
			"reason": fmt.Sprintf("%v", evt.Reason),
		})
	}
}

func receiptTypeName(kind types.ReceiptType) string {
	switch kind {
	case types.ReceiptTypeDelivered:
		return "delivered"
	case types.ReceiptTypeRead:
		return "read"
	case types.ReceiptTypeReadSelf:
		return "read-self"
	case types.ReceiptTypePlayed:
		return "played"
	case types.ReceiptTypeRetry:
		return "retry"
	case types.ReceiptTypeSender:
		return "sender"
	default:
		return string(kind)
	}
}

// extractText preserves captions so a media-only message still has the text
// its sender wrote. The media descriptor travels separately.
func extractText(evt *events.Message) string {
	m := evt.Message
	if m == nil {
		return ""
	}
	if c := m.GetConversation(); c != "" {
		return c
	}
	if e := m.GetExtendedTextMessage(); e != nil {
		return e.GetText()
	}
	if image := m.GetImageMessage(); image != nil {
		return image.GetCaption()
	}
	if document := m.GetDocumentMessage(); document != nil {
		return document.GetCaption()
	}
	if video := m.GetVideoMessage(); video != nil {
		return video.GetCaption()
	}
	return ""
}

func messageContext(m *waE2E.Message) *waE2E.ContextInfo {
	if m == nil {
		return nil
	}
	if v := m.GetExtendedTextMessage(); v != nil {
		return v.GetContextInfo()
	}
	if v := m.GetImageMessage(); v != nil {
		return v.GetContextInfo()
	}
	if v := m.GetDocumentMessage(); v != nil {
		return v.GetContextInfo()
	}
	if v := m.GetVideoMessage(); v != nil {
		return v.GetContextInfo()
	}
	if v := m.GetAudioMessage(); v != nil {
		return v.GetContextInfo()
	}
	if v := m.GetStickerMessage(); v != nil {
		return v.GetContextInfo()
	}
	return nil
}

func mediaDescriptor(m *waE2E.Message) interface{} {
	if m == nil {
		return nil
	}
	if v := m.GetImageMessage(); v != nil {
		return map[string]interface{}{"kind": "image", "mime_type": v.GetMimetype(), "size": v.GetFileLength()}
	}
	if v := m.GetDocumentMessage(); v != nil {
		return map[string]interface{}{"kind": "document", "mime_type": v.GetMimetype(), "file_name": v.GetFileName(), "size": v.GetFileLength()}
	}
	if v := m.GetVideoMessage(); v != nil {
		return map[string]interface{}{"kind": "video", "mime_type": v.GetMimetype(), "size": v.GetFileLength(), "duration_secs": v.GetSeconds()}
	}
	if v := m.GetAudioMessage(); v != nil {
		return map[string]interface{}{"kind": "audio", "mime_type": v.GetMimetype(), "size": v.GetFileLength(), "duration_secs": v.GetSeconds(), "voice_note": v.GetPTT()}
	}
	if v := m.GetStickerMessage(); v != nil {
		return map[string]interface{}{"kind": "sticker", "mime_type": v.GetMimetype(), "size": v.GetFileLength()}
	}
	return nil
}

// ---------------------------------------------------------------------------
// Op dispatch
// ---------------------------------------------------------------------------

func (s *sidecar) dispatch(conn net.Conn, req rpcRequest) {
	if req.Version != 1 {
		s.fail(conn, req.RequestID, "BadRequest", "unsupported protocol version")
		return
	}
	switch req.Op {
	case "status":
		s.opStatus(conn, req)
	case "start_pairing":
		s.opStartPairing(conn, req)
	case "logout":
		s.opLogout(conn, req)
	case "list_chats":
		s.opListChats(conn, req)
	case "fetch_history":
		s.opFetchHistory(conn, req)
	case "send_text":
		s.opSendText(conn, req)
	case "replay_events":
		s.opReplayEvents(conn, req)
	case "ack_events":
		s.opAckEvents(conn, req)
	default:
		s.fail(conn, req.RequestID, "BadRequest", "unknown op: "+req.Op)
	}
}

func (s *sidecar) startPairing() error {
	if s.client == nil {
		return fmt.Errorf("whatsapp client is unavailable")
	}
	s.pairMu.Lock()
	defer s.pairMu.Unlock()
	if s.client.Store.ID != nil || s.pairing {
		return nil
	}
	qrChan, err := s.client.GetQRChannel(context.Background())
	if err != nil {
		return err
	}
	if err := s.client.Connect(); err != nil {
		return err
	}
	s.pairing = true
	go func() {
		defer func() {
			s.pairMu.Lock()
			s.pairing = false
			s.pairMu.Unlock()
		}()
		for evt := range qrChan {
			if evt.Event == "code" {
				// Never print the QR or session material into service logs.
				s.emitEvent(map[string]interface{}{"event": "qr", "code": evt.Code})
			} else if s.logger != nil {
				s.logger.Infof("pair flow: %s", evt.Event)
			}
		}
	}()
	return nil
}

func (s *sidecar) opStartPairing(conn net.Conn, req rpcRequest) {
	if s.client == nil {
		s.fail(conn, req.RequestID, "NotConnected", "whatsapp client is unavailable")
		return
	}
	if err := s.startPairing(); err != nil {
		s.fail(conn, req.RequestID, "NotConnected", "start pairing: "+err.Error())
		return
	}
	s.ok(conn, req.RequestID, map[string]interface{}{"paired": s.client.Store.ID != nil})
}

func (s *sidecar) opLogout(conn net.Conn, req rpcRequest) {
	var params struct {
		ExpectedDeviceJID string `json:"expected_device_jid"`
	}
	if err := json.Unmarshal(req.Params, &params); err != nil || params.ExpectedDeviceJID == "" {
		s.fail(conn, req.RequestID, "BadRequest", "expected_device_jid required")
		return
	}
	if s.client == nil || s.client.Store.ID == nil {
		s.fail(conn, req.RequestID, "NotPaired", "no linked device")
		return
	}
	if actual := s.client.Store.ID.String(); actual != params.ExpectedDeviceJID {
		s.fail(conn, req.RequestID, "BadRequest", "linked device does not match expected_device_jid")
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	if err := s.client.Logout(ctx); err != nil {
		s.fail(conn, req.RequestID, "LogoutFailed", err.Error())
		return
	}
	s.emitEvent(map[string]interface{}{"event": "logged-out", "reason": "user_initiated"})
	s.ok(conn, req.RequestID, map[string]interface{}{"unlinked_device_jid": params.ExpectedDeviceJID})
}

func (s *sidecar) opStatus(conn net.Conn, req rpcRequest) {
	journalStatus := map[string]interface{}{}
	if s.journal != nil {
		journalStatus["event_journal_failures"] = s.journalFailures.Load()
		if acked, err := s.journal.acked(); err == nil {
			journalStatus["events_acked_through"] = acked
		}
		if pending, err := s.journal.pendingCount(); err == nil {
			journalStatus["events_pending"] = pending
		}
	}
	if s.client == nil {
		journalStatus["paired"] = false
		journalStatus["connected"] = false
		journalStatus["device_jid"] = ""
		s.ok(conn, req.RequestID, journalStatus)
		return
	}
	paired := s.client.Store.ID != nil
	connected := s.client.IsConnected()
	var deviceJID string
	if s.client.Store.ID != nil {
		deviceJID = s.client.Store.ID.String()
	}
	journalStatus["paired"] = paired
	journalStatus["connected"] = connected
	journalStatus["device_jid"] = deviceJID
	s.ok(conn, req.RequestID, journalStatus)
}

func (s *sidecar) opReplayEvents(conn net.Conn, req rpcRequest) {
	if s.journal == nil {
		s.fail(conn, req.RequestID, "Unavailable", "event journal is not configured")
		return
	}
	var p struct {
		After int64 `json:"after"`
		Limit int   `json:"limit"`
	}
	if err := json.Unmarshal(req.Params, &p); err != nil {
		s.fail(conn, req.RequestID, "BadRequest", "invalid replay parameters")
		return
	}
	if p.Limit == 0 {
		p.Limit = 100
	}
	acked, err := s.journal.acked()
	if err != nil {
		s.fail(conn, req.RequestID, "Internal", "read event cursor failed")
		return
	}
	if p.After < acked {
		p.After = acked
	}
	rows, err := s.journal.readAfter(p.After, p.Limit)
	if err != nil {
		s.fail(conn, req.RequestID, "BadRequest", err.Error())
		return
	}
	events := make([]map[string]interface{}, 0, len(rows))
	for _, row := range rows {
		var event map[string]interface{}
		if err := json.Unmarshal(row.Payload, &event); err != nil {
			s.fail(conn, req.RequestID, "Internal", "stored event is corrupt")
			return
		}
		event["seq"] = row.Seq
		events = append(events, event)
	}
	s.ok(conn, req.RequestID, map[string]interface{}{"events": events})
}

func (s *sidecar) opAckEvents(conn net.Conn, req rpcRequest) {
	if s.journal == nil {
		s.fail(conn, req.RequestID, "Unavailable", "event journal is not configured")
		return
	}
	var p struct {
		Through int64 `json:"through"`
	}
	if err := json.Unmarshal(req.Params, &p); err != nil {
		s.fail(conn, req.RequestID, "BadRequest", "invalid ack parameters")
		return
	}
	if err := s.journal.ackThrough(p.Through); err != nil {
		s.fail(conn, req.RequestID, "BadRequest", err.Error())
		return
	}
	s.ok(conn, req.RequestID, map[string]interface{}{"acked_through": p.Through})
}

func (s *sidecar) opListChats(conn net.Conn, req rpcRequest) {
	var p struct {
		Limit int `json:"limit"`
	}
	_ = json.Unmarshal(req.Params, &p)
	if p.Limit <= 0 {
		p.Limit = 50
	}
	if s.client == nil || s.client.Store.ID == nil {
		s.fail(conn, req.RequestID, "NotPaired", "no linked device")
		return
	}
	// whatsmeow doesn't expose a server-side chat list; the closest source
	// is the contact store. We surface known contacts as chat candidates;
	// the Rust side dedups against the emails table for "active" chats.
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	contacts, err := s.client.Store.Contacts.GetAllContacts(ctx)
	if err != nil {
		s.fail(conn, req.RequestID, "Internal", "GetAllContacts: "+err.Error())
		return
	}
	chats := make([]map[string]interface{}, 0, len(contacts))
	for jid, info := range contacts {
		if jid.Server != types.DefaultUserServer {
			continue // 1:1 only
		}
		name := info.FullName
		if name == "" {
			name = info.PushName
		}
		chats = append(chats, map[string]interface{}{
			"jid":             jid.String(),
			"name":            name,
			"last_message_at": 0,
		})
		if len(chats) >= p.Limit {
			break
		}
	}
	s.ok(conn, req.RequestID, map[string]interface{}{"chats": chats})
}

func (s *sidecar) opFetchHistory(conn net.Conn, req rpcRequest) {
	var p struct {
		ChatJID string `json:"chat_jid"`
		Limit   int    `json:"limit"`
	}
	if err := json.Unmarshal(req.Params, &p); err != nil || p.ChatJID == "" {
		s.fail(conn, req.RequestID, "BadRequest", "chat_jid required")
		return
	}
	// This process has no durable history store yet. A successful empty result
	// would tell the caller that a chat has no messages, which is false.
	s.fail(conn, req.RequestID, "Unavailable", "chat history is not stored by this sidecar")
}

func (s *sidecar) opSendText(conn net.Conn, req rpcRequest) {
	var p struct {
		ChatJID string `json:"chat_jid"`
		Text    string `json:"text"`
	}
	if err := json.Unmarshal(req.Params, &p); err != nil || p.ChatJID == "" || p.Text == "" {
		s.fail(conn, req.RequestID, "BadRequest", "chat_jid and text required")
		return
	}
	if s.client == nil || s.client.Store.ID == nil {
		s.fail(conn, req.RequestID, "NotPaired", "no linked device")
		return
	}
	if !s.client.IsConnected() {
		s.fail(conn, req.RequestID, "NotConnected", "websocket not connected")
		return
	}
	jid, err := types.ParseJID(p.ChatJID)
	if err != nil {
		s.fail(conn, req.RequestID, "BadRequest", "bad jid: "+err.Error())
		return
	}
	msg := &waE2E.Message{Conversation: &p.Text}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	resp, err := s.client.SendMessage(ctx, jid, msg)
	if err != nil {
		s.fail(conn, req.RequestID, "SendFailed", err.Error())
		return
	}
	s.ok(conn, req.RequestID, map[string]interface{}{"message_id": resp.ID})
}

// ---------------------------------------------------------------------------
// Connection loop
// ---------------------------------------------------------------------------

func (s *sidecar) serveConn(conn net.Conn) {
	s.registerConn(conn)
	defer s.unregisterConn(conn)

	scanner := bufio.NewScanner(conn)
	scanner.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	for scanner.Scan() {
		line := scanner.Bytes()
		if len(line) == 0 {
			continue
		}
		var req rpcRequest
		if err := json.Unmarshal(line, &req); err != nil {
			s.fail(conn, "", "BadRequest", "bad json: "+err.Error())
			continue
		}
		go s.dispatch(conn, req)
	}
	if err := scanner.Err(); err != nil {
		s.fail(conn, "", "BadRequest", "invalid or oversized request frame")
	}
}

func main() {
	logger := waLog.Stdout("wa-sidecar", "INFO", true)
	// The contract harness runs the actual process/socket without contacting
	// WhatsApp or loading a linked-device session.
	if len(os.Args) == 2 && os.Args[1] == "--offline-test" {
		serve(&sidecar{logger: logger})
		return
	}

	if err := privateDirectory(filepath.Dir(storePath())); err != nil {
		logger.Errorf("mkdir store dir: %v", err)
		os.Exit(1)
	}
	container, err := sqlstore.New(context.Background(), "sqlite",
		"file:"+storePath()+"?_pragma=foreign_keys(1)", logger)
	if err != nil {
		logger.Errorf("open sqlstore: %v", err)
		os.Exit(1)
	}
	deviceStore, err := container.GetFirstDevice(context.Background())
	if err != nil {
		logger.Errorf("get device: %v", err)
		os.Exit(1)
	}

	client := whatsmeow.NewClient(deviceStore, logger)
	journal, err := openEventJournal(storePath() + ".events")
	if err != nil {
		logger.Errorf("open durable WhatsApp event journal: %v", err)
		os.Exit(1)
	}
	defer journal.Close()
	s := &sidecar{client: client, journal: journal, logger: logger}
	client.AddEventHandler(s.handleWAEvent)

	// Pairing vs. reconnect.
	if client.Store.ID == nil {
		if err := s.startPairing(); err != nil {
			logger.Errorf("start pairing: %v", err)
			os.Exit(1)
		}
	} else {
		if err := client.Connect(); err != nil {
			logger.Errorf("connect (reconnect): %v", err)
			os.Exit(1)
		}
	}

	serve(s)
}

type ownedSocket struct {
	net.Listener
	lock *os.File
}

func (s *ownedSocket) Close() error {
	err := s.Listener.Close()
	_ = syscall.Flock(int(s.lock.Fd()), syscall.LOCK_UN)
	_ = s.lock.Close()
	return err
}

func listenSocket(sock string) (net.Listener, error) {
	// Hold a per-socket process lock through the listener lifetime. Without
	// this, two simultaneous starts can both decide an old socket is stale.
	lock, err := os.OpenFile(sock+".lock", os.O_CREATE|os.O_RDWR, 0600)
	if err != nil {
		return nil, fmt.Errorf("open sidecar socket lock: %w", err)
	}
	if err := syscall.Flock(int(lock.Fd()), syscall.LOCK_EX|syscall.LOCK_NB); err != nil {
		_ = lock.Close()
		return nil, fmt.Errorf("sidecar socket is already owned: %w", err)
	}
	keepLock := false
	defer func() {
		if !keepLock {
			_ = syscall.Flock(int(lock.Fd()), syscall.LOCK_UN)
			_ = lock.Close()
		}
	}()
	if info, err := os.Lstat(sock); err == nil {
		if info.Mode()&os.ModeSocket == 0 {
			return nil, fmt.Errorf("refusing to replace non-socket path %s", sock)
		}
		// A CLI can attach to the running sidecar. Starting another sidecar
		// must not remove its active listening socket.
		if conn, dialErr := net.DialTimeout("unix", sock, 250*time.Millisecond); dialErr == nil {
			_ = conn.Close()
			return nil, fmt.Errorf("sidecar already listening on %s", sock)
		}
		if err := os.Remove(sock); err != nil {
			return nil, fmt.Errorf("remove stale socket: %w", err)
		}
	} else if !os.IsNotExist(err) {
		return nil, fmt.Errorf("inspect socket path: %w", err)
	}
	listener, err := net.Listen("unix", sock)
	if err != nil {
		return nil, err
	}
	keepLock = true
	return &ownedSocket{Listener: listener, lock: lock}, nil
}

func serve(s *sidecar) {
	logger := s.logger
	// UDS listener.
	sock := socketPath()
	if err := privateDirectory(filepath.Dir(sock)); err != nil {
		logger.Errorf("mkdir sock dir: %v", err)
		os.Exit(1)
	}
	ln, err := listenSocket(sock)
	if err != nil {
		logger.Errorf("listen: %v", err)
		os.Exit(1)
	}
	if err := os.Chmod(sock, 0o600); err != nil {
		logger.Warnf("chmod sock: %v", err)
	}
	logger.Infof("listening on %s (store=%s)", sock, storePath())

	// Graceful shutdown.
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGINT, syscall.SIGTERM)
	go func() {
		<-sigCh
		logger.Infof("shutdown signal received")
		_ = ln.Close()
		if s.client != nil {
			s.client.Disconnect()
		}
		_ = os.Remove(sock)
		os.Exit(0)
	}()

	for {
		conn, err := ln.Accept()
		if err != nil {
			logger.Warnf("accept: %v", err)
			time.Sleep(200 * time.Millisecond)
			continue
		}
		go s.serveConn(conn)
	}
}
