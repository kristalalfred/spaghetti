fn main() {
    let info_plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        info_plist.display()
    );
    println!("cargo:rerun-if-changed=Info.plist");
}
