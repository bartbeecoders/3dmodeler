use std::env;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    // crates/box3d-sys -> 3dmodeler -> box3d repo root
    let modeler_root = manifest_dir.parent().unwrap().parent().unwrap();
    let box3d_root = modeler_root.parent().unwrap();

    let include_dir = box3d_root.join("include");
    let src_dir = box3d_root.join("src");
    let target = env::var("TARGET").unwrap();
    let is_wasm = target.starts_with("wasm32");

    let sysroot = modeler_root.join("tools/wasi-sysroot");
    if is_wasm && !sysroot.exists() {
        panic!(
            "wasi sysroot not found at {}. Run tools/get-wasi-sysroot.sh first.",
            sysroot.display()
        );
    }

    // --- bindings -------------------------------------------------------
    let mut builder = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", include_dir.display()))
        .allowlist_function("b3.*")
        .allowlist_type("b3.*")
        .allowlist_var("b3.*|B3_.*")
        .derive_default(true)
        .layout_tests(false);

    if is_wasm {
        // Parse with the wasm ABI so struct layouts/pointer sizes are correct.
        // clang defaults to hidden visibility on wasm and bindgen skips hidden
        // functions, hence -fvisibility=default.
        builder = builder
            .clang_arg("--target=wasm32-wasip1")
            .clang_arg(format!("--sysroot={}", sysroot.display()))
            .clang_arg("-fvisibility=default");
    }

    let bindings = builder.generate().expect("bindgen failed");
    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_dir.join("bindings.rs"))
        .expect("failed to write bindings");

    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=shims/wasm_shims.c");
    println!("cargo:rerun-if-changed={}", src_dir.display());
    println!("cargo:rerun-if-changed={}", include_dir.display());

    // --- library --------------------------------------------------------
    if is_wasm {
        // Host CFLAGS (e.g. from conda: -march=nocona) must not leak into the
        // wasm cross-compile.
        for var in ["CFLAGS", "CPPFLAGS", "LDFLAGS"] {
            env::remove_var(var);
        }

        // Cross-compile the box3d C sources for wasm32. We compile with the
        // wasip1 triple so libc headers work; the resulting objects link fine
        // into a wasm32-unknown-unknown module.
        let mut build = cc::Build::new();
        build
            .target("wasm32-wasip1")
            .compiler("clang")
            .archiver("llvm-ar")
            .flag(format!("--sysroot={}", sysroot.display()))
            .flag("-fno-stack-protector")
            .define("NDEBUG", None)
            .opt_level(2)
            .include(&include_dir)
            .include(&src_dir)
            .file("shims/wasm_shims.c");

        let mut sources: Vec<_> = std::fs::read_dir(&src_dir)
            .expect("cannot read box3d src dir")
            .filter_map(|e| {
                let p = e.ok()?.path();
                (p.extension()? == "c").then_some(p)
            })
            .collect();
        sources.sort();
        assert!(!sources.is_empty(), "no C sources found in {}", src_dir.display());
        for s in sources {
            build.file(s);
        }

        build.compile("box3d");

        // libc.a from the wasi sysroot supplies malloc/mem/math/qsort/snprintf.
        // The shims above shadow everything that would pull in WASI imports.
        println!(
            "cargo:rustc-link-search=native={}",
            sysroot.join("lib/wasm32-wasip1").display()
        );
        println!("cargo:rustc-link-lib=static=c");
    } else {
        // Native: link the prebuilt library from the repo's cmake build.
        let lib_dir = box3d_root.join("build/src");
        if !lib_dir.join("libbox3d.a").exists() {
            panic!(
                "libbox3d.a not found in {}. Build box3d first (./build.sh in the repo root).",
                lib_dir.display()
            );
        }
        println!("cargo:rustc-link-search=native={}", lib_dir.display());
        println!("cargo:rustc-link-lib=static=box3d");
        println!("cargo:rustc-link-lib=m");
    }
}
