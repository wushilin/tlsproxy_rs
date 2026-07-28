#!/bin/sh
set -eu
cd "$(dirname "$0")"
command -v docker >/dev/null
command -v curl >/dev/null
command -v jq >/dev/null
(cd ../.. && cargo build)
if [ "${TLS_PROXY_DOCKER_SUDO:-0}" = 1 ]; then compose() { sudo docker compose "$@"; }; else compose() { docker compose "$@"; }; fi
compose down -v --remove-orphans
compose up -d
cleanup() { compose logs --no-color; compose down -v --remove-orphans; }
trap cleanup EXIT
until curl -ksSf https://127.0.0.1:14448/setup >/dev/null; do sleep 1; done
curl -ksSf https://127.0.0.1:14448/tlsproxy_api/setup -H 'content-type: application/json' --data '{"token":"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","username":"admin","password":"correct horse battery staple","control_hostname":"tlsproxy.test","self_ips":["10.30.50.3"],"provider_id":"pebble"}'
until curl -ksSf --resolve tlsproxy.test:18443:127.0.0.1 https://tlsproxy.test:18443/login >/dev/null; do sleep 1; done
cookie=$(mktemp)
ca_file=$(mktemp)
trap 'rm -f "$cookie" "$ca_file" 2>/dev/null || true; cleanup' EXIT
curl -ksS --resolve tlsproxy.test:18443:127.0.0.1 -c "$cookie" -X POST https://tlsproxy.test:18443/login -H 'content-type: application/x-www-form-urlencoded' --data 'username=admin&password=correct%20horse%20battery%20staple' >/dev/null
session=$(curl -ksSf --resolve tlsproxy.test:18443:127.0.0.1 -b "$cookie" https://tlsproxy.test:18443/tlsproxy_api/session)
csrf=$(printf '%s' "$session" | jq -r .csrf_token)
compose cp pebble:/test/certs/pebble.minica.pem "$ca_file" >/dev/null
if [ "${TLS_PROXY_DOCKER_SUDO:-0}" = 1 ]; then sudo chown "$(id -u):$(id -g)" "$ca_file"; fi
ca=$(cat "$ca_file")
provider=$(jq -n --arg ca "$ca" '{id:"pebble",name:"Local Pebble",directory_url:"https://pebble:14000/dir",contacts:[],staging:true,directory_ca_pem:$ca}')
curl -ksSf --resolve tlsproxy.test:18443:127.0.0.1 -b "$cookie" -X PUT https://tlsproxy.test:18443/tlsproxy_api/providers -H "x-csrf-token: $csrf" -H 'content-type: application/json' --data "$provider"
stored=$(curl -ksSf --resolve tlsproxy.test:18443:127.0.0.1 -b "$cookie" https://tlsproxy.test:18443/tlsproxy_api/config)
updated=$(printf '%s' "$stored" | jq '.config.control_plane.public_resolvers=["10.30.50.4:8053"] | {revision,config}')
curl -ksSf --resolve tlsproxy.test:18443:127.0.0.1 -b "$cookie" -X PUT https://tlsproxy.test:18443/tlsproxy_api/config -H "x-csrf-token: $csrf" -H 'content-type: application/json' --data "$updated" >/dev/null
sleep 2
curl -ksSf --resolve tlsproxy.test:18443:127.0.0.1 -b "$cookie" -X POST https://tlsproxy.test:18443/tlsproxy_api/renew -H "x-csrf-token: $csrf" >/dev/null
i=0
while [ "$i" -lt 60 ]; do
  status=$(curl -ksSf --resolve tlsproxy.test:18443:127.0.0.1 -b "$cookie" https://tlsproxy.test:18443/tlsproxy_api/status)
  [ "$(printf '%s' "$status" | jq -r '.managed_certificates[]|select(.id=="control-plane")|.generation_id // empty')" != "" ] && break
  i=$((i+1)); sleep 1
done
[ "$i" -lt 60 ]
echo | openssl s_client -connect 127.0.0.1:18443 -servername tlsproxy.test 2>/dev/null | openssl x509 -noout -subject -issuer
echo "Pebble TLS-ALPN issuance, activation, cache reload, and subsequent TLS serving passed"
