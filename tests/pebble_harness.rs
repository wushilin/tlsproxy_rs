#[test]
fn pebble_harness_routes_tls_alpn_to_the_real_port_443() {
    let compose = include_str!("pebble/compose.yaml");
    let config = include_str!("pebble/pebble-config.json");
    let script = include_str!("pebble/run.sh");
    assert!(config.contains("\"tlsPort\": 443"));
    assert!(compose.contains("10.30.50.3"));
    assert!(script.contains("tlsproxy_api/renew"));
    assert!(script.contains("openssl s_client"));
}
