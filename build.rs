use std::path::Path;

fn main() {
    // Re-run this script if the external SDK directory changes.
    println!("cargo:rerun-if-changed=external/EDSDK");

    if std::env::var_os("CARGO_FEATURE_BACKEND_CANON").is_some() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        link_canon_sdk(&manifest_dir);
        copy_canon_dlls(&manifest_dir);
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
        // Embed the framework search path as an rpath so dyld finds it at runtime.
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
        // OUT_DIR is target/<profile>/build/<crate>-<hash>/out — go up 3 levels to reach target/<profile>/
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
