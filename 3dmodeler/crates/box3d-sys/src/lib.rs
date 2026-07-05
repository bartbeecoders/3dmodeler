//! Raw FFI bindings to the box3d physics library.
//!
//! - Native targets link the prebuilt static library from `<repo>/build/src`.
//! - `wasm32-*` targets cross-compile the box3d C sources with clang against
//!   the wasi sysroot (see `build.rs` and `shims/wasm_shims.c`).

#![allow(non_upper_case_globals)]
#![allow(non_camel_case_types)]
#![allow(non_snake_case)]

include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
