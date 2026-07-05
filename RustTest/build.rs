use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // RustTest lives inside the box3d repository
    let box3d_root = manifest_dir.parent().expect("RustTest must live inside the box3d repo");

    let include_dir = box3d_root.join("include");
    let shared_dir = box3d_root.join("shared");
    let lib_dir = box3d_root.join("build/src");
    let shared_lib_dir = box3d_root.join("build/shared");

    for lib in [&lib_dir, &shared_lib_dir] {
        if !lib.exists() {
            panic!(
                "{} not found. Build box3d first (e.g. run ./build.sh in the repo root).",
                lib.display()
            );
        }
    }

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-search=native={}", shared_lib_dir.display());
    // libshared.a (CreateHuman ragdoll helper) depends on libbox3d.a
    println!("cargo:rustc-link-lib=static=shared");
    println!("cargo:rustc-link-lib=static=box3d");
    println!("cargo:rustc-link-lib=m");

    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed={}", include_dir.join("box3d").display());
    println!("cargo:rerun-if-changed={}", shared_dir.join("human.h").display());

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", include_dir.display()))
        .clang_arg(format!("-I{}", shared_dir.display()))
        .allowlist_function("b3.*|CreateHuman|DestroyHuman|Human_.*")
        .allowlist_type("b3.*|Human|Bone|BoneId")
        .allowlist_var("b3.*|B3_.*|bone_.*|FILTER_JOINT_COUNT")
        .derive_default(true)
        .layout_tests(false)
        .generate()
        .expect("bindgen failed to generate box3d bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs");
    bindings.write_to_file(&out_path).expect("failed to write bindings");
}
