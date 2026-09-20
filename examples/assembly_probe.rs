fn main() -> anyhow::Result<()> {
    let repeats = std::env::var("SIGLA_PROBE_REPEATS")
        .ok()
        .map(|s| s.parse::<usize>())
        .transpose()?
        .unwrap_or(1)
        .max(1);
    for path in std::env::args().skip(1) {
        for iteration in 0..repeats {
            let start = std::time::Instant::now();
            let facts = sigla::metadata::extract(std::path::Path::new(&path))?;
            println!(
                "{}",
                serde_json::json!({"path":path,"iteration":iteration,"elapsed_us":start.elapsed().as_micros(),"types":facts.members.iter().filter(|m|matches!(m.kind.as_str(),"class"|"struct"|"interface"|"enum")).count(),"methods":facts.methods,"properties":facts.properties,"events":facts.events,"generic_parameters":facts.generic_parameters,"method_implementations":facts.method_implementations,"forwarders":facts.forwarders.len()})
            );
        }
    }
    Ok(())
}
