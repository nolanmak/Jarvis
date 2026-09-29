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
// Ops: status, list_chats, fetch_history, send_text.
//
// Lifecycle: on first run with no stored session the sidecar emits `qr`
// events and retains the latest one for the pairing CLI (#1228). It never
// logs a QR. On pairing, whatsmeow persists the session and emits
// `pair-success`; subsequent starts reconnect. Server logout emits `logged-out`.
//
// Concurrency: one accepted connection at a time. Each request line is
// dispatched on its own goroutine; writes are serialized by a mutex.
// Daemon/CLI multiplexing and durable replay are tracked in #1229.
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
	"sync"
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
	runtime := os.Getenv("XDG_RUNTIME_DIR")
	if runtime == "" {
		runtime = fmt.Sprintf("/run/user/%d", os.Getuid())
		if info, err := os.Stat(runtime); err != nil || !info.IsDir() {
			return filepath.Join(fmt.Sprintf("/tmp/augmentagent-%d", os.Getuid()), "wa.sock")
		}
	}
	return filepath.Join(runtime, "augmentagent", "wa.sock")
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
	client *whatsmeow.Client
	// writeMu serializes all frames written to the active connection.
	writeMu sync.Mutex
	conn    net.Conn
	lastQR  string
	logger  waLog.Logger
}

func (s *sidecar) writeFrame(v interface{}) {
	s.writeMu.Lock()
	defer s.writeMu.Unlock()
	if s.conn == nil {
		return
	}
	b, err := json.Marshal(v)
	if err != nil {
		if s.logger != nil {
			s.logger.Errorf("marshal frame: %v", err)
		}
		return
	}
	b = append(b, '\n')
	_ = s.conn.SetWriteDeadline(time.Now().Add(5 * time.Second))
	if _, err := s.conn.Write(b); err != nil {
		if s.logger != nil {
			s.logger.Warnf("write frame: %v", err)
		}
	}
}

func (s *sidecar) emitEvent(ev map[string]interface{}) {
	ev["version"] = 1
	if ev["event"] == "qr" {
		if code, ok := ev["code"].(string); ok {
			s.writeMu.Lock()
			s.lastQR = code
			s.writeMu.Unlock()
		}
	}
	if ev["event"] == "pair-success" || ev["event"] == "logged-out" {
		s.writeMu.Lock()
		s.lastQR = ""
		s.writeMu.Unlock()
	}
	s.writeFrame(ev)
}

func (s *sidecar) ok(reqID string, result interface{}) {
	s.writeFrame(rpcResponse{Version: 1, RequestID: reqID, OK: true, Result: result})
}

func (s *sidecar) fail(reqID, kind, msg string) {
	s.writeFrame(rpcResponse{
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

func (s *sidecar) dispatch(req rpcRequest) {
	if req.Version != 1 {
		s.fail(req.RequestID, "BadRequest", "unsupported protocol version")
		return
	}
	switch req.Op {
	case "status":
		s.opStatus(req)
	case "list_chats":
		s.opListChats(req)
	case "fetch_history":
		s.opFetchHistory(req)
	case "send_text":
		s.opSendText(req)
	default:
		s.fail(req.RequestID, "BadRequest", "unknown op: "+req.Op)
	}
}

func (s *sidecar) opStatus(req rpcRequest) {
	if s.client == nil {
		s.ok(req.RequestID, map[string]interface{}{
			"paired": false, "connected": false, "device_jid": "",
		})
		return
	}
	paired := s.client.Store.ID != nil
	connected := s.client.IsConnected()
	var deviceJID string
	if s.client.Store.ID != nil {
		deviceJID = s.client.Store.ID.String()
	}
	s.ok(req.RequestID, map[string]interface{}{
		"paired":     paired,
		"connected":  connected,
		"device_jid": deviceJID,
	})
}

func (s *sidecar) opListChats(req rpcRequest) {
	var p struct {
		Limit int `json:"limit"`
	}
	_ = json.Unmarshal(req.Params, &p)
	if p.Limit <= 0 {
		p.Limit = 50
	}
	if s.client == nil || s.client.Store.ID == nil {
		s.fail(req.RequestID, "NotPaired", "no linked device")
		return
	}
	// whatsmeow doesn't expose a server-side chat list; the closest source
	// is the contact store. We surface known contacts as chat candidates;
	// the Rust side dedups against the emails table for "active" chats.
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	contacts, err := s.client.Store.Contacts.GetAllContacts(ctx)
	if err != nil {
		s.fail(req.RequestID, "Internal", "GetAllContacts: "+err.Error())
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
	s.ok(req.RequestID, map[string]interface{}{"chats": chats})
}

func (s *sidecar) opFetchHistory(req rpcRequest) {
	var p struct {
		ChatJID string `json:"chat_jid"`
		Limit   int    `json:"limit"`
	}
	if err := json.Unmarshal(req.Params, &p); err != nil || p.ChatJID == "" {
		s.fail(req.RequestID, "BadRequest", "chat_jid required")
		return
	}
	// This process has no durable history store yet. A successful empty result
	// would tell the caller that a chat has no messages, which is false.
	s.fail(req.RequestID, "Unavailable", "chat history is not stored by this sidecar")
}

func (s *sidecar) opSendText(req rpcRequest) {
	var p struct {
		ChatJID string `json:"chat_jid"`
		Text    string `json:"text"`
	}
	if err := json.Unmarshal(req.Params, &p); err != nil || p.ChatJID == "" || p.Text == "" {
		s.fail(req.RequestID, "BadRequest", "chat_jid and text required")
		return
	}
	if s.client == nil || s.client.Store.ID == nil {
		s.fail(req.RequestID, "NotPaired", "no linked device")
		return
	}
	if !s.client.IsConnected() {
		s.fail(req.RequestID, "NotConnected", "websocket not connected")
		return
	}
	jid, err := types.ParseJID(p.ChatJID)
	if err != nil {
		s.fail(req.RequestID, "BadRequest", "bad jid: "+err.Error())
		return
	}
	msg := &waE2E.Message{Conversation: &p.Text}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	resp, err := s.client.SendMessage(ctx, jid, msg)
	if err != nil {
		s.fail(req.RequestID, "SendFailed", err.Error())
		return
	}
	s.ok(req.RequestID, map[string]interface{}{"message_id": resp.ID})
}

// ---------------------------------------------------------------------------
// Connection loop
// ---------------------------------------------------------------------------

func (s *sidecar) serveConn(conn net.Conn) {
	s.writeMu.Lock()
	s.conn = conn
	lastQR := s.lastQR
	s.writeMu.Unlock()
	if lastQR != "" {
		s.writeFrame(map[string]interface{}{"version": 1, "event": "qr", "code": lastQR})
	}
	defer func() {
		s.writeMu.Lock()
		s.conn = nil
		s.writeMu.Unlock()
		_ = conn.Close()
	}()

	scanner := bufio.NewScanner(conn)
	scanner.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
	for scanner.Scan() {
		line := scanner.Bytes()
		if len(line) == 0 {
			continue
		}
		var req rpcRequest
		if err := json.Unmarshal(line, &req); err != nil {
			s.fail("", "BadRequest", "bad json: "+err.Error())
			continue
		}
		go s.dispatch(req)
	}
	if err := scanner.Err(); err != nil {
		s.fail("", "BadRequest", "invalid or oversized request frame")
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
	s := &sidecar{client: client, logger: logger}
	client.AddEventHandler(s.handleWAEvent)

	// Pairing vs. reconnect.
	if client.Store.ID == nil {
		qrChan, err := client.GetQRChannel(context.Background())
		if err != nil {
			logger.Errorf("start QR pairing: %v", err)
			os.Exit(1)
		}
		if err := client.Connect(); err != nil {
			logger.Errorf("connect (pairing): %v", err)
			os.Exit(1)
		}
		go func() {
			for evt := range qrChan {
				if evt.Event == "code" {
					// Keep the latest code for a CLI that connects after pairing began.
					// Never print QR/session material into service logs.
					s.emitEvent(map[string]interface{}{
						"event": "qr",
						"code":  evt.Code,
					})
				} else {
					logger.Infof("pair flow: %s", evt.Event)
				}
			}
		}()
	} else {
		if err := client.Connect(); err != nil {
			logger.Errorf("connect (reconnect): %v", err)
			os.Exit(1)
		}
	}

	serve(s)
}

func serve(s *sidecar) {
	logger := s.logger
	// UDS listener.
	sock := socketPath()
	if err := privateDirectory(filepath.Dir(sock)); err != nil {
		logger.Errorf("mkdir sock dir: %v", err)
		os.Exit(1)
	}
	if info, err := os.Lstat(sock); err == nil {
		if info.Mode()&os.ModeSocket == 0 {
			logger.Errorf("refusing to replace non-socket path %s", sock)
			os.Exit(1)
		}
		if err := os.Remove(sock); err != nil {
			logger.Errorf("remove stale socket: %v", err)
			os.Exit(1)
		}
	} else if !os.IsNotExist(err) {
		logger.Errorf("inspect socket path: %v", err)
		os.Exit(1)
	}
	ln, err := net.Listen("unix", sock)
	if err != nil {
		logger.Errorf("listen %s: %v", sock, err)
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
		// One client (the daemon) at a time — serve synchronously so a new
		// connection replaces the old write target cleanly.
		s.serveConn(conn)
	}
}
