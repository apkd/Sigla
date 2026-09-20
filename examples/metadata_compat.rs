//! isolates a single metadata operation so decoder aborts stay in a test process.
use windows_metadata::{
    Type,
    reader::{File, Index},
};
fn main() {
    let args = std::env::args().collect::<Vec<_>>();
    let file = File::read(&args[1]).expect("PE reader rejected DLL");
    eprintln!("PE read passed");
    let index = Index::new(vec![file]);
    eprintln!("Index construction passed");
    for ty in index.types() {
        if args.get(2).is_some_and(|name| name != ty.name()) {
            continue;
        }
        println!("{}.{}", ty.namespace(), ty.name());
        let generics = ty
            .generic_params()
            .map(|g| Type::Generic(g.name().into(), g.sequence()))
            .collect::<Vec<_>>();
        for method in ty.methods() {
            if args.get(3).is_none_or(|name| name != method.name()) {
                continue;
            }
            eprintln!("Decoding {}", method.name());
            println!("{:?}", method.signature(&generics));
        }
    }
}
