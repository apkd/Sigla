use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=managed/Program.cs");
    println!("cargo:rerun-if-changed=managed/TrackedFileSystem.cs");
    println!("cargo:rerun-if-changed=managed/Sigla.Discovery.csproj");
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("managed");
    let status = Command::new("dotnet")
        .args([
            "build",
            "managed/Sigla.Discovery.csproj",
            "--configuration",
            "Release",
            "--nologo",
            "--verbosity",
            "quiet",
            "--output",
        ])
        .arg(&output)
        .arg(format!(
            "-p:BaseIntermediateOutputPath={}/",
            output.join("obj").display()
        ))
        .status()
        .expect("Building Sigla's embedded MSBuild integration requires a .NET SDK");
    assert!(
        status.success(),
        "Cannot build embedded MSBuild integration"
    );
}
