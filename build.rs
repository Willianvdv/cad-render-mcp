// Builds the C++ shim against a dynamically linked OCCT (LGPL-2.1 + exception).
// OCCT_DIR (default: Homebrew `opencascade`) must contain include/opencascade and lib/.
use std::path::PathBuf;

const OCCT_LIBS: &[&str] = &[
    "TKernel",
    "TKMath",
    "TKG2d",
    "TKG3d",
    "TKGeomBase",
    "TKGeomAlgo",
    "TKBRep",
    "TKTopAlgo",
    "TKMesh",
    "TKBO",
    "TKPrim",
    "TKShHealing",
    "TKXSBase",
    "TKDE",
    "TKDESTEP",
    "TKDEIGES",
    "TKCDF",
    "TKLCAF",
    "TKCAF",
    "TKXCAF",
];

fn main() {
    println!("cargo:rerun-if-changed=cpp/occt_shim.cpp");
    println!("cargo:rerun-if-changed=cpp/occt_shim.h");
    println!("cargo:rerun-if-env-changed=OCCT_DIR");
    let dir = std::env::var("OCCT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let out = std::process::Command::new("brew")
                .args(["--prefix", "opencascade"])
                .output();
            match out {
                Ok(o) if o.status.success() => {
                    PathBuf::from(String::from_utf8_lossy(&o.stdout).trim())
                }
                _ => PathBuf::from("/opt/homebrew/opt/opencascade"),
            }
        });
    let inc = dir.join("include/opencascade");
    let lib = dir.join("lib");
    assert!(
        inc.exists(),
        "OCCT headers not found at {} (set OCCT_DIR)",
        inc.display()
    );
    cc::Build::new()
        .cpp(true)
        .std("c++17")
        .opt_level(3)
        .include(&inc)
        .include("cpp")
        .flag_if_supported("-Wno-deprecated-declarations")
        .flag_if_supported("-Wno-unused-parameter")
        .file("cpp/occt_shim.cpp")
        .compile("occt_shim");
    println!("cargo:rustc-link-search=native={}", lib.display());
    for l in OCCT_LIBS {
        println!("cargo:rustc-link-lib=dylib={l}");
    }
    println!("cargo:rustc-link-arg=-Wl,-rpath,{}", lib.display());
    println!("cargo:rustc-link-lib=c++");
}
