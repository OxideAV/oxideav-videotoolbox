//! Runtime-loaded VideoToolbox + supporting framework handles.
//!
//! Loaded once via `OnceLock` on first use and cached for the process
//! lifetime. If any framework fails to dlopen the cache stores the
//! error so subsequent calls don't repeatedly hammer dyld.
//!
//! Frameworks needed for a usable VT bridge:
//!
//! | Framework        | Purpose                                          |
//! |------------------|--------------------------------------------------|
//! | VideoToolbox     | `VTDecompressionSession*`, `VTCompressionSession*` |
//! | CoreVideo        | `CVPixelBuffer*`, `CVImageBuffer*`               |
//! | CoreMedia        | `CMSampleBuffer*`, `CMVideoFormatDescription*`   |
//! | CoreFoundation   | `CFRetain` / `CFRelease` / `CFNumber*` / `CFDictionary*` |

use libloading::Library;
use std::sync::OnceLock;

/// Handles to the four frameworks the VT bridge needs.
///
/// All four must load successfully — VideoToolbox depends transitively
/// on the other three, but `dlopen` doesn't pull them automatically
/// for us.
pub struct Framework {
    pub video_toolbox: Library,
    pub core_video: Library,
    pub core_media: Library,
    pub core_foundation: Library,
}

/// Process-wide cache. `OnceLock` so concurrent first calls collapse
/// to a single load. Stored as `Result` so the dlopen failure surface
/// is visible to callers without a re-load attempt.
static FRAMEWORK: OnceLock<Result<Framework, String>> = OnceLock::new();

/// Get (or load) the framework handles. Returns the cached `Err` if a
/// previous load attempt failed.
pub fn framework() -> Result<&'static Framework, &'static str> {
    FRAMEWORK
        .get_or_init(load)
        .as_ref()
        .map_err(|s| s.as_str())
}

fn load() -> Result<Framework, String> {
    // Frameworks live in `/System/Library/Frameworks/X.framework/X` on
    // disk, but on macOS 11+ they're served from the dyld shared
    // cache and not present as files. `dlopen` consults the cache
    // automatically so the path string still works.
    let video_toolbox = open("/System/Library/Frameworks/VideoToolbox.framework/VideoToolbox")?;
    let core_video = open("/System/Library/Frameworks/CoreVideo.framework/CoreVideo")?;
    let core_media = open("/System/Library/Frameworks/CoreMedia.framework/CoreMedia")?;
    let core_foundation =
        open("/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation")?;

    Ok(Framework {
        video_toolbox,
        core_video,
        core_media,
        core_foundation,
    })
}

fn open(path: &str) -> Result<Library, String> {
    // SAFETY: dlopen on a fixed system framework path with no init
    // callbacks; equivalent to a normal program startup load.
    unsafe { Library::new(path) }.map_err(|e| format!("dlopen {path}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke test: every framework on this Mac loads cleanly.
    #[test]
    fn frameworks_load() {
        let fw = framework().expect("framework load");
        // Verify a stable VT entry point resolves so we know the
        // dyld cache served the right binary.
        let _: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
            fw.video_toolbox
                .get(b"VTDecompressionSessionCreate\0")
                .expect("VTDecompressionSessionCreate symbol")
        };
        let _: libloading::Symbol<unsafe extern "C" fn()> = unsafe {
            fw.core_foundation
                .get(b"CFRetain\0")
                .expect("CFRetain symbol")
        };
    }
}
