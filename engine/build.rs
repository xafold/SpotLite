// Builds the C shim over llama.cpp (shim/llama_shim.c) and links llama.cpp's shared libraries, which
// scripts/build-llama.sh puts in Vendor/llama, and PDFium (scripts/fetch-pdfium.sh). The libraries are found at run time
// beside the binary ($ORIGIN, where an install puts them) or in Vendor/ (dev builds).
fn main() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let llama = root.join("Vendor/llama");
    let (include, lib) = (llama.join("include"), llama.join("lib"));
    assert!(include.join("llama.h").exists(), "no llama.cpp in {}: run scripts/build-llama.sh", llama.display());
    cc::Build::new().file("shim/llama_shim.c").include(&include).opt_level(2).compile("llama_shim");
    println!("cargo:rerun-if-changed=shim/llama_shim.c");
    println!("cargo:rerun-if-changed={}", llama.join("BUILD").display());
    println!("cargo:rustc-link-search=native={}", lib.display());
    for name in ["mtmd", "llama", "ggml", "ggml-base"] {
        println!("cargo:rustc-link-lib=dylib={name}");
    }
    // PDFium (prebuilt, scripts/fetch-pdfium.sh): PDF text and page renders.
    let pdfium = root.join("Vendor/pdfium/lib");
    println!("cargo:rustc-link-search=native={}", pdfium.display());
    println!("cargo:rustc-link-lib=dylib=pdfium");
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    // Dev builds also look where the libraries were built. A package (scripts/package.sh) carries them beside the
    // binary, and names no folder of the machine it was built on.
    println!("cargo:rerun-if-env-changed=SPOTLITE_PACKAGE");
    if std::env::var_os("SPOTLITE_PACKAGE").is_none() {
        println!("cargo:rustc-env=SPOTLITE_DEV_VENDOR={}", root.join("Vendor").canonicalize().unwrap().display());
        for folder in [&lib, &pdfium] {
            println!("cargo:rustc-link-arg=-Wl,-rpath,{}", folder.canonicalize().unwrap().display());
        }
    }
    println!("cargo:rustc-env=LLAMA_BUILD_TAG={}", std::fs::read_to_string(llama.join("BUILD")).unwrap_or_default()
        .split_whitespace().next().unwrap_or("unknown"));
}
