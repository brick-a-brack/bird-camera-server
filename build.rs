use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=external/EDSDK");
    println!("cargo:rerun-if-changed=src/backends/webcam_macos/bridge.m");
    println!("cargo:rerun-if-changed=src/backends/webcam_macos/bridge.h");
    println!("cargo:rerun-if-changed=src/backends/camera2_android/bridge.c");
    println!("cargo:rerun-if-changed=src/backends/camera2_android/bridge.h");
    println!("cargo:rerun-if-changed=src/backends/sony/bridge.cpp");
    println!("cargo:rerun-if-changed=src/backends/sony/bridge.h");
    println!("cargo:rerun-if-changed=logo/logo.ico");

    let target = std::env::var("TARGET").unwrap_or_default();

    // Windows resources (icon) — only when targeting Windows, not when cross-compiling
    // to another target (e.g. Android) from a Windows host.
    if target.contains("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("logo/logo.ico");
        res.compile().expect("failed to compile Windows resources");
    }

    if std::env::var_os("CARGO_FEATURE_BACKEND_CANON").is_some() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        link_canon_sdk(&manifest_dir);
        copy_canon_dlls(&manifest_dir);
        copy_canon_so(&manifest_dir);
    }

    // backend-gphoto2 links `libgphoto2` via pkg-config (brew/apt). For a
    // self-contained binary we also copy libgphoto2 + its camlibs/iolibs plugins
    // and their non-system dependency closure next to the binary. On Linux they
    // get an $ORIGIN rpath and are usable straight away; on macOS the files are
    // staged here and the CI packaging step rewrites install names and lipo-merges
    // the two arches (build scripts run before the link, so they cannot rewrite
    // the binary's own Homebrew-baked absolute load commands).
    if std::env::var_os("CARGO_FEATURE_BACKEND_GPHOTO2").is_some() {
        copy_gphoto2_bundle();
    }

    if std::env::var_os("CARGO_FEATURE_BACKEND_NIKON_ZS2").is_some() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        if target.contains("apple") {
            copy_nikon_runtime(&manifest_dir);
        } else if target.contains("windows") {
            copy_nikon_runtime_windows(&manifest_dir);
        }
    }

    if std::env::var_os("CARGO_FEATURE_BACKEND_SONY").is_some()
        && (target.contains("windows") || target.contains("apple") || target.contains("linux"))
    {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set");
        build_sony(&manifest_dir, &target);
    }

    // Linux only, and only when BOTH the gphoto2 and Sony backends are bundled:
    // reconcile their two libusb copies into a single loaded object. gphoto2
    // links libusb-1.0.so.0 (loaded into the GLOBAL scope at startup), while
    // Sony's Cr_Core dlopen's "libusb-1.0.so" — a different SONAME, so the loader
    // maps a SECOND libusb. Without RTLD_DEEPBIND/DF_SYMBOLIC (neither is set),
    // Sony's dlopen'd libusb resolves its OWN internal libusb_* calls against the
    // global scope = gphoto2's libusb, so a transfer allocated by one libusb is
    // submitted through the other's libusb_submit_transfer → segfault. Making
    // gphoto2's bundled libusb answer to the SONAME Sony dlopen's collapses them
    // to one object. Runs after both bundles are staged. Must precede this line.
    if target.contains("linux")
        && std::env::var_os("CARGO_FEATURE_BACKEND_GPHOTO2").is_some()
        && std::env::var_os("CARGO_FEATURE_BACKEND_SONY").is_some()
    {
        unify_libusb_soname();
    }

    if std::env::var_os("CARGO_FEATURE_BACKEND_WEBCAM_MACOS").is_some()
        && target.contains("apple")
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

    if std::env::var_os("CARGO_FEATURE_BACKEND_CAMERA2_ANDROID").is_some()
        && target.contains("android")
    {
        let ndk_home = std::env::var("ANDROID_NDK_HOME")
            .or_else(|_| std::env::var("NDK_HOME"))
            .expect("ANDROID_NDK_HOME (or NDK_HOME) must be set to build the Android Camera2 backend");

        // Determine the prebuilt host-toolchain directory based on the build host.
        let host_tag = if cfg!(target_os = "windows") {
            "windows-x86_64"
        } else if cfg!(target_os = "macos") {
            "darwin-x86_64"
        } else {
            "linux-x86_64"
        };

        let api_level = "24"; // Camera2 NDK requires API 24+
        let abi_triple = if target.starts_with("aarch64") {
            "aarch64-linux-android"
        } else if target.starts_with("armv7") || target.starts_with("arm-") {
            "armv7a-linux-androideabi"
        } else if target.starts_with("x86_64") {
            "x86_64-linux-android"
        } else {
            "i686-linux-android"
        };

        let sysroot = format!(
            "{ndk_home}/toolchains/llvm/prebuilt/{host_tag}/sysroot"
        );
        // On Windows the NDK ships .cmd wrappers, not ELF executables.
        let clang_ext = if cfg!(target_os = "windows") { ".cmd" } else { "" };
        let clang = format!(
            "{ndk_home}/toolchains/llvm/prebuilt/{host_tag}/bin/{abi_triple}{api_level}-clang{clang_ext}"
        );

        // cargo-ndk injects CFLAGS_<rust-target>=--target=<abi-triple>21 which sets
        // __ANDROID_API__=21 and makes Camera2 NDK symbols appear unavailable (API 24+).
        // Override using the Rust target triple as the var name (matches cargo-ndk's key)
        // but keep the ABI triple in the --target= value for clang.
        std::env::set_var(
            format!("CFLAGS_{target}"),
            format!("--target={abi_triple}{api_level}"),
        );

        cc::Build::new()
            .file("src/backends/camera2_android/bridge.c")
            .include("src/backends/camera2_android")
            .compiler(&clang)
            .flag(format!("--sysroot={sysroot}"))
            .flag("-std=c11")
            .flag("-Wall")
            .compile("camera2_android_bridge");

        // Tell the Rust linker where the NDK system libraries live.
        println!(
            "cargo:rustc-link-search=native={ndk_home}/toolchains/llvm/prebuilt/{host_tag}/sysroot/usr/lib/{abi_triple}/{api_level}"
        );
        println!("cargo:rustc-link-lib=camera2ndk");
        println!("cargo:rustc-link-lib=mediandk");
        println!("cargo:rustc-link-lib=android");
        println!("cargo:rustc-link-lib=log");
    }
}

// ---------------------------------------------------------------------------
// Sony Camera Remote SDK (CrSDK)
//
// The SDK is C++ with async callbacks, wrapped by src/backends/sony/bridge.cpp
// (a flat C shim). We compile that shim as C++, link Cr_Core, and copy the SDK's
// runtime libraries + the CrAdapter/ plugin dir next to the produced binary so
// Cr_Core finds its transport adapters at runtime (like the Canon DLL copy).
//
// Vendored layout (git-ignored, see src/backends/sony/README.md):
//   external/SONY/CrSDK/include/CRSDK/*.h
//   external/SONY/CrSDK/windows/x64/{Cr_Core.{lib,dll}, monitor_protocol*.dll, CrAdapter/}
//   external/SONY/CrSDK/macos/{libCr_Core.dylib, libmonitor_protocol*.dylib, CrAdapter/}
//   external/SONY/CrSDK/linux/x64/{libCr_Core.so, libmonitor_protocol*.so, CrAdapter/}
//
// Best-effort: a missing vendored SDK only warns, so non-Sony builds keep working.
// ---------------------------------------------------------------------------

fn build_sony(manifest_dir: &str, target: &str) {
    let include = format!("{manifest_dir}/external/SONY/CrSDK/include");
    let libdir = sony_lib_dir(manifest_dir, target);

    // Compile the C++ bridge. On Windows the SDK DLL is built UNICODE, so CrChar
    // is wchar_t — define UNICODE to match the ABI of the exported functions.
    let mut build = cc::Build::new();
    build
        .cpp(true)
        .std("c++17")
        .file("src/backends/sony/bridge.cpp")
        .include("src/backends/sony");
    if target.contains("msvc") {
        build.include(&include);
        build.define("UNICODE", None);
        build.define("_UNICODE", None);
        // The bridge uses _wfopen/_wremove for Unicode paths.
        build.define("_CRT_SECURE_NO_WARNINGS", None);
    } else {
        // Pull the vendored SDK headers in as *system* headers: they are Sony's,
        // we cannot fix them, and under cc's default -Wall -Wextra they emit
        // dozens of warnings (MSVC-only pragmas, unused callback parameters,
        // member init order…) that drown out warnings from our own bridge.
        build.flag("-isystem").flag(&include);
    }
    build.compile("toucan_sony_bridge");

    if !Path::new(&libdir).exists() {
        println!(
            "cargo:warning=Sony CrSDK not found at {libdir} — see src/backends/sony/README.md (vendor the SDK)"
        );
        return;
    }

    // Link Cr_Core (Cr_Core.lib on Windows, libCr_Core.{dylib,so} elsewhere).
    println!("cargo:rustc-link-search=native={libdir}");
    println!("cargo:rustc-link-lib=Cr_Core");

    copy_sony_runtime(&libdir, target);

    // Cr_Core and its CrAdapter plugins sit next to the binary; let the loader find
    // them there at runtime.
    if target.contains("linux") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    } else if target.contains("apple") {
        println!("cargo:rustc-link-arg=-Wl,-rpath,@loader_path");
    }
}

fn sony_lib_dir(manifest_dir: &str, target: &str) -> String {
    let base = format!("{manifest_dir}/external/SONY/CrSDK");
    if target.contains("windows") {
        format!("{base}/windows/x64")
    } else if target.contains("apple") {
        format!("{base}/macos")
    } else {
        // Linux — only the x86_64 SDK is vendored by default.
        format!("{base}/linux/x64")
    }
}

/// Copies Cr_Core + the monitor_protocol libs and the CrAdapter/ plugin dir next
/// to the produced binary (skipping the Windows import lib). Best-effort.
fn copy_sony_runtime(libdir: &str, target: &str) {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let profile_dir = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR structure")
        .to_path_buf();

    let src = Path::new(libdir);
    let entries = match std::fs::read_dir(src) {
        Ok(e) => e,
        Err(e) => {
            println!("cargo:warning=Sony runtime copy: cannot read {libdir}: {e}");
            return;
        }
    };

    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            // CrAdapter/ (transport plugins) — copy the whole tree.
            copy_tree(&path, &profile_dir.join(&name));
            // macOS: Cr_Core locates its transport plugins through
            // NSBundle.mainBundle.bundlePath + "Contents/Frameworks/CrAdapter"
            // (the app-bundle layout), NOT flat next to the binary. Without a copy
            // there, EnumCameraObjects returns CrError_Adaptor_Create (0x8703) and
            // no Sony camera is ever listed. The flat copy above still lets the
            // binary resolve libCr_Core / libmonitor_protocol via @loader_path.
            if target.contains("apple") && name.to_str() == Some("CrAdapter") {
                copy_tree(&path, &profile_dir.join("Contents/Frameworks").join(&name));
            }
        } else {
            // Skip the import library; it is only needed at link time.
            if path.extension().and_then(|e| e.to_str()) == Some("lib") {
                continue;
            }
            let dst = profile_dir.join(&name);
            let _ = std::fs::remove_file(&dst);
            if let Err(e) = std::fs::copy(&path, &dst) {
                println!("cargo:warning=Sony runtime copy {name:?} failed: {e}");
                continue;
            }
            // Sony ships libmonitor_protocol.so — the live-view transport, dlopen'd
            // by Cr_Core — with no rpath at all, while it NEEDs its sibling
            // libmonitor_protocol_pf.so. The loader does not fall back to the
            // executable's rpath for a dlopen'd library's own dependencies (rustc
            // emits DT_RUNPATH, which does not propagate down the chain), so the
            // load silently fails and live view never produces a frame. Give every
            // copied lib an $ORIGIN rpath so siblings resolve.
            if target.contains("linux") {
                patch_rpath(&dst, "$ORIGIN");
            }
        }
    }
    // Plain build-script output (visible with `cargo build -vv`): a successful
    // staging is not a warning, and emitting it as one made every build noisy.
    println!("Sony CrSDK runtime staged in {}", profile_dir.display());
}

/// Collapse the gphoto2 and Sony libusb copies into a single loaded object
/// (Linux, gphoto2 + Sony builds). See the call site for the interposition
/// crash this prevents. Strategy: rename the flat bundled libusb to the SONAME
/// Sony's Cr_Core dlopen's ("libusb-1.0.so"), set its soname to match, and
/// repoint every bundled consumer's NEEDED entry from "libusb-1.0.so.0" to it.
/// At runtime the flat libusb loads once (via gphoto2's patched NEEDED, into the
/// global scope); Sony's dlopen("libusb-1.0.so") then dedups to that same object
/// instead of mapping a second libusb. Best-effort: needs patchelf.
fn unify_libusb_soname() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let profile_dir = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR structure")
        .to_path_buf();

    const OLD: &str = "libusb-1.0.so.0"; // the bundled gphoto2 libusb SONAME/file
    const NEW: &str = "libusb-1.0.so"; // the SONAME Sony's Cr_Core dlopen's

    let src = profile_dir.join(OLD);
    if !src.exists() {
        // gphoto2 didn't stage a flat libusb (e.g. its closure resolved it
        // elsewhere) — nothing to unify, and Sony keeps its own CrAdapter libusb.
        println!("cargo:warning=libusb unify: {OLD} not staged, skipping");
        return;
    }
    let dst = profile_dir.join(NEW);
    let _ = std::fs::remove_file(&dst);
    if let Err(e) = std::fs::rename(&src, &dst) {
        println!("cargo:warning=libusb unify: rename {OLD}->{NEW} failed: {e}");
        return;
    }
    // The dynamic loader resolves a NEEDED string as a *file name*, so the file
    // must be named NEW; its recorded SONAME must be NEW too so dedup keys match.
    if !patchelf(&["--set-soname", NEW], &dst) {
        println!("cargo:warning=libusb unify: could not set soname on {NEW}");
    }

    // Repoint every bundled ELF that links libusb-1.0.so.0 to the renamed file:
    // the flat libs (libgphoto2*, libEDSDK.so, libCr_Core.so, …) plus the
    // dlopen'd camlibs/iolibs plugins. Files without that NEEDED are left
    // untouched (patchelf --replace-needed is a no-op / harmless error there).
    let mut targets: Vec<PathBuf> = shared_objects_recursive(&profile_dir);
    for sub in ["camlibs", "iolibs"] {
        targets.extend(shared_objects(&profile_dir.join(sub)));
    }
    for t in &targets {
        // Skip the libusb we just renamed.
        if t == &dst {
            continue;
        }
        patchelf(&["--replace-needed", OLD, NEW], t);
    }

    // The gphoto2 bundle manifest (consumed by the CI packaging step to copy the
    // flat libs) still lists the old name; point it at the renamed file so the
    // packaged artifact ships libusb-1.0.so, not a now-missing libusb-1.0.so.0.
    let manifest = profile_dir.join("gphoto2-bundle.manifest");
    if let Ok(text) = std::fs::read_to_string(&manifest) {
        let rewritten: String = text
            .lines()
            .map(|l| if l == OLD { NEW } else { l })
            .collect::<Vec<_>>()
            .join("\n");
        let _ = std::fs::write(&manifest, format!("{rewritten}\n"));
    }

    println!("cargo:warning=libusb unify: staged single {NEW} and repointed {} bundled consumer(s)", targets.len());
}

/// All `.so*` files directly in a directory (flat libs carry versioned suffixes
/// like `libgphoto2.so.6`, which `shared_objects` — matching only `.so` — misses).
fn shared_objects_recursive(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.contains(".so") {
            out.push(p);
        }
    }
    out
}

/// Run patchelf with the given args on a file; returns whether it succeeded.
fn patchelf(args: &[&str], file: &Path) -> bool {
    Command::new("patchelf")
        .args(args)
        .arg(file)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn link_canon_sdk(manifest_dir: &str) {
    let target = std::env::var("TARGET").unwrap_or_default();

    if target.contains("windows") {
        println!(
            "cargo:rustc-link-search=native={}/external/EDSDK/EDSDKv132010W/Windows/EDSDK_64/Library",
            manifest_dir
        );
        println!("cargo:rustc-link-lib=EDSDK");
    } else if target.contains("apple") {
        println!(
            "cargo:rustc-link-search=framework={}/external/EDSDK/EDSDKv132010M",
            manifest_dir
        );
        println!("cargo:rustc-link-lib=framework=EDSDK");
        println!(
            "cargo:rustc-link-arg=-Wl,-rpath,{}/external/EDSDK/EDSDKv132010M",
            manifest_dir
        );
    } else if target.contains("linux") {
        let arch_dir = canon_linux_arch_dir(&target);
        println!(
            "cargo:rustc-link-search=native={}/external/EDSDK/EDSDKv132010L/Linux/EDSDK/Library/{}",
            manifest_dir, arch_dir
        );
        println!("cargo:rustc-link-lib=EDSDK");
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
    }
}

fn canon_linux_arch_dir(target: &str) -> &'static str {
    if target.starts_with("x86_64-") {
        "x86_64"
    } else if target.starts_with("aarch64-") {
        "ARM64"
    } else if target.starts_with("arm") {
        "ARM32"
    } else {
        panic!(
            "unsupported Linux target for Canon EDSDK: {target} \
             (supported: x86_64, aarch64, arm)"
        );
    }
}

fn copy_canon_dlls(manifest_dir: &str) {
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }

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

fn copy_canon_so(manifest_dir: &str) {
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("linux") {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let profile_dir = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR structure")
        .to_path_buf();

    let arch_dir = canon_linux_arch_dir(&target);
    let src = Path::new(manifest_dir)
        .join("external/EDSDK/EDSDKv132010L/Linux/EDSDK/Library")
        .join(arch_dir)
        .join("libEDSDK.so");
    let dst = profile_dir.join("libEDSDK.so");

    if src.exists() {
        std::fs::copy(&src, &dst)
            .unwrap_or_else(|e| panic!("failed to copy libEDSDK.so to {dst:?}: {e}"));
        println!("cargo:warning=Copied libEDSDK.so to {}", profile_dir.display());
    } else {
        println!("cargo:warning=libEDSDK.so not found, skipping copy: {}", src.display());
    }
}

// ---------------------------------------------------------------------------
// Nikon runtime
//
// The Nikon Remote SDK (MAID3 CS Layer) is dlopen'd at runtime, not linked. We
// copy the unzipped runtime artifacts next to the produced binary so the server
// finds them via `current_exe()`:
//   - `TypeCommon Module.bundle`           → loaded via dlopen
//   - `Frameworks/libNkPTPDriver2.dylib`   → referenced as @executable_path/../Frameworks
//   - `Frameworks/Royalmile.framework`     → referenced via @rpath
//   - the 3 `.config` files                → deployed to ~/Library/Preferences/Nikon/NXTether at startup
//
// Source layout expected (unzip TestApp.zip into this, see src/backends/nikon_zs2/README.md):
//   external/NIKON/runtime/{TypeCommon Module.bundle, Frameworks/, config/*.config}
//
// Best-effort: a missing source only logs a warning so non-Nikon builds and dev
// machines without the SDK keep working.
// ---------------------------------------------------------------------------

fn copy_nikon_runtime(manifest_dir: &str) {
    println!("cargo:rerun-if-changed=external/NIKON/runtime");

    let src_root = Path::new(manifest_dir).join("external/NIKON/runtime");
    if !src_root.exists() {
        println!(
            "cargo:warning=Nikon runtime not found at {} — see src/backends/nikon_zs2/README.md (unzip TestApp.zip)",
            src_root.display()
        );
        return;
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let profile_dir = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR structure")
        .to_path_buf();

    // The CFBundle (copied recursively, preserving its internal structure).
    copy_tree(
        &src_root.join("TypeCommon Module.bundle"),
        &profile_dir.join("TypeCommon Module.bundle"),
    );
    // Sibling Frameworks/ (the bundle hardcodes @executable_path/../Frameworks).
    copy_tree(
        &src_root.join("Frameworks"),
        &profile_dir.join("Frameworks"),
    );
    // Config files sit flat next to the binary; the backend deploys them to
    // ~/Library/Preferences/Nikon/NXTether at startup.
    let cfg_dir = src_root.join("config");
    for name in ["DC_PTP_Config.config", "MaidLayer.config", "RangeValue.config"] {
        let src = cfg_dir.join(name);
        if src.exists() {
            let _ = std::fs::copy(&src, profile_dir.join(name));
        } else {
            println!("cargo:warning=Nikon config missing: {}", src.display());
        }
    }
    fixup_nikon_runtime(&profile_dir);

    println!(
        "cargo:warning=Nikon runtime staged in {}",
        profile_dir.display()
    );
}

// ---------------------------------------------------------------------------
// Nikon runtime (Windows)
//
// Unlike macOS, the Windows SDK ships loose, ready-to-use artifacts (no zip, no
// `.app` layout, no codesigning, and no Objective-C class clash with the Canon
// EDSDK — the PTP driver is a plain DLL). We copy the CS-Layer DLL, its
// dependent DLLs and the 3 `.config` files next to the produced binary so:
//   - the backend `LoadLibrary`s `ControlServiceLayer.dll` via `current_exe()`;
//   - Windows resolves the dependent DLLs (`NkdPTP.dll`, `NkRoyalmile.dll`,
//     `dnssd.dll`) from the binary's directory (the default search path);
//   - the backend deploys the `.config` files to %APPDATA%\Nikon\NXTether at
//     startup from the copies staged here.
//
// Source layout (as unzipped from the SDK package, no rearranging needed):
//   external/NIKON/Module/Win/BinaryFile/{*.dll, *.config}
//
// Best-effort: a missing source only logs a warning so dev machines without the
// SDK keep building.
// ---------------------------------------------------------------------------

fn copy_nikon_runtime_windows(manifest_dir: &str) {
    println!("cargo:rerun-if-changed=external/NIKON/Module/Win/BinaryFile");

    let src = Path::new(manifest_dir).join("external/NIKON/Module/Win/BinaryFile");
    if !src.exists() {
        println!(
            "cargo:warning=Nikon Windows runtime not found at {} — see src/backends/nikon_zs2/README.md",
            src.display()
        );
        return;
    }

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let profile_dir = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR structure")
        .to_path_buf();

    for name in [
        "ControlServiceLayer.dll",
        "NkdPTP.dll",
        "NkRoyalmile.dll",
        "dnssd.dll",
        "DC_PTP_Config.config",
        "MaidLayer.config",
        "RangeValue.config",
    ] {
        let s = src.join(name);
        if s.exists() {
            let dst = profile_dir.join(name);
            // Drop a possibly read-only stale copy before overwriting.
            let _ = std::fs::remove_file(&dst);
            if let Err(e) = std::fs::copy(&s, &dst) {
                println!("cargo:warning=Nikon Windows copy {name} failed: {e}");
            }
        } else {
            println!("cargo:warning=Nikon Windows runtime missing: {}", s.display());
        }
    }

    println!(
        "cargo:warning=Nikon Windows runtime staged in {}",
        profile_dir.display()
    );
}

// ---------------------------------------------------------------------------
// Nikon runtime fixup
//
// The shipped bundle/dylib are wired for a `.app` layout and carry a download
// quarantine, so a plain copy fails to dlopen:
//   - the bundle binary depends on `@executable_path/../Frameworks/libNkPTPDriver2.dylib`
//     (a hardcoded path that resolves above the binary's dir, not next to it);
//   - `libNkPTPDriver2.dylib` references `@rpath/Royalmile.framework` but ships
//     with NO rpath, so `@rpath` never resolves;
//   - `com.apple.quarantine` blocks loading.
//
// We rewrite both to `@loader_path`-relative paths that hold wherever the binary
// lives, add the missing rpath, strip quarantine, and ad-hoc re-sign the Mach-Os
// we touched (modifying a signed Mach-O invalidates its signature; arm64 refuses
// to load it otherwise). Best-effort: failures only warn.
// ---------------------------------------------------------------------------

fn fixup_nikon_runtime(profile_dir: &Path) {
    if !std::env::var("TARGET").unwrap_or_default().contains("apple") {
        return;
    }

    let bundle_bin = profile_dir
        .join("TypeCommon Module.bundle/Contents/MacOS/TypeCommon Module");
    let ptp_dylib = profile_dir.join("Frameworks/libNkPTPDriver2.dylib");

    // Strip the download quarantine from everything we staged.
    for target in [
        profile_dir.join("TypeCommon Module.bundle"),
        profile_dir.join("Frameworks"),
    ] {
        run_quiet("xattr", &["-cr".as_ref(), target.as_os_str()]);
    }

    // Bundle binary: hardcoded `@executable_path/../Frameworks/...` → a path
    // relative to the binary itself. From `<bundle>/Contents/MacOS`, three `..`
    // reach the dir that holds the bundle and our binary, then `Frameworks/`.
    if bundle_bin.exists() {
        run_quiet(
            "install_name_tool",
            &[
                "-change".as_ref(),
                "@executable_path/../Frameworks/libNkPTPDriver2.dylib".as_ref(),
                "@loader_path/../../../Frameworks/libNkPTPDriver2.dylib".as_ref(),
                bundle_bin.as_os_str(),
            ],
        );
        run_quiet("codesign", &["--force".as_ref(), "--sign".as_ref(), "-".as_ref(), bundle_bin.as_os_str()]);
    }

    // PTP driver: give it an rpath so its `@rpath/Royalmile.framework` resolves
    // to the sibling framework in the same Frameworks dir (`@loader_path`), and
    // rename its clashing ObjC PTP classes so it can coexist with the Canon EDSDK.
    if ptp_dylib.exists() {
        patch_nikon_objc_classes(&ptp_dylib);
        run_quiet("install_name_tool", &["-add_rpath".as_ref(), "@loader_path".as_ref(), ptp_dylib.as_os_str()]);
        run_quiet("codesign", &["--force".as_ref(), "--sign".as_ref(), "-".as_ref(), ptp_dylib.as_os_str()]);
    }
}

// ---------------------------------------------------------------------------
// Nikon / Canon ObjC class clash
//
// libNkPTPDriver2.dylib (Nikon) and EDSDK (Canon) both define Objective-C classes
// with identical names (PTPOperationRequest, PTPEvent, …). The ObjC runtime keeps
// only one class per name process-wide, so loading both SDKs in one process
// corrupts one of the drivers. macOS has no per-dylib ObjC namespace (no
// dlmopen), so to keep a SINGLE process we rename those classes IN the Nikon
// dylib to unique names. The rename is byte-for-byte same length (`PTP` → `NkP`),
// matched as full NUL-terminated strings, so file offsets are untouched. Only the
// runtime registration NAME changes; the dylib's exported `_OBJC_CLASS_$_*`
// symbols and its internal class-ref pointers are unchanged (macOS two-level
// namespace binds them per-dylib), so Nikon keeps using its own classes — they
// just no longer collide with Canon's. The dylib is ad-hoc re-signed afterwards.
// ---------------------------------------------------------------------------

fn patch_nikon_objc_classes(dylib: &Path) {
    // PTP* → NkP* (same length). Matched WITH the trailing NUL so a prefix
    // (e.g. "PTPEvent") doesn't hit a longer name ("PTPEventPrivateData").
    const RENAMES: &[(&[u8], &[u8])] = &[
        (b"PTPOperationRequestPrivateData\0", b"NkPOperationRequestPrivateData\0"),
        (b"PTPOperationRequest\0", b"NkPOperationRequest\0"),
        (b"PTPOperationResponsePrivateData\0", b"NkPOperationResponsePrivateData\0"),
        (b"PTPOperationResponse\0", b"NkPOperationResponse\0"),
        (b"PTPEventPrivateData\0", b"NkPEventPrivateData\0"),
        (b"PTPEvent\0", b"NkPEvent\0"),
    ];

    let mut bytes = match std::fs::read(dylib) {
        Ok(b) => b,
        Err(e) => {
            println!("cargo:warning=Nikon ObjC rename: cannot read {}: {e}", dylib.display());
            return;
        }
    };

    let mut total = 0usize;
    for (from, to) in RENAMES {
        assert_eq!(from.len(), to.len(), "ObjC rename must be same length");
        let mut start = 0;
        while let Some(rel) = bytes[start..].windows(from.len()).position(|w| w == *from) {
            let at = start + rel;
            bytes[at..at + to.len()].copy_from_slice(to);
            start = at + to.len();
            total += 1;
        }
    }

    if total == 0 {
        println!("cargo:warning=Nikon ObjC rename: no clashing class names found (already patched?)");
        return;
    }
    match std::fs::write(dylib, &bytes) {
        Ok(()) => println!("cargo:warning=Nikon: renamed {total} clashing ObjC class name(s) in libNkPTPDriver2"),
        Err(e) => println!("cargo:warning=Nikon ObjC rename: write failed: {e}"),
    }
}

/// Runs a tool, logging non-zero exits as a cargo warning (best-effort fixup).
fn run_quiet(program: &str, args: &[&std::ffi::OsStr]) {
    match Command::new(program).args(args).output() {
        Ok(out) if out.status.success() => {}
        Ok(out) => println!(
            "cargo:warning=Nikon fixup: {program} exited {} — {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
        Err(e) => println!("cargo:warning=Nikon fixup: failed to run {program}: {e}"),
    }
}

/// Recursively copies a directory tree (used for the CFBundle / Frameworks).
fn copy_tree(src: &Path, dst: &Path) {
    if !src.exists() {
        println!("cargo:warning=Nikon runtime source missing: {}", src.display());
        return;
    }
    if src.is_dir() {
        let _ = std::fs::create_dir_all(dst);
        if let Ok(entries) = std::fs::read_dir(src) {
            for entry in entries.flatten() {
                copy_tree(&entry.path(), &dst.join(entry.file_name()));
            }
        }
    } else {
        if let Some(parent) = dst.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::copy(src, dst) {
            println!("cargo:warning=Nikon copy {} failed: {e}", src.display());
        }
    }
}

// ---------------------------------------------------------------------------
// gphoto2 runtime bundle
//
// Copies libgphoto2, libgphoto2_port, their camlibs/iolibs plugins and the
// non-system dependency closure next to the produced binary so the server can
// run without a system libgphoto2. The runtime points CAMLIBS/IOLIBS at the
// copied plugin dirs (see GPhoto2Backend::new). On Linux the copies get an
// $ORIGIN rpath and work straight away; on macOS the files are staged and the CI
// packaging step rewrites install names and lipo-merges the two arches — build
// scripts run before the link, so they cannot rewrite the binary's own
// Homebrew-baked absolute load commands.
// ---------------------------------------------------------------------------

fn copy_gphoto2_bundle() {
    let target = std::env::var("TARGET").unwrap_or_default();
    let is_mac = target.contains("apple");
    let is_linux = target.contains("linux");
    if !is_mac && !is_linux {
        return;
    }

    let libdir = match pkg_config_var("libgphoto2", "libdir") {
        Some(d) => PathBuf::from(d),
        None => {
            println!("cargo:warning=gphoto2 bundle: pkg-config libdir not found, skipping");
            return;
        }
    };

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR not set");
    let profile_dir = Path::new(&out_dir)
        .ancestors()
        .nth(3)
        .expect("unexpected OUT_DIR structure")
        .to_path_buf();

    let lib_ext = if is_mac { "dylib" } else { "so" };
    let camlibs_dir = locate_camlibs(&libdir);
    let iolibs_dir = pkg_config_var("libgphoto2_port", "driverdir").map(PathBuf::from);

    // Core libraries seed the dependency-closure walk.
    let mut roots: Vec<PathBuf> = Vec::new();
    for name in ["libgphoto2", "libgphoto2_port"] {
        let p = libdir.join(format!("{name}.{lib_ext}"));
        if p.exists() {
            roots.push(p);
        }
    }

    // Camera drivers: skip the toy-camera camlibs that depend on libgd (which
    // drags in a tree of image/AV1 codecs). Every serious driver — ptp2
    // (Canon/Nikon/Sony/Fuji/Olympus/Panasonic over USB), the serial Canon and
    // Olympus libs, etc. — has no libgd dependency, so this only drops novelty
    // cameras that cannot be remote-controlled anyway. Unsupported cameras just
    // don't get detected; libgphoto2 degrades gracefully (no crash). I/O drivers
    // are tiny — keep them all.
    let mut camlib_plugins: Vec<PathBuf> = Vec::new();
    let mut skipped = 0usize;
    if let Some(dir) = &camlibs_dir {
        for p in shared_objects(dir) {
            if depends_on_libgd(&p, is_mac) {
                skipped += 1;
            } else {
                camlib_plugins.push(p);
            }
        }
    }
    let iolib_plugins: Vec<PathBuf> =
        iolibs_dir.as_ref().map(|d| shared_objects(d)).unwrap_or_default();

    // Plugins are dlopen'd `.so` modules; they go into their own subdirs (not
    // flat), so we collect their dependency closure but not the plugins themselves.
    let mut plugins = camlib_plugins.clone();
    plugins.extend(iolib_plugins.iter().cloned());
    let closure = collect_closure(&roots, &plugins, is_mac);
    // Bundle file name per flat lib. `canonicalize` resolved the dev symlink
    // (`libgphoto2.so`) to the fully-versioned real file (`libgphoto2.so.6.0.0`),
    // but on Linux the loader resolves NEEDED entries by SONAME
    // (`libgphoto2.so.6`). Copy under the SONAME so $ORIGIN lookups from the
    // binary and the plugins resolve. On macOS the install name already uses the
    // versioned file name, so keep the canonical file name there.
    let named: Vec<(PathBuf, String)> = closure
        .iter()
        .map(|lib| {
            let fallback =
                lib.file_name().and_then(|n| n.to_str()).unwrap_or_default().to_string();
            let name = if is_linux { soname(lib).unwrap_or_else(|| fallback.clone()) } else { fallback };
            (lib.clone(), name)
        })
        .collect();
    for (lib, name) in &named {
        copy_as(lib, &profile_dir.join(name));
    }
    copy_plugins(&camlib_plugins, &profile_dir.join("camlibs"));
    copy_plugins(&iolib_plugins, &profile_dir.join("iolibs"));
    println!(
        "cargo:warning=gphoto2 bundle: {} libs, {} camlibs (+{} libgd-only skipped), {} iolibs in {}",
        closure.len(),
        camlib_plugins.len(),
        skipped,
        iolib_plugins.len(),
        profile_dir.display()
    );

    // Record the bundled flat dylibs so the macOS CI packaging step knows exactly
    // which files to lipo-merge and relink (plugin dirs are always camlibs/ iolibs/).
    // The manifest drives the CI copy step, so it must list the names actually
    // written to the bundle (SONAMEs on Linux, canonical names on macOS).
    let manifest: Vec<&str> = named.iter().map(|(_, n)| n.as_str()).collect();
    // Trailing newline is required: the CI packaging scripts read this file with
    // `while read`, which silently drops a final line that is not newline-
    // terminated — leaving that lib unbundled while the binary still gets
    // relinked to it (dangling @executable_path/$ORIGIN reference → launch crash).
    let _ = std::fs::write(
        profile_dir.join("gphoto2-bundle.manifest"),
        format!("{}\n", manifest.join("\n")),
    );

    if is_linux {
        // Flat libs find siblings via $ORIGIN; plugins (one dir down) via $ORIGIN/..
        println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN");
        for (_, name) in &named {
            patch_rpath(&profile_dir.join(name), "$ORIGIN");
        }
        for sub in ["camlibs", "iolibs"] {
            let dir = profile_dir.join(sub);
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                patch_rpath(&entry.path(), "$ORIGIN/..");
            }
        }
    }
}

fn pkg_config_var(pkg: &str, var: &str) -> Option<String> {
    let out = Command::new("pkg-config")
        .args(["--variable", var, pkg])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!v.is_empty()).then_some(v)
}

/// camlibs live in `<libdir>/libgphoto2/<modversion>`; Homebrew exposes no usable
/// pkg-config variable for it, so derive it and fall back to the single
/// plugin-bearing subdirectory.
fn locate_camlibs(libdir: &Path) -> Option<PathBuf> {
    let base = libdir.join("libgphoto2");
    if let Ok(out) = Command::new("pkg-config")
        .args(["--modversion", "libgphoto2"])
        .output()
    {
        if out.status.success() {
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let cand = base.join(&version);
            if cand.is_dir() {
                return Some(cand);
            }
        }
    }
    std::fs::read_dir(&base).ok()?.flatten().map(|e| e.path()).find(|p| {
        p.is_dir()
            && std::fs::read_dir(p)
                .map(|d| {
                    d.flatten()
                        .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("so"))
                })
                .unwrap_or(false)
    })
}

/// `LD_LIBRARY_PATH` for the `ldd` probes, with the crate's own output dirs
/// removed. Cargo prepends `target/<profile>` and `target/<profile>/deps` when it
/// runs a build script, so on a rebuild `ldd` resolves libgphoto2 & friends to the
/// copies a *previous* run staged in the bundle. The closure would then point at
/// the bundle itself (src == dest), and `copy_as` deletes the destination before
/// copying — wiping the very libs it is meant to stage. Always resolve against the
/// real prefix / system dirs instead. `None` = leave the variable unset.
fn ldd_library_path() -> Option<String> {
    let current = std::env::var("LD_LIBRARY_PATH").ok()?;
    let profile_dir = std::env::var("OUT_DIR")
        .ok()
        .and_then(|d| Path::new(&d).ancestors().nth(3).map(Path::to_path_buf))?;
    let deps_dir = profile_dir.join("deps");
    let kept: Vec<&str> = current
        .split(':')
        .filter(|e| !e.is_empty())
        .filter(|e| Path::new(e) != profile_dir && Path::new(e) != deps_dir)
        .collect();
    (!kept.is_empty()).then(|| kept.join(":"))
}

/// Direct dynamic dependencies of a library, as absolute paths.
fn list_deps(lib: &Path, is_mac: bool) -> Vec<PathBuf> {
    let output = if is_mac {
        Command::new("otool").arg("-L").arg(lib).output()
    } else {
        let mut cmd = Command::new("ldd");
        cmd.arg(lib);
        match ldd_library_path() {
            Some(path) => cmd.env("LD_LIBRARY_PATH", path),
            None => cmd.env_remove("LD_LIBRARY_PATH"),
        };
        cmd.output()
    };
    let out = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    if is_mac {
        // otool -L: first line is the file itself, then "<path> (compat …)".
        text.lines()
            .skip(1)
            .filter_map(|l| l.split_whitespace().next())
            .map(PathBuf::from)
            .collect()
    } else {
        // ldd: "libfoo.so.1 => /abs/path/libfoo.so.1 (0x…)".
        text.lines()
            .filter_map(|l| l.split("=>").nth(1))
            .filter_map(|rhs| rhs.split_whitespace().next())
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .collect()
    }
}

/// Whether a dependency should be copied into the bundle (skip the OS base libs).
fn is_bundleworthy(dep: &Path, is_mac: bool) -> bool {
    if is_mac {
        let s = dep.to_string_lossy();
        s.starts_with("/opt/homebrew/") || s.starts_with("/usr/local/")
    } else {
        const SYSTEM: &[&str] = &[
            "libc.", "libm.", "libpthread.", "libdl.", "librt.", "ld-linux",
            "libgcc_s.", "libstdc++.", "libresolv.", "libnsl.", "linux-vdso",
            "libutil.", "libcrypt.", "libz.",
            // Host-coupled: MUST come from the target machine, never the bundle.
            // libudev talks to the host's systemd-udevd (netlink + /run/udev DB);
            // a copy frozen at build time mismatches a different host's udevd and
            // corrupts udev's device list. libusb (which we DO bundle) reaches
            // udev through it, but always via the stable udev_* API, so a bundled
            // libusb + host libudev is safe — a bundled libudev is not.
            "libudev.",
        ];
        let name = dep.file_name().and_then(|n| n.to_str()).unwrap_or("");
        !SYSTEM.iter().any(|s| name.starts_with(s))
    }
}

/// Recursively resolve the flat library closure of the core libs + plugin deps.
fn collect_closure(roots: &[PathBuf], plugins: &[PathBuf], is_mac: bool) -> Vec<PathBuf> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    // Plugins are copied into subdirs, not flat — keep them out of the closure.
    for p in plugins {
        if let Ok(c) = std::fs::canonicalize(p) {
            seen.insert(c);
        }
    }
    let mut stack: Vec<PathBuf> = roots.to_vec();
    for p in plugins {
        for d in list_deps(p, is_mac) {
            if is_bundleworthy(&d, is_mac) {
                stack.push(d);
            }
        }
    }
    let mut out = Vec::new();
    while let Some(lib) = stack.pop() {
        let real = std::fs::canonicalize(&lib).unwrap_or(lib);
        if !seen.insert(real.clone()) {
            continue;
        }
        for d in list_deps(&real, is_mac) {
            if is_bundleworthy(&d, is_mac) {
                stack.push(d);
            }
        }
        out.push(real);
    }
    out
}

fn copy_into(src: &Path, dest_dir: &Path) {
    let Some(name) = src.file_name() else { return };
    copy_as(src, &dest_dir.join(name));
}

/// Copy `src` to an explicit destination path (used to rename a lib to its
/// SONAME while copying).
fn copy_as(src: &Path, dest: &Path) {
    // Never let the source be the destination: `remove_file` below would delete the
    // file we are about to read.
    let same_file = src == dest
        || matches!(
            (std::fs::canonicalize(src), std::fs::canonicalize(dest)),
            (Ok(a), Ok(b)) if a == b
        );
    if same_file {
        return;
    }
    // brew dylibs are read-only, and std::fs::copy copies their mode — so a stale
    // read-only copy from a previous build can't be overwritten. Drop it first.
    let _ = std::fs::remove_file(dest);
    if let Err(e) = std::fs::copy(src, dest) {
        println!("cargo:warning=gphoto2 bundle: copy {} failed: {e}", src.display());
        return;
    }
    // brew/apt dylibs are read-only; make the copy writable so the Linux patchelf
    // pass and the macOS CI install_name_tool relink can modify it.
    make_writable(dest);
}

/// The SONAME an ELF advertises (Linux), e.g. `libgphoto2.so.6` — the name the
/// dynamic loader resolves NEEDED entries by. `None` if patchelf is unavailable
/// or the lib carries no SONAME (fall back to the file name).
fn soname(lib: &Path) -> Option<String> {
    let out = Command::new("patchelf").arg("--print-soname").arg(lib).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}

#[cfg(unix)]
fn make_writable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | 0o200));
    }
}

#[cfg(not(unix))]
fn make_writable(_path: &Path) {}

/// All `.so` plugin modules in a directory.
fn shared_objects(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("so") {
            out.push(p);
        }
    }
    out
}

/// Whether a plugin links libgd (directly). The toy-camera camlibs do; the real
/// drivers (ptp2, canon, …) do not. Used to drop the libgd codec tree.
fn depends_on_libgd(lib: &Path, is_mac: bool) -> bool {
    list_deps(lib, is_mac).iter().any(|d| {
        d.file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("libgd."))
            .unwrap_or(false)
    })
}

fn copy_plugins(plugins: &[PathBuf], dest: &Path) {
    if plugins.is_empty() {
        return;
    }
    if let Err(e) = std::fs::create_dir_all(dest) {
        println!("cargo:warning=gphoto2 bundle: mkdir {} failed: {e}", dest.display());
        return;
    }
    for p in plugins {
        copy_into(p, dest);
    }
}

/// Add an rpath to a copied ELF (Linux). Best-effort: needs `patchelf`.
fn patch_rpath(lib: &Path, rpath: &str) {
    let _ = Command::new("patchelf")
        .args(["--add-rpath", rpath])
        .arg(lib)
        .status();
}
