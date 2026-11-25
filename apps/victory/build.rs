fn main() {
    // The D3D12 Agility SDK exports (src/agility.rs): D3D12 finds them on
    // the executable. Only the MSVC linker takes /EXPORT on a binary.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKVersion,DATA");
        println!("cargo:rustc-link-arg-bins=/EXPORT:D3D12SDKPath,DATA");
        // d3d12.dll reads those two exports as it loads, and a plain import
        // would load it before `main` runs, which is before `agility::choose`
        // can decide between the app-local runtime and the system's. Delay
        // loading it moves that load to the first D3D12 call.
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:d3d12.dll");
        println!("cargo:rustc-link-arg-bins=delayimp.lib");
    }
}
