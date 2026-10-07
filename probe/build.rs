// The probe ships to every machine, so it gets the same lean link as grove.
/// Release builds are stripped by the linker rather than by `strip` in the
/// profile: rustc strips macOS binaries with LLVM's objcopy, which leaves
/// chained fixups' string pool misaligned (dyld refuses such dylibs, and
/// `dyld_info` such executables). On Linux this is the flag rustc's own
/// stripping passes.
fn link_args() {
    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    let target = |name: &str| std::env::var(format!("CARGO_CFG_TARGET_{name}")).unwrap_or_default();
    match target("OS").as_str() {
        "macos" => {
            println!("cargo:rustc-link-arg-bins=-Wl,-x,-S");
            // Nothing looks symbols up in the executable itself.
            println!("cargo:rustc-link-arg-bins=-Wl,-no_exported_symbols");
            // Chained fixups let dyld slide each pointer when its page is
            // first touched (page-in linking) instead of walking all of them
            // on every launch. Apple Silicon's dyld reads them from macOS 11,
            // the target's minimum; Intel builds keep supporting older systems.
            if target("ARCH") == "aarch64" {
                println!("cargo:rustc-link-arg-bins=-Wl,-fixup_chains");
            }
        }
        "linux" => println!("cargo:rustc-link-arg-bins=-Wl,--strip-all"),
        _ => {}
    }
}

fn main() {
    link_args();
}
