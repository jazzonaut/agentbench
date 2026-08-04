//! Links the application icon and a version block into the Windows executables.
//!
//! Both binaries get them, and nothing else does: `winresource` emits `cargo:rustc-link-arg-bins`, so the
//! resource reaches `agentbench.exe` and `agentbench-tray.exe` without being linked into the library or
//! into every integration test.
//!
//! **A failure here is a warning, never an error.** Embedding a resource needs `rc.exe` from the Windows
//! SDK, or `llvm-rc`, and a machine can have a perfectly good Rust toolchain with neither. Artwork is not
//! worth breaking `cargo install agentbench` over: without it the executables fall back to the stock icon,
//! and so does the notification area — see the `LoadImageW` call in `src/tray/windows.rs`, which treats a
//! missing resource as an expected outcome rather than a fault.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=branding/agentbench.ico");

    // `CARGO_CFG_TARGET_OS` rather than `cfg!(windows)`. A build script is compiled for the host and its
    // own `cfg` describes the machine running it, which is the wrong question: what matters is whether the
    // executable being produced is a Windows one.
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    #[cfg(windows)]
    embed();
}

/// Attach the icon and version block, or say why it could not be attached.
///
/// Only compiled on a Windows host, because that is where the `winresource` build-dependency is resolved
/// from — a `[target.'cfg(windows)']` build-dependency is keyed on the host, not the target. Cross-building
/// a Windows binary from elsewhere therefore lands in the early return above rather than here.
#[cfg(windows)]
fn embed() {
    let mut resource = winresource::WindowsResource::new();
    // Identifier 1 is load-bearing twice over. The shell shows an executable's lowest-numbered icon
    // resource, and `src/tray/windows.rs` asks for this one by number when it loads the icon at the size
    // the notification area wants. Changing it here changes it there.
    resource.set_icon_with_id("branding/agentbench.ico", "1");

    // Read from the manifest rather than restated, so the Properties > Details tab cannot drift away from
    // what the crate says about itself. Read at run time, not with `env!`: Cargo's documented guarantee is
    // that a build script is *run* with the `CARGO_PKG_*` variables set. `winresource` fills in FileVersion
    // and ProductVersion from `CARGO_PKG_VERSION` on its own.
    let description = std::env::var("CARGO_PKG_DESCRIPTION").unwrap_or_default();
    resource.set("ProductName", "AgentBench");
    resource.set("FileDescription", &description);
    resource.set("LegalCopyright", "Licensed under the MIT licence");

    if let Err(error) = resource.compile() {
        println!(
            "cargo:warning=could not embed the Windows icon resource ({error}); \
             the executables and the tray icon will use the stock application icon"
        );
    }
}
