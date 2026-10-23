// Minimal repro of the node_result write+rename pattern.
// Usage: cargo run --release --example fstest
use tokio::fs::File;
use tokio::io::AsyncWriteExt;

#[tokio::main(flavor = "multi_thread", worker_threads = 8)]
async fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "/tmp/fstest_out".into());
    std::fs::create_dir_all(&dir).unwrap();
    let mut payload = vec![0u8; 95239];
    randfill(&mut payload);
    let mut failures = 0;
    for i in 0..100 {
        let tmp = format!("{dir}/.inprogress_test_{i}");
        let finalp = format!("{dir}/final_{i}.mp4");
        let mut file = File::create(&tmp).await.unwrap();
        // write in 8KB chunks like the multipart stream
        let mut off = 0;
        while off < payload.len() {
            let end = (off + 8192).min(payload.len());
            file.write_all(&payload[off..end]).await.unwrap();
            off = end;
        }
        drop(file);
        let ondisk_before = std::fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        std::fs::rename(&tmp, &finalp).unwrap();
        let ondisk_after = std::fs::metadata(&finalp).map(|m| m.len()).unwrap_or(0);
        let readback = std::fs::read(&finalp).unwrap();
        let matchok = readback == payload;
        if ondisk_before != payload.len() as u64 || ondisk_after != payload.len() as u64 || !matchok {
            failures += 1;
            println!("ROUND {i}: before={ondisk_before} after={ondisk_after} readback={matchok}  *** FAIL ***");
        }
        let _ = std::fs::remove_file(&finalp);
    }
    println!("done: {failures}/100 failures");
}

fn randfill(b: &mut [u8]) {
    let mut s: u64 = 0x9E3779B97F4A7C15;
    for x in b.iter_mut() {
        s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *x = (s >> 33) as u8;
    }
}
