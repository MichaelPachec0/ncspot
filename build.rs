use std::process::Command;

fn main() {
    protobuf_codegen::Codegen::new()
        .pure()
        .includes(["src/internal_api/proto"])
        .input("src/internal_api/proto/collection2v2.proto")
        .cargo_out_dir("collection_proto")
        .run_from_script();

    let mut version = String::from(env!("CARGO_PKG_VERSION"));
    if let Some(commit_hash) = commit_hash() {
        version = format!("{version} ({commit_hash})");
    }
    println!("cargo:rustc-env=VERSION={version}");
}

fn commit_hash() -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|hash| hash.trim().into())
}
