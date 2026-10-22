fn main() {
    cc::Build::new()
        .file("bridge/tap_bridge.m")
        .flag("-fobjc-arc")
        .flag("-fmodules")
        .compile("tap_bridge");

    println!("cargo:rustc-link-lib=framework=CoreAudio");
    println!("cargo:rustc-link-lib=framework=AudioToolbox");
    println!("cargo:rustc-link-lib=framework=Foundation");

    // Claude: spaghettid runs as a launchd agent, which Local Network privacy
    // covers. Without an embedded Info.plist, SRT to a LAN peer failed with
    // EHOSTUNREACH; Apple's TN3179 has a standalone executable that needs the
    // privilege embed NSLocalNetworkUsageDescription this way.
    let info_plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
    println!(
        "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        info_plist.display()
    );

    println!("cargo:rerun-if-changed=Info.plist");
    println!("cargo:rerun-if-changed=bridge/tap_bridge.m");
    println!("cargo:rerun-if-changed=bridge/tap_bridge.h");
}
