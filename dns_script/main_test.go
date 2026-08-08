package main

import (
	"fmt"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"
)

func TestProbeIsPathStyle(t *testing.T) {
	var gotPath, gotHost string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		gotPath, gotHost = r.URL.Path, r.Host
		w.WriteHeader(http.StatusOK)
	}))
	defer srv.Close()

	cfg := &Config{S3Endpoint: srv.URL, S3AccessKey: "ak", S3SecretKey: "sk"}
	exists, err := bucketExists(cfg, "somebucket")
	if err != nil || !exists {
		t.Fatalf("exists=%v err=%v", exists, err)
	}
	if gotPath != "/somebucket" {
		t.Errorf("expected path-style /somebucket, got path %q", gotPath)
	}
	if strings.HasPrefix(gotHost, "somebucket.") {
		t.Errorf("virtual-hosted style detected: host %q", gotHost)
	}
}

func TestPrintAuthHeader(t *testing.T) {
	req, err := http.NewRequest(http.MethodHead, "https://rusts3api.wushilin.net/testbucket", nil)
	if err != nil {
		t.Fatal(err)
	}
	fixed := time.Date(2026, 8, 8, 12, 0, 0, 0, time.UTC)
	signV4(req, "asdf123", "@super4321", "us-east-1", fixed)
	fmt.Println("GO_AUTH:", req.Header.Get("Authorization"))
}

func TestProbeURLStyles(t *testing.T) {
	cfg := &Config{S3Endpoint: "https://rusts3api.wushilin.net"}

	// default (unset) => path style
	u, err := probeURL(cfg, "mybucket")
	if err != nil || u.String() != "https://rusts3api.wushilin.net/mybucket" {
		t.Errorf("default should be path style, got %v err=%v", u, err)
	}

	pathStyle := true
	cfg.S3PathStyle = &pathStyle
	u, _ = probeURL(cfg, "mybucket")
	if u.String() != "https://rusts3api.wushilin.net/mybucket" {
		t.Errorf("path style: got %v", u)
	}

	pathStyle = false
	u, _ = probeURL(cfg, "mybucket")
	if u.String() != "https://mybucket.rusts3api.wushilin.net/" {
		t.Errorf("virtual-hosted style: got %v", u)
	}
}

func TestRecursionGuard(t *testing.T) {
	pathStyle := false
	cfg := &Config{
		S3Endpoint:       "https://rusts3api.wushilin.net",
		S3AccessKey:      "ak",
		S3SecretKey:      "sk",
		S3PathStyle:      &pathStyle,
		AllowedS3Buckets: []string{"*.rusts3api.wushilin.net"},
	}
	// Virtual-hosted probe of "code" targets code.rusts3api.wushilin.net,
	// which matches our own pattern => guaranteed recursion => error.
	if _, err := bucketExists(cfg, "code"); err == nil || !strings.Contains(err.Error(), "recurse") {
		t.Errorf("expected recursion error, got %v", err)
	}

	// Path style against the same endpoint is fine (host is the endpoint,
	// not bucket.endpoint) — must not trip the guard.
	u, _ := probeURL(&Config{S3Endpoint: cfg.S3Endpoint}, "code")
	if err := checkProbeRecursion(cfg, u); err != nil {
		t.Errorf("path style should not trip guard: %v", err)
	}
}

func TestMatchWildcard(t *testing.T) {
	cases := []struct {
		fqdn, pattern string
		want          bool
	}{
		{"a.b.wushilin.net", "*.*.wushilin.net", true},
		{"host-03.b.wushilin.net", "*.*.wushilin.net", true},
		{"-223.b.wushilin.net", "*.*.wushilin.net", false},
		{"a.wushilin.net", "*.*.wushilin.net", false},
		{"a.b.c.wushilin.net", "*.*.wushilin.net", false},
		{"a.b.wushilin.org", "*.*.wushilin.net", false},
		// a single * must not swallow multiple tokens / dots
		{"a.b.wushilin.net", "*.wushilin.net", false},
		{"asdf.asfaw.awegawef.wushilin.net", "*.wushilin.net", false},
		// * must be a valid DNS label
		{"abc-.b.wushilin.net", "*.*.wushilin.net", false},   // trailing hyphen
		{"my_host.b.wushilin.net", "*.*.wushilin.net", false}, // underscore
		{".b.wushilin.net", "*.*.wushilin.net", false},        // empty label
		{strings.Repeat("a", 64) + ".b.wushilin.net", "*.*.wushilin.net", false}, // >63 chars
		{strings.Repeat("a", 63) + ".b.wushilin.net", "*.*.wushilin.net", true},
	}
	for _, c := range cases {
		if got := matchWildcard(c.fqdn, c.pattern); got != c.want {
			t.Errorf("matchWildcard(%q, %q) = %v, want %v", c.fqdn, c.pattern, got, c.want)
		}
	}
}

func TestCaseInsensitivity(t *testing.T) {
	if normalizeFQDN("CODE.RustS3API.wushilin.NET.") != "code.rusts3api.wushilin.net" {
		t.Error("fqdn should be lowercased and trailing dot stripped")
	}
	b, ok := extractBucket(normalizeFQDN("MyBucket.rusts3api.wushilin.net"), "*.rusts3api.wushilin.net")
	if !ok || b != "mybucket" {
		t.Errorf("uppercase fqdn should probe lowercase bucket, got %q, %v", b, ok)
	}
	if !matchWildcard(normalizeFQDN("Host-03.B.wushilin.net"), "*.*.wushilin.net") {
		t.Error("wildcard match should be case-insensitive")
	}
}

func TestExtractBucket(t *testing.T) {
	b, ok := extractBucket("mybucket.rusts3api.wushilin.net", "*.rusts3api.wushilin.net")
	if !ok || b != "mybucket" {
		t.Errorf("got %q, %v", b, ok)
	}
	if _, ok := extractBucket("a.b.rusts3api.wushilin.net", "*.rusts3api.wushilin.net"); ok {
		t.Error("multi-label should not match")
	}
	if _, ok := extractBucket("rusts3api.wushilin.net", "*.rusts3api.wushilin.net"); ok {
		t.Error("bare suffix should not match")
	}
}
