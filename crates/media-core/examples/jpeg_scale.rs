//! Spike: full JPEG decode (image crate) vs DCT-scaled decode (jpeg-decoder) for thumbnails/analysis.
use std::time::Instant;
fn main() -> anyhow::Result<()> {
    let p = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&p)?;
    for _ in 0..2 {
        let t = Instant::now();
        let full = image::load_from_memory(&bytes)?;
        let t_full = t.elapsed();
        let t = Instant::now();
        let mut d = jpeg_decoder::Decoder::new(std::io::Cursor::new(&bytes));
        let (w, h) = d.scale(1000, 1000)?;
        let px = d.decode()?;
        let t_scaled = t.elapsed();
        println!("full {}x{} {:?} | scaled {}x{} ({} bytes) {:?}", full.width(), full.height(), t_full, w, h, px.len(), t_scaled);
    }
    Ok(())
}
