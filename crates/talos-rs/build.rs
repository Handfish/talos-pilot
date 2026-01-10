//! Build script for talos-rs protobuf code generation

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create generated directory if it doesn't exist
    std::fs::create_dir_all("src/generated")?;

    // Configure tonic-build for Talos Machine API
    tonic_build::configure()
        .build_server(false) // We only need the client
        .build_client(true)
        .out_dir("src/generated")
        .compile_protos(
            &[
                "proto/google/rpc/status.proto",
                "proto/common/common.proto",
                "proto/machine/machine.proto",
                "proto/storage/storage.proto",
                "proto/time/time.proto",
                "proto/inspect/inspect.proto",
            ],
            &["proto"],
        )?;

    // Tell Cargo to re-run if proto files change
    println!("cargo:rerun-if-changed=proto/");

    Ok(())
}
