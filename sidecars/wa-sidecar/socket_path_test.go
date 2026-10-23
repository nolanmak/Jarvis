package main

import "testing"

func TestDefaultSocketPathIsShortAndPrivateOnMac(t *testing.T) {
	got := socketPathFor("darwin", 501, "/Users/a long Unicode home/Library/Caches", false)
	want := "/tmp/augmentagent-501/wa.sock"
	if got != want {
		t.Fatalf("macOS socket path = %q, want %q", got, want)
	}
}

func TestDefaultSocketPathKeepsLinuxRuntimeDir(t *testing.T) {
	got := socketPathFor("linux", 1000, "/run/user/1000", true)
	want := "/run/user/1000/augmentagent/wa.sock"
	if got != want {
		t.Fatalf("Linux socket path = %q, want %q", got, want)
	}
}
