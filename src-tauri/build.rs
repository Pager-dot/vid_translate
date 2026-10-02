fn main() {
    #[cfg(target_os = "windows")]
    {
        // libvosk.lib sits in the crate root. Local builds link only because the MSVC
        // linker searches its working directory; say it explicitly so CI links too.
        println!(
            "cargo:rustc-link-search=native={}",
            env!("CARGO_MANIFEST_DIR")
        );
    }
    #[cfg(target_os = "macos")]
    {
        let vendor_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/macos");
        println!("cargo:rustc-link-search=native={}", vendor_dir.display());
        // Inside a bundled .app, libvosk.dylib lives in Contents/Frameworks (see
        // tauri.macos.conf.json) while the binary is in Contents/MacOS. During `cargo run` /
        // `tauri dev` there is no bundle at all, so also keep the vendor dir on the rpath.
        println!("cargo:rustc-link-arg=-Wl,-rpath,@executable_path/../Frameworks");
        println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
        println!("cargo:rustc-link-arg=-Wl,-rpath,{}", vendor_dir.display());
    }
    #[cfg(target_os = "linux")]
    {
        let vendor_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("vendor/linux-x86_64");
        println!("cargo:rustc-link-search=native={}", vendor_dir.display());
        // Look for libvosk.so next to the executable at runtime (AppImage/deb layout),
        // so no system-wide install or LD_LIBRARY_PATH is required.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
        // Tauri names the bundled lib dir after productName, so this must track it.
        // The pre-0.0.5 name stays on the rpath as a harmless fallback: a missing
        // rpath entry is skipped by the loader, and it keeps older debs working.
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/VidTranslate");
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/vid_translate");
    }
    tauri_build::build()
}
