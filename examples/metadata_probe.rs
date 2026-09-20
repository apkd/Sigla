//! development probe only. a caught decoder panic is a failed compatibility case.
use std::{path::PathBuf, time::Instant};
use windows_metadata::{
    Type,
    reader::{File, Index, TypeDef},
};

#[derive(Default, serde::Serialize)]
struct Counts {
    types: usize,
    methods: usize,
    fields: usize,
    interfaces: usize,
    generic_parameters: usize,
    decoded_methods: usize,
    failed_methods: usize,
    failed_fields: usize,
    failures: Vec<String>,
    global_type_visible: bool,
}
fn visit(index: &Index, t: TypeDef<'_>, c: &mut Counts) {
    c.types += 1;
    c.interfaces += t.interface_impls().count();
    let generics = t
        .generic_params()
        .map(|g| {
            c.generic_parameters += 1;
            Type::Generic(g.name().into(), g.sequence())
        })
        .collect::<Vec<_>>();
    for method in t.methods() {
        c.methods += 1;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| method.signature(&generics)))
        {
            Ok(s) => {
                std::hint::black_box(s);
                c.decoded_methods += 1;
            }
            Err(_) => {
                c.failed_methods += 1;
                if c.failures.len() < 8 {
                    c.failures
                        .push(format!("{}.{}::{}", t.namespace(), t.name(), method.name()));
                }
            }
        }
    }
    for field in t.fields() {
        c.fields += 1;
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| field.ty())).is_err() {
            c.failed_fields += 1;
        }
    }
    for nested in index.nested(t) {
        visit(index, nested, c);
    }
}
fn main() -> anyhow::Result<()> {
    let paths = std::env::args_os()
        .skip(1)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    anyhow::ensure!(!paths.is_empty(), "Pass DLL paths");
    if std::env::var_os("SIGLA_PROBE_DIAGNOSTICS").is_none() {
        std::panic::set_hook(Box::new(|_| {}));
    }
    for path in paths {
        let mut timings = Vec::new();
        let mut last = Counts::default();
        let mut load_us = 0;
        let mut index_us = 0;
        for _ in 0..6 {
            let start = Instant::now();
            let file = File::read(&path)
                .ok_or_else(|| anyhow::anyhow!("Cannot read {}", path.display()))?;
            load_us = start.elapsed().as_micros();
            let start = Instant::now();
            let index = Index::new(vec![file]);
            index_us = start.elapsed().as_micros();
            let start = Instant::now();
            let mut c = Counts::default();
            for t in index.types() {
                visit(&index, t, &mut c);
            }
            c.global_type_visible = index.get("", "GlobalType").next().is_some();
            timings.push(start.elapsed().as_micros());
            last = c;
        }
        timings.remove(0);
        timings.sort();
        println!(
            "{}",
            serde_json::json!({"path":path,"file_bytes":std::fs::metadata(&path)?.len(),"warm_read_us":load_us,"warm_index_us":index_us,"warm_signature_walk_median_us":timings[timings.len()/2],"counts":last})
        );
    }
    Ok(())
}
