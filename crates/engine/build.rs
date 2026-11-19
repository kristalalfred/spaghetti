fn main() {
    println!("cargo:rustc-link-lib=framework=AudioToolbox");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        cc::Build::new()
            .file("bridge/tap_bridge.m")
            .file("bridge/watch.m")
            .file("bridge/microphone.m")
            .flag("-fobjc-arc")
            .flag("-fmodules")
            .compile("tap_bridge");
        println!("cargo:rustc-link-lib=framework=CoreAudio");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=AVFoundation");
    }

    println!("cargo:rerun-if-changed=bridge/tap_bridge.m");
    println!("cargo:rerun-if-changed=bridge/tap_bridge.h");
    println!("cargo:rerun-if-changed=bridge/watch.m");
    println!("cargo:rerun-if-changed=bridge/microphone.m");
}
