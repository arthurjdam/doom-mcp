use std::fs;

fn main() {
    let vendor = "vendor/doomgeneric";

    let mut sources: Vec<_> = fs::read_dir(vendor)
        .expect("vendor/doomgeneric is missing")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "c"))
        .collect();
    sources.sort();

    let mut build = cc::Build::new();
    build
        .files(&sources)
        .file("csrc/dmcp.c")
        .include(vendor)
        .include("csrc")
        // Render at Doom's native 320x200; scaling happens on the Rust side.
        .define("DOOMGENERIC_RESX", "320")
        .define("DOOMGENERIC_RESY", "200")
        .define("NORMALUNIX", None)
        .define("_DEFAULT_SOURCE", None)
        // Route the engine's exit() calls (I_Quit, I_Error) to the shim so a
        // fatal engine error doesn't take down the MCP server.
        .define("exit", "dmcp_exit")
        .opt_level(2)
        .warnings(false)
        // 1990s C: implicit declarations and loose pointer types are errors
        // in modern compilers by default.
        .flag_if_supported("-Wno-implicit-function-declaration")
        .flag_if_supported("-Wno-int-conversion")
        .flag_if_supported("-Wno-incompatible-pointer-types")
        .flag_if_supported("-Wno-incompatible-function-pointer-types")
        .flag_if_supported("-fno-strict-aliasing")
        .compile("doomgeneric");

    println!("cargo:rerun-if-changed={vendor}");
    println!("cargo:rerun-if-changed=csrc");
}
