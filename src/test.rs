pub mod statistics;

use statistics::{GlobalStats, ConnStats};
use std::sync::Arc;

#[tokio::main]
async fn main() {

    let stat = GlobalStats::new();
    let garc = Arc::new(stat);
    let garc1 = Arc::clone(&garc);
    let lstat = Arc::new(ConnStats::new(garc));
    let lstat1 = Arc::clone(&lstat);
    let lstat2 = Arc::clone(&lstat);
    let jh1 = tokio::spawn(async move {
        for _ in 0..1000 {
            lstat1.add_uploaded_bytes(10);
        }
    });
    let jh2 = tokio::spawn(async move {
        for _ in 0..1000 {
            lstat2.add_downloaded_bytes(100);
        }
    });
    jh1.await.unwrap();
    jh2.await.unwrap();
    let global_up = garc1.total_uploaded_bytes();
    let global_down = garc1.total_downloaded_bytes();
    println!("GUP {global_up} GDOWN {global_down}")
}