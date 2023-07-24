pub mod errors;
pub mod statistics;

pub mod resolve;
use resolve::HostAndPort;
use std::io;
#[tokio::main]
async fn main() {
    let resolver = resolve::ResolveConfig::load_from_json_file("resolve.json").unwrap();
    let stdin = io::stdin();
    let mut buf: String = String::new();
    loop {
        println!("Enter host:port to resolve: ");
        if stdin.read_line(&mut buf).unwrap() == 0 {
            break;
        }
        let trimmed = buf.trim();

        let host_and_port = HostAndPort::parse_single(trimmed).unwrap();
        println!(
            "{:#?}",
            resolver.resolve(&host_and_port.host, host_and_port.port.unwrap_or(443))
        );
        buf.clear();
    }
}
