pub mod statistics;

pub mod resolve;
#[tokio::main]
async fn main() {
    let resolver = resolve::ResolveConfig::load_from_json_file("resolve.json").unwrap();
    println!("{:#?}", resolver.resolve("www.google1.com", 443));
}