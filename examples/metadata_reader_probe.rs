//! isolate PE/table-reader cost from Sigla signature extraction.
fn main() -> anyhow::Result<()> {
    for path in std::env::args().skip(1) {
        for iteration in 0..5 {
            let start = std::time::Instant::now();
            let bytes = std::fs::read(&path)?;
            let read_us = start.elapsed().as_micros();
            let view = dotscope::CilAssemblyView::from_mem(bytes)?;
            let total_us = start.elapsed().as_micros();
            println!(
                "{}",
                serde_json::json!({"path":path,"iteration":iteration,"read_us":read_us,"reader_us":total_us-read_us})
            );
            std::hint::black_box(view);
        }
    }
    Ok(())
}
