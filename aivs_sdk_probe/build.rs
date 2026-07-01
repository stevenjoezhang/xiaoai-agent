use std::path::PathBuf;

fn main() {
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target_os != "linux" {
        return;
    }

    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let sdk_dir = manifest_dir.parent().unwrap().join("xiaoai_asr_probe");

    println!("cargo:rustc-link-search=native={}", sdk_dir.display());
    println!("cargo:rustc-link-lib=dylib=aivs_sdk");

    // The speaker already provides libaivs_sdk.so in /usr/lib.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib");
}
