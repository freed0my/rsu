//! Compiles the WGSL shader to SPIR-V with naga, so building needs nothing
//! beyond cargo (no glslc / Vulkan SDK on the build machine).

use naga::back::spv;
use naga::valid::{Capabilities, ValidationFlags, Validator};

fn main() {
    println!("cargo:rerun-if-changed=src/shader.wgsl");
    let src = std::fs::read_to_string("src/shader.wgsl").expect("read shader.wgsl");
    let module =
        naga::front::wgsl::parse_str(&src).unwrap_or_else(|e| panic!("shader.wgsl:\n{}", e.emit_to_string(&src)));
    let info = Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .unwrap_or_else(|e| panic!("shader.wgsl validation: {e:?}"));
    // No coordinate-space adjustment: the shader already works in Vulkan's
    // clip space (y down), which matches pixel coordinates.
    let options = spv::Options { flags: spv::WriterFlags::empty(), ..Default::default() };
    let words = spv::write_vec(&module, &info, &options, None).expect("SPIR-V generation");
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    let out = std::path::Path::new(&std::env::var("OUT_DIR").unwrap()).join("shader.spv");
    std::fs::write(out, bytes).expect("write shader.spv");
}
