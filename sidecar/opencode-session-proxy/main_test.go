package main

import (
	"os"
	"path/filepath"
	"testing"
)

func writeForkMap(t *testing.T, path, body string) {
	t.Helper()
	if err := os.WriteFile(path, []byte(body), 0o600); err != nil {
		t.Fatalf("write fork map: %v", err)
	}
}

func TestForkParent(t *testing.T) {
	path := filepath.Join(t.TempDir(), "fork-map.json")
	writeForkMap(t, path, `{"version":1,"forks":{"fork-1":"parent-1"}}`)

	if got := forkParent(path, "fork-1"); got != "parent-1" {
		t.Fatalf("expected parent-1, got %q", got)
	}
	if got := forkParent(path, "fork-2"); got != "" {
		t.Fatalf("unexpected mapping: %q", got)
	}
	if got := forkParent(filepath.Join(t.TempDir(), "missing.json"), "fork-1"); got != "" {
		t.Fatalf("missing file should be ignored, got %q", got)
	}
}

func TestForkParentRejectsInvalidInput(t *testing.T) {
	dir := t.TempDir()
	cases := map[string]string{
		"broken":       `{ not json`,
		"old-version":  `{"version":0,"forks":{"fork-1":"parent-1"}}`,
		"self-mapping": `{"version":1,"forks":{"fork-1":"fork-1"}}`,
		"empty-parent": `{"version":1,"forks":{"fork-1":""}}`,
	}
	for name, body := range cases {
		t.Run(name, func(t *testing.T) {
			path := filepath.Join(dir, name+".json")
			writeForkMap(t, path, body)
			if got := forkParent(path, "fork-1"); got != "" {
				t.Fatalf("expected no mapping, got %q", got)
			}
		})
	}
}

func TestDefaultForkMapPath(t *testing.T) {
	home := t.TempDir()
	t.Setenv("KIMI_CODE_HOME", home)
	want := filepath.Join(home, "cache-keeper", "fork-map.json")
	if got := defaultForkMapPath(); got != want {
		t.Fatalf("expected %q, got %q", want, got)
	}
}
