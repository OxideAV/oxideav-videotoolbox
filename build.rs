// Build-time linker directives for the iOS branch of
// `oxideav-videotoolbox`.
//
// On iOS we cannot `dlopen("/System/Library/Frameworks/<Name>.framework/<Name>")`
// at runtime the way the macOS branch does — sandboxed iOS apps do not get to
// open absolute system paths. Instead we let the system dyld link-load the four
// frameworks at process start, and then resolve symbols at runtime via
// `libloading::os::unix::Library::this()` (i.e. `dlsym(RTLD_DEFAULT, ...)`).
//
// On macOS we keep the existing dlopen-at-first-use code path in `sys.rs`, so
// the build script is a no-op there: macOS binaries have no compile-time
// VideoToolbox dependency and degrade gracefully when the framework cannot be
// loaded (older OS, sandboxed environment without VT entitlements).
//
// On Linux / Windows the entire crate compiles to an empty rlib via the
// `#![cfg(any(target_os = "macos", target_os = "ios"))]` gate, so this script
// is also a no-op there.

fn main() {
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os == "ios" {
        // Emitted in the order the runtime vtable resolves them.
        println!("cargo:rustc-link-lib=framework=VideoToolbox");
        println!("cargo:rustc-link-lib=framework=CoreVideo");
        println!("cargo:rustc-link-lib=framework=CoreMedia");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");
    }
}
