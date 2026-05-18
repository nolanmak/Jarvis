module github.com/nolanmak/AugmentAgent/sidecars/wa-sidecar

go 1.22

// Build (once a Go toolchain is present on the box):
//   cd sidecars/wa-sidecar && go mod tidy && go build -o wa-sidecar .
//
// `go mod tidy` resolves exact pinned versions + generates go.sum. We don't
// commit go.sum here because it can't be produced without the toolchain on
// this host (see #74 — Go sidecar build pending; Rust side + JSON-RPC
// contract + mock-socket tests are complete and verified).
require (
	github.com/mdp/qrterminal/v3 v3.2.0
	go.mau.fi/whatsmeow v0.0.0-20240901000000-000000000000
	google.golang.org/protobuf v1.34.2
	modernc.org/sqlite v1.30.0
)
