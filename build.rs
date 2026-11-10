// Build script. No dependencies and no network. It embeds app.manifest into the exe
// (see that file for why) and, for the MSVC linker, adds one hardening flag.
use std::path::PathBuf;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=app.manifest");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("app.manifest");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    match std::env::var("CARGO_CFG_TARGET_ENV").as_deref() {
        Ok("msvc") => {
            // the MSVC linker embeds a manifest file by itself
            println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
            println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());
            println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");
            // Extra, not what the DLL hardening relies on (see src/harden.rs).
            println!("cargo:rustc-link-arg-bins=/DEPENDENTLOADFLAG:0x800");
        }
        Ok("gnu") => {
            // MinGW: turn the manifest into a resource object with windres and link it.
            // Resource type 24 = RT_MANIFEST, id 1 = the manifest of an exe.
            let rc = out.join("manifest.rc");
            let obj = out.join("manifest.o");
            let path = manifest.display().to_string().replace('\\', "/");
            std::fs::write(&rc, format!("1 24 \"{path}\"\n")).expect("write manifest.rc");
            let tools = ["x86_64-w64-mingw32-windres", "windres"];
            let done = tools.iter().any(|tool| {
                Command::new(tool)
                    .args(["--input-format=rc", "--output-format=coff", "--target=pe-x86-64"])
                    .arg("-i")
                    .arg(&rc)
                    .arg("-o")
                    .arg(&obj)
                    .status()
                    .map(|s| s.success())
                    .unwrap_or(false)
            });
            // no silent fallback: an exe without the manifest would be weaker than promised
            assert!(done, "windres (part of MinGW-w64) is needed to embed app.manifest");
            println!("cargo:rustc-link-arg-bins={}", obj.display());
        }
        _ => {}
    }
}
