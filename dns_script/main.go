// dnsauth: decides whether a Let's Encrypt cert request for a given FQDN
// should be allowed.
//
// Usage: dnsauth <fqdn>
//
// Exit code 0 = allowed, 1 = not allowed (fail closed on any error).
//
// Checks, in order:
//  1. allowed_exact_hosts    — exact match
//  2. allowed_wildcard_hosts — patterns like "*.*.wushilin.net" where each *
//     matches exactly one valid DNS label (letters/digits/hyphens, must not
//     start or end with a hyphen)
//  3. allowed_s3_buckets     — patterns like "*.rusts3api.wushilin.net"; the *
//     is the bucket name, which is probed via a path-style HEAD request
//     against s3_endpoint. Allowed only if the bucket exists.
//
// Reads config.toml from the current working directory.
package main

import (
	"crypto/hmac"
	"crypto/tls"
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"net/http"
	"net/url"
	"os"
	"regexp"
	"strings"
	"time"

	"github.com/BurntSushi/toml"
)

type Config struct {
	AllowedExactHosts    []string `toml:"allowed_exact_hosts"`
	AllowedWildcardHosts []string `toml:"allowed_wildcard_hosts"`
	AllowedS3Buckets     []string `toml:"allowed_s3_buckets"`
	S3Endpoint           string   `toml:"s3_endpoint"`
	S3AccessKey          string   `toml:"s3_access_key"`
	S3SecretKey          string   `toml:"s3_secret_key"`
	S3Region             string   `toml:"s3_region"`
	S3TimeoutSeconds     int      `toml:"s3_timeout_seconds"`
	S3InsecureSkipVerify bool     `toml:"s3_insecure_skip_verify"`
	// Pointer so an absent key defaults to true (path style).
	S3PathStyle *bool `toml:"s3_path_style"`
}

// A single DNS label: 1-63 chars, alphanumeric plus inner hyphens.
// Rejects things like "-223" (leading hyphen) or "abc-" (trailing hyphen).
var dnsLabelRe = regexp.MustCompile(`^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$`)

// S3 bucket name: 3-63 chars, lowercase alphanumeric and hyphens,
// starts and ends with a letter or digit.
var bucketNameRe = regexp.MustCompile(`^[a-z0-9][a-z0-9-]{1,61}[a-z0-9]$`)

func main() {
	if len(os.Args) != 2 {
		fmt.Fprintf(os.Stderr, "usage: %s <fqdn>\n", os.Args[0])
		os.Exit(1)
	}
	fqdn := normalizeFQDN(os.Args[1])
	if fqdn == "" {
		deny("empty or invalid fqdn")
	}

	var cfg Config
	if _, err := toml.DecodeFile("config.toml", &cfg); err != nil {
		deny("cannot read config.toml: %v", err)
	}

	// 1. Exact hosts
	for _, h := range cfg.AllowedExactHosts {
		if fqdn == normalizeFQDN(h) {
			allow("exact match: %s", h)
		}
	}

	// 2. Wildcard hosts
	for _, pattern := range cfg.AllowedWildcardHosts {
		if matchWildcard(fqdn, normalizeFQDN(pattern)) {
			allow("wildcard match: %s", pattern)
		}
	}

	// 3. S3 bucket patterns
	for _, pattern := range cfg.AllowedS3Buckets {
		bucket, ok := extractBucket(fqdn, normalizeFQDN(pattern))
		if !ok {
			continue
		}
		if !bucketNameRe.MatchString(bucket) {
			deny("%q matches s3 pattern %q but %q is not a valid bucket name", fqdn, pattern, bucket)
		}
		exists, err := bucketExists(&cfg, bucket)
		if err != nil {
			deny("s3 probe for bucket %q failed: %v", bucket, err)
		}
		if exists {
			allow("s3 bucket %q exists (pattern %s)", bucket, pattern)
		}
		deny("s3 bucket %q does not exist", bucket)
	}

	deny("%q matched no rule", fqdn)
}

func allow(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "ALLOW: "+format+"\n", args...)
	os.Exit(0)
}

func deny(format string, args ...any) {
	fmt.Fprintf(os.Stderr, "DENY: "+format+"\n", args...)
	os.Exit(1)
}

// normalizeFQDN lowercases and strips a trailing dot.
func normalizeFQDN(s string) string {
	return strings.TrimSuffix(strings.ToLower(strings.TrimSpace(s)), ".")
}

// matchWildcard matches fqdn against a pattern like "*.*.wushilin.net".
// Each "*" matches exactly one syntactically valid DNS label; literal
// tokens must match exactly. Label counts must be equal.
func matchWildcard(fqdn, pattern string) bool {
	fl := strings.Split(fqdn, ".")
	pl := strings.Split(pattern, ".")
	if len(fl) != len(pl) {
		return false
	}
	for i := range pl {
		if pl[i] == "*" {
			if !dnsLabelRe.MatchString(fl[i]) {
				return false
			}
		} else if pl[i] != fl[i] {
			return false
		}
	}
	return true
}

// extractBucket returns the bucket name if fqdn matches an s3 pattern of the
// form "*.suffix" (a single leading wildcard label). Returns ok=false if the
// pattern does not apply to this fqdn.
func extractBucket(fqdn, pattern string) (string, bool) {
	if !strings.HasPrefix(pattern, "*.") {
		return "", false
	}
	suffix := pattern[1:] // ".rusts3api.wushilin.net"
	if !strings.HasSuffix(fqdn, suffix) {
		return "", false
	}
	bucket := strings.TrimSuffix(fqdn, suffix)
	if bucket == "" || strings.Contains(bucket, ".") {
		// must be exactly one label
		return "", false
	}
	return bucket, true
}

// bucketExists issues a SigV4-signed path-style HEAD request:
//
//	HEAD {endpoint}/{bucket}
//
// 200/403 => bucket exists, 404 => does not exist, anything else is an error
// (fail closed).
func bucketExists(cfg *Config, bucket string) (bool, error) {
	if cfg.S3Endpoint == "" || cfg.S3AccessKey == "" || cfg.S3SecretKey == "" {
		return false, fmt.Errorf("s3_endpoint / s3_access_key / s3_secret_key not configured")
	}
	region := cfg.S3Region
	if region == "" {
		region = "us-east-1"
	}
	timeout := cfg.S3TimeoutSeconds
	if timeout <= 0 {
		timeout = 5
	}

	reqURL, err := probeURL(cfg, bucket)
	if err != nil {
		return false, err
	}
	if err := checkProbeRecursion(cfg, reqURL); err != nil {
		return false, err
	}

	req, err := http.NewRequest(http.MethodHead, reqURL.String(), nil)
	if err != nil {
		return false, err
	}
	signV4(req, cfg.S3AccessKey, cfg.S3SecretKey, region, time.Now().UTC())

	client := &http.Client{Timeout: time.Duration(timeout) * time.Second}
	if cfg.S3InsecureSkipVerify {
		client.Transport = &http.Transport{
			TLSClientConfig: &tls.Config{InsecureSkipVerify: true},
		}
	}
	resp, err := client.Do(req)
	if err != nil {
		return false, err
	}
	defer resp.Body.Close()

	switch resp.StatusCode {
	case http.StatusOK:
		return true, nil
	case http.StatusForbidden:
		// With invalid credentials the server returns 403 for every
		// bucket, existing or not, so existence cannot be inferred
		// from a 403. Deny (fail closed).
		return false, fmt.Errorf("403 from HEAD %s (cannot infer bucket existence; check credentials)", reqURL.String())
	case http.StatusNotFound:
		return false, nil
	default:
		return false, fmt.Errorf("unexpected status %d from HEAD %s", resp.StatusCode, reqURL.String())
	}
}

// probeURL builds the HEAD-bucket URL in the configured style.
func probeURL(cfg *Config, bucket string) (*url.URL, error) {
	endpoint, err := url.Parse(strings.TrimSuffix(cfg.S3Endpoint, "/"))
	if err != nil {
		return nil, fmt.Errorf("bad s3_endpoint: %w", err)
	}
	reqURL := *endpoint
	if cfg.S3PathStyle == nil || *cfg.S3PathStyle {
		// Path style: HEAD https://endpoint/bucket
		reqURL.Path = endpoint.Path + "/" + bucket
	} else {
		// Virtual-hosted style: HEAD https://bucket.endpoint/
		// Careful: if the endpoint is served through this same TLS
		// proxy, this probe itself triggers a cert request for
		// bucket.endpoint and recurses back into this program.
		reqURL.Host = bucket + "." + endpoint.Host
		reqURL.Path = endpoint.Path + "/"
	}
	return &reqURL, nil
}

// checkProbeRecursion refuses a probe whose target hostname would itself
// match one of our own S3 or wildcard rules: such a probe goes through the
// TLS proxy, triggers a cert request for that hostname, and recurses back
// into this program. This only happens with virtual-hosted style; the
// path-style probe host is the endpoint itself.
func checkProbeRecursion(cfg *Config, reqURL *url.URL) error {
	host := normalizeFQDN(reqURL.Hostname())
	for _, pattern := range cfg.AllowedS3Buckets {
		if _, ok := extractBucket(host, normalizeFQDN(pattern)); ok {
			return fmt.Errorf("misconfiguration: probe host %q matches own s3 pattern %q and would recurse through the proxy; use path style or an endpoint that bypasses the proxy", host, pattern)
		}
	}
	for _, pattern := range cfg.AllowedWildcardHosts {
		if matchWildcard(host, normalizeFQDN(pattern)) {
			return fmt.Errorf("misconfiguration: probe host %q matches own wildcard pattern %q and would recurse through the proxy; use path style or an endpoint that bypasses the proxy", host, pattern)
		}
	}
	return nil
}

const emptyPayloadSHA256 ="e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"

// signV4 signs an S3 request (empty body) with AWS Signature Version 4.
func signV4(req *http.Request, accessKey, secretKey, region string, now time.Time) {
	amzDate := now.Format("20060102T150405Z")
	dateStamp := now.Format("20060102")

	req.Header.Set("Host", req.URL.Host)
	req.Header.Set("X-Amz-Date", amzDate)
	req.Header.Set("X-Amz-Content-Sha256", emptyPayloadSHA256)

	canonicalURI := req.URL.EscapedPath()
	if canonicalURI == "" {
		canonicalURI = "/"
	}
	signedHeaders := "host;x-amz-content-sha256;x-amz-date"
	canonicalHeaders := "host:" + req.URL.Host + "\n" +
		"x-amz-content-sha256:" + emptyPayloadSHA256 + "\n" +
		"x-amz-date:" + amzDate + "\n"

	canonicalRequest := strings.Join([]string{
		req.Method,
		canonicalURI,
		"", // no query string
		canonicalHeaders,
		signedHeaders,
		emptyPayloadSHA256,
	}, "\n")

	scope := dateStamp + "/" + region + "/s3/aws4_request"
	stringToSign := strings.Join([]string{
		"AWS4-HMAC-SHA256",
		amzDate,
		scope,
		hexSHA256([]byte(canonicalRequest)),
	}, "\n")

	kDate := hmacSHA256([]byte("AWS4"+secretKey), dateStamp)
	kRegion := hmacSHA256(kDate, region)
	kService := hmacSHA256(kRegion, "s3")
	kSigning := hmacSHA256(kService, "aws4_request")
	signature := hex.EncodeToString(hmacSHA256(kSigning, stringToSign))

	req.Header.Set("Authorization", fmt.Sprintf(
		"AWS4-HMAC-SHA256 Credential=%s/%s, SignedHeaders=%s, Signature=%s",
		accessKey, scope, signedHeaders, signature))
}

func hexSHA256(data []byte) string {
	h := sha256.Sum256(data)
	return hex.EncodeToString(h[:])
}

func hmacSHA256(key []byte, data string) []byte {
	mac := hmac.New(sha256.New, key)
	mac.Write([]byte(data))
	return mac.Sum(nil)
}
