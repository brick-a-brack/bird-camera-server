use std::path::Path;

fn main() {
    println!("cargo:rerun-if-changed=external/EDSDK");
    println!("cargo:rerun-if-changed=src/backends/webcam_macos/bridge.m");
    println!("cargo:rerun-if-changed=src/backends/webcam_macos/bridge.h");
    println!("cargo:rerun-if-changed=logo/logo.ico");

    #[cfg(target_os = "windows")]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("logo/logo.ico");
        res.compile().expect("failed to compile Windows resources");
    }

    if std::env::var_os("CARGO_FEATURE_BACKEND_CANON").is_some() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        link_canon_sdk(&manifest_dir);
        copy_canon_dlls(&manifest_dir);
    }

    if cfg!(target_os = "macos")
        && std::env::var_os("CARGO_FEATURE_BACKEND_WEBCAM_MACOS").is_some()
    {
        cc::Build::new()
            .file("src/backends/webcam_macos/bridge.m")
            .include("src/backends/webcam_macos")
            .flag("-fobjc-arc")
            .flag("-fmodules")
            .compile("webcam_macos_bridge");

        println!("cargo:rustc-link-lib=framework=AVFoundation");
        println!("cargo:rustc-link-lib=framework=CoreMedia");
        println!("cargo:rustc-link-lib=framework=CoreVideo");
        println!("cargo:rustc-link-lib=framework=CoreImage");
        println!("cargo:rustc-link-lib=framework=Foundation");
        println!("cargo:rustc-link-lib=framework=IOKit");
    }
}

fn link_canon_sdk(manifest_dir: &str) {
    #[cfg(target_os = "windows")]
    {
        println!(
            "cargo:rustc-link-search=native={}/external/EDSDK/EDSDKv132010W/Windows/EDSDK_64/Library",
            manifest_dir
        );
        println!("cargo:rustc-link-lib=EDSDK");
    }

    #[cfg(target_os = "macos")]
    {
        println!(
            "cargo:rustc-link-search=framework={}/external/EDSDK/EDSDKv132010M",
            manifest_dir
        );
        println!("cargo:rustc-link-lib=framework=EDSDK");
        println!(
            "cargo:rustc-link-arg=-Wl,-rpath,{}/external/EDSDK/EDSDKv132010M",
            manifest_dir
        );
    }

    #[cfg(target_os = "linux")]
    {
        println!(
            "cargo:rustc-link-search=native={}/external/EDSDK/EDSDKv132010L",
            manifest_dir
        );
        println!("cargo:rustc-link-lib=EDSDK");
    }
}

fn copy_canon_dlls(manifest_dir: &str) {
    #[cfg(target_os = "windows")]
    {
        let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
        let profile_dir = Path::new(&out_dir)
            .ancestors()
            .nth(3)
            .expect("unexpected OUT_DIR structure")
            .to_path_buf();

        let dll_src = Path::new(manifest_dir)
            .join("external/EDSDK/EDSDKv132010W/Windows/EDSDK_64/Dll");

        for dll in &["EDSDK.dll", "EdsImage.dll"] {
            let src = dll_src.join(dll);
            let dst = profile_dir.join(dll);
            if src.exists() {
                std::fs::copy(&src, &dst)
                    .unwrap_or_else(|e| panic!("failed to copy {dll} to {dst:?}: {e}"));
                println!("cargo:warning=Copied {dll} to {}", profile_dir.display());
            } else {
                println!("cargo:warning=Canon DLL not found, skipping copy: {}", src.display());
            }
        }
    }
}
