//! Scanner throughput. Run with:
//! `cargo test --release -p aiterm-intercept --test throughput -- --ignored --nocapture`

use std::time::Instant;

use aiterm_intercept::Scanner;

fn measure(name: &str, data: &[u8]) {
    let mut scanner = Scanner::new();
    let mut count = 0usize;
    let start = Instant::now();
    let rounds = 20;
    for _ in 0..rounds {
        for chunk in data.chunks(64 * 1024) {
            scanner.feed(chunk, |_| count += 1);
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let mb = (data.len() * rounds) as f64 / 1e6;
    println!("{name:>12}: {:>8.0} MB/s ({count} sequences)", mb / secs);
}

#[test]
#[ignore]
fn throughput() {
    // Typical colored program output: SGR-heavy text.
    let line = "\x1b[32m+\x1b[0m src/main.rs:42: \x1b[1mfn\x1b[0m render(&mut self) -> Result<(), Error> {\r\n";
    let text: Vec<u8> = line.repeat(200_000).into_bytes();
    // Plain ASCII without escapes (`cat`, `seq`).
    let plain: Vec<u8> = "1234567890 the quick brown fox jumps over\r\n"
        .repeat(400_000)
        .into_bytes();
    // Kitty graphics: large base64 APC payloads.
    let apc = format!("\x1b_Ga=T,f=100,m=1;{}\x1b\\", "QUJD".repeat(1024))
        .repeat(2_000)
        .into_bytes();
    // Sixel image data.
    let sixel = format!("\x1bP0;1q\"1;1;80;80#0;2;0;0;0{}\x1b\\", "#0~~~~@@@@-".repeat(400))
        .repeat(2_000)
        .into_bytes();
    measure("plain", &plain);
    measure("sgr text", &text);
    measure("kitty apc", &apc);
    measure("sixel dcs", &sixel);
}
