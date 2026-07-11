fn main() {
    #[cfg(target_os = "linux")]
    {
        let vendor_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/linux-x86_64");
        println!("cargo:rustc-link-search=native={}", vendor_dir.display());
        // Look for libvosk.so next to the executable at runtime (AppImage/deb layout),
        // so no system-wide install or LD_LIBRARY_PATH is required.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/vid_translate");
    }
    tauri_build::build()
}
