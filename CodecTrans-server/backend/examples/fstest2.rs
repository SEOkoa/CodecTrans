// Fine-grained: after each chunk write, compare path metadata vs fd metadata.
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    let dir = "/tmp/fstest2";
    std::fs::create_dir_all(dir).unwrap();
    let mut payload = vec![0u8; 95239];
    let mut s: u64 = 0x123456789ABCDEF0;
    for x in payload.iter_mut() { s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); *x = (s >> 33) as u8; }

    let path = format!("{dir}/f.mp4");
    let _ = std::fs::remove_file(&path);
    let mut file = File::create(&path).await.unwrap();
    let mut off = 0;
    let mut i = 0;
    while off < payload.len() {
        let end = (off + 8192).min(payload.len());
        file.write_all(&payload[off..end]).await.unwrap();
        i += 1;
        let psize = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let fsize = file.metadata().await.map(|m| m.len()).unwrap_or(0);
        if i <= 4 || i >= 10 || end == payload.len() {
            println!("chunk {i:2}: wrote {end:6}  path_meta={psize:6}  fd_meta={fsize:6}  lag_path={} lag_fd={}", end as i64 - psize as i64, end as i64 - fsize as i64);
        }
        off = end;
    }
    drop(file);
    let psize = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!("after drop: path_meta={psize}");
    std::thread::sleep(std::time::Duration::from_millis(100));
    let psize2 = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    println!("after 100ms: path_meta={psize2}");
}
