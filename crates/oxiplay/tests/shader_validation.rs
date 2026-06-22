//! Validation statique du shader de rendu GPU (`src/render/yuv_hdr.wgsl`).
//!
//! wgpu ne compile/valide le WGSL qu'au **runtime** (sur GPU) ; ce test le
//! valide via naga à la compilation des tests, indépendamment de la feature
//! `gpu`, pour que la CI attrape toute erreur de syntaxe ou de type du shader.

#[test]
fn yuv_hdr_shader_is_valid() {
    let src = include_str!("../src/render/yuv_hdr.wgsl");
    let module = naga::front::wgsl::parse_str(src)
        .unwrap_or_else(|e| panic!("WGSL invalide : {}", e.emit_to_string(src)));
    let mut validator = naga::valid::Validator::new(
        naga::valid::ValidationFlags::all(),
        naga::valid::Capabilities::all(),
    );
    validator
        .validate(&module)
        .unwrap_or_else(|e| panic!("validation du shader échouée : {e:?}"));
}
