//! `SwiftShader`, the software Vulkan driver Chrome ships, for a machine that
//! offers no adapter: it needs nothing but glibc, so the build Chrome for
//! Testing publishes runs anywhere, with the Vulkan loader beside it.

use anyhow::Result;
use wado_host::pinned_download::{Archive, Member, Pinned};

const SWIFTSHADER: Pinned = Pinned {
    name: "the SwiftShader software GPU adapter",
    url: "https://storage.googleapis.com/chrome-for-testing-public/158.0.8127.36/linux64/chrome-headless-shell-linux64.zip",
    archive: Archive::Zip,
    members: &[
        Member {
            path: "chrome-headless-shell-linux64/libvk_swiftshader.so",
            sha256: "e269fbb6feb540a535df28678f5b2cc8bc39234ee0b11ad9602b4b96f116878d",
        },
        Member {
            path: "chrome-headless-shell-linux64/libvulkan.so.1",
            sha256: "620dc6e30d842e4e62b00a01b38a3b084d2b24a41d45b77af42a9b89db9b04cc",
        },
        // It names the driver beside it, `./libvk_swiftshader.so`.
        Member {
            path: "chrome-headless-shell-linux64/vk_swiftshader_icd.json",
            sha256: "c9eef28b6b984fec220ef0abfecc40b502d46946706e47bfe97707027cb818bd",
        },
    ],
};

/// The variables that name the Vulkan drivers the loader reads, the newer one
/// first. One the user set is their choice of driver, which this never
/// overrides.
const DRIVER_VARIABLES: [&str; 2] = ["VK_DRIVER_FILES", "VK_ICD_FILENAMES"];

/// Whether `SwiftShader` may be fetched, or only taken from the cache.
pub enum Fetch {
    Allowed,
    CachedOnly,
}

/// Hands the Vulkan loader `SwiftShader` where wgpu finds no adapter and the user
/// chose no driver or backend. It sets the loader's variables, so it runs before
/// any other thread does.
pub fn provide_when_absent(fetch: Fetch, has_adapter: impl FnOnce() -> bool) -> Result<()> {
    let chosen = DRIVER_VARIABLES
        .into_iter()
        .chain(["WGPU_BACKEND"])
        .any(|name| std::env::var_os(name).is_some());
    if chosen || has_adapter() {
        return Ok(());
    }
    let dir = match (fetch, SWIFTSHADER.cached()) {
        (_, Some(dir)) => dir,
        (Fetch::CachedOnly, None) => return Ok(()),
        (Fetch::Allowed, None) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(SWIFTSHADER.dir())?,
    };
    // wgpu opens the loader by its name, which finds one already loaded.
    // SAFETY: a loader runs no code of its own on load.
    if unsafe { libloading::Library::new("libvulkan.so.1") }.is_err() {
        let ours = dir.join("libvulkan.so.1");
        // SAFETY: as above, and it stays loaded for the rest of the process.
        std::mem::forget(unsafe { libloading::Library::new(&ours) }?);
    }
    let manifest = dir.join("vk_swiftshader_icd.json");
    for name in DRIVER_VARIABLES {
        // SAFETY: no other thread runs: the fetch's runtime has shut down, and
        // the run's has not started.
        unsafe { std::env::set_var(name, &manifest) };
    }
    Ok(())
}
