# Pebble end-to-end ACME harness

Run `./run.sh` with Docker Compose, curl, jq, and OpenSSL installed. It builds
tlsproxy, bootstraps RocksDB, installs Pebble as a private ACME provider,
points public-DNS checks at challtestsrv, and requests the control certificate.
Pebble connects to the actual mandatory port 443 with `acme-tls/1`; the script
then waits for atomic activation/cache reload and verifies the issued
certificate is served. Public CAs are never contacted. The stack and volume
are removed on exit, while logs are printed for diagnosis.
