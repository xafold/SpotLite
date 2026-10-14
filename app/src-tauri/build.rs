fn main() {
    // The pages are embedded at compile time; without this a change to them alone wouldn't rebuild the app.
    println!("cargo:rerun-if-changed=../ui");
    // Dev builds find gtk-layer-shell where scripts/fetch-layer-shell.sh put it; a package (scripts/package.sh) carries
    // it beside the app, and names no folder of the machine it was built on.
    println!("cargo:rerun-if-env-changed=SPOTLITE_PACKAGE");
    if std::env::var_os("SPOTLITE_PACKAGE").is_none() {
        let vendor = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Vendor");
        println!("cargo:rustc-env=SPOTLITE_DEV_VENDOR={}", vendor.canonicalize().unwrap_or(vendor).display());
    }
    tauri_build::build()
}
