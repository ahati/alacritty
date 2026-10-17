//! Temporary measurement harness for the graphics decode path.
//!
//! Usage: `cargo run --release -p alacritty_terminal --example decode_bench -- <dir>`

use std::time::Instant;

use alacritty_terminal::graphics::command::Format;
use alacritty_terminal::graphics::image::decode;

const MAX_DIMENSION: u32 = 10_000;

/// Dimensions of the synthetic transmissions, chosen to match a typical photo.
const BENCH_WIDTH: u32 = 1200;
const BENCH_HEIGHT: u32 = 900;

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "/tmp/pngsuite".into());

    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .expect("readable directory")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
        .collect();
    files.sort();

    let mut ok = 0usize;
    let mut failed = 0usize;
    let mut total_bytes = 0usize;
    let mut total = std::time::Duration::ZERO;
    let mut worst = std::time::Duration::ZERO;
    let mut worst_name = String::new();

    for path in &files {
        let data = std::fs::read(path).unwrap();
        let start = Instant::now();
        let result = decode(Format::Png, &data, 0, 0, MAX_DIMENSION);
        let elapsed = start.elapsed();
        match result {
            Ok(image) => {
                ok += 1;
                total_bytes += image.pixels.len();
                total += elapsed;
                if elapsed > worst {
                    worst = elapsed;
                    worst_name = path.file_name().unwrap().to_string_lossy().into_owned();
                }
            },
            Err(_) => failed += 1,
        }
    }

    println!("--- PNG corpus ({dir}) ---");
    println!("files:            {}", files.len());
    println!("decoded:          {ok}  failed: {failed}");
    println!("output pixels:    {:.1} MiB", total_bytes as f64 / (1024.0 * 1024.0));
    println!("total decode:     {total:?}");
    println!("mean per image:   {:?}", total / ok.max(1) as u32);
    println!("worst image:      {worst:?}  ({worst_name})");

    raw_case("raw RGB (uncompressed)", Format::Rgb, BENCH_WIDTH, BENCH_HEIGHT, false);
    raw_case("raw RGB (zlib)", Format::Rgb, BENCH_WIDTH, BENCH_HEIGHT, true);
    raw_case("raw RGBA (uncompressed)", Format::Rgba, BENCH_WIDTH, BENCH_HEIGHT, false);
}

/// Time a synthetic raw transmission, optionally zlib compressed.
fn raw_case(label: &str, format: Format, width: u32, height: u32, compress: bool) {
    let channels = format.bytes_per_pixel().unwrap();
    let pixels = vec![0x5Au8; width as usize * height as usize * channels];
    let data =
        if compress { miniz_oxide::deflate::compress_to_vec_zlib(&pixels, 6) } else { pixels };

    // Decompression happens before `decode` in the real path, so time both.
    let start = Instant::now();
    let payload =
        if compress { miniz_oxide::inflate::decompress_to_vec_zlib(&data).unwrap() } else { data };
    let inflated = start.elapsed();

    let start = Instant::now();
    let image = decode(format, &payload, width, height, MAX_DIMENSION).unwrap();
    let decoded = start.elapsed();

    println!("--- {label} ---");
    println!("inflate:          {inflated:?}");
    println!("decode:           {decoded:?}");
    println!("total:            {:?}", inflated + decoded);
    println!("output:           {:.1} MiB", image.pixels.len() as f64 / (1024.0 * 1024.0));
}
