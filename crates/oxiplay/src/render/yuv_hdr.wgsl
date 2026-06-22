// Conversion YUV → RGB avec colorimétrie correcte (BT.601 / BT.709 / BT.2020)
// et tone-mapping HDR (PQ / HLG → SDR), exécutée sur le GPU.
//
// Entrée : trois plans YUV (planaire, ex. YUV420P) en textures R8.
//   - plane_y : luma pleine résolution
//   - plane_u, plane_v : chroma sous-échantillonnée (l'échantillonnage
//     bilinéaire du sampler gère l'upscale 4:2:0).
// Sortie : RGBA linéaire→sRGB prêt pour l'affichage (ou la texture Slint).
//
// Les paramètres (matrice colorimétrique, plage, type de transfert HDR) sont
// poussés via un uniform `Params`, calculés côté Rust depuis les métadonnées
// de la trame (color_space / color_range / color_trc).

struct Params {
    // 0 = BT.601, 1 = BT.709, 2 = BT.2020
    matrix: u32,
    // 0 = plage limitée (16..235), 1 = plage complète (0..255)
    full_range: u32,
    // 0 = SDR, 1 = HDR PQ (SMPTE 2084), 2 = HDR HLG (ARIB STD-B67)
    transfer: u32,
    // Luminance crête de la cible SDR (nits), pour le tone-mapping (ex. 203).
    sdr_white: f32,
};

@group(0) @binding(0) var plane_y: texture_2d<f32>;
@group(0) @binding(1) var plane_u: texture_2d<f32>;
@group(0) @binding(2) var plane_v: texture_2d<f32>;
@group(0) @binding(3) var samp: sampler;
@group(0) @binding(4) var<uniform> params: Params;

struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// Triangle plein écran (3 sommets), pas de vertex buffer.
@vertex
fn vs_main(@builtin(vertex_index) idx: u32) -> VsOut {
    var out: VsOut;
    let x = f32((idx << 1u) & 2u);
    let y = f32(idx & 2u);
    out.uv = vec2<f32>(x, y);
    out.pos = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

// Coefficients luma (Kr, Kb) par espace colorimétrique ; Kg = 1 - Kr - Kb.
fn luma_coeffs(matrix: u32) -> vec2<f32> {
    if (matrix == 2u) {
        return vec2<f32>(0.2627, 0.0593); // BT.2020
    } else if (matrix == 1u) {
        return vec2<f32>(0.2126, 0.0722); // BT.709
    }
    return vec2<f32>(0.299, 0.114);       // BT.601
}

// YUV (0..1) → RGB (0..1) selon la matrice et la plage.
fn yuv_to_rgb(yuv: vec3<f32>) -> vec3<f32> {
    var y = yuv.x;
    var u = yuv.y - 0.5;
    var v = yuv.z - 0.5;
    // Dé-quantification de la plage limitée vers [0,1] plein.
    if (params.full_range == 0u) {
        y = (y - 16.0 / 255.0) * (255.0 / 219.0);
        u = u * (255.0 / 224.0);
        v = v * (255.0 / 224.0);
    }
    let k = luma_coeffs(params.matrix);
    let kr = k.x;
    let kb = k.y;
    let kg = 1.0 - kr - kb;
    // Inverse de la matrice YCbCr non normalisée.
    let r = y + 2.0 * (1.0 - kr) * v;
    let b = y + 2.0 * (1.0 - kb) * u;
    let g = (y - kr * r - kb * b) / kg;
    return vec3<f32>(r, g, b);
}

// EOTF PQ (SMPTE 2084) : code [0,1] → luminance linéaire normalisée (0..1 pour
// ~10000 nits).
fn pq_eotf(e: vec3<f32>) -> vec3<f32> {
    let m1 = 0.1593017578125;
    let m2 = 78.84375;
    let c1 = 0.8359375;
    let c2 = 18.8515625;
    let c3 = 18.6875;
    let ep = pow(max(e, vec3<f32>(0.0)), vec3<f32>(1.0 / m2));
    let num = max(ep - c1, vec3<f32>(0.0));
    let den = c2 - c3 * ep;
    return pow(num / den, vec3<f32>(1.0 / m1));
}

// EOTF HLG (ARIB STD-B67) : code [0,1] → scène linéaire (0..1).
fn hlg_eotf(e: vec3<f32>) -> vec3<f32> {
    let a = 0.17883277;
    let b = 0.28466892;
    let c = 0.55991073;
    var o: vec3<f32>;
    for (var i = 0; i < 3; i = i + 1) {
        let x = e[i];
        if (x <= 0.5) {
            o[i] = (x * x) / 3.0;
        } else {
            o[i] = (exp((x - c) / a) + b) / 12.0;
        }
    }
    return o;
}

// Tone-mapping Reinhard étendu (luminance crête → SDR), simple et stable.
fn tonemap(c: vec3<f32>) -> vec3<f32> {
    let l = dot(c, vec3<f32>(0.2627, 0.6780, 0.0593)); // luma BT.2020
    let l_out = l / (1.0 + l);
    let scale = select(1.0, l_out / l, l > 0.0001);
    return c * scale;
}

// Conversion approximative du gamut BT.2020 → BT.709 (matrice fixe).
fn bt2020_to_bt709(c: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        dot(c, vec3<f32>(1.6605, -0.5876, -0.0728)),
        dot(c, vec3<f32>(-0.1246, 1.1329, -0.0083)),
        dot(c, vec3<f32>(-0.0182, -0.1006, 1.1187)),
    );
}

// Encodage sRGB (OETF) pour l'affichage.
fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let yuv = vec3<f32>(
        textureSample(plane_y, samp, in.uv).r,
        textureSample(plane_u, samp, in.uv).r,
        textureSample(plane_v, samp, in.uv).r,
    );
    var rgb = yuv_to_rgb(yuv);

    if (params.transfer == 1u) {
        // HDR PQ : EOTF → tone-map → gamut 2020→709 → sRGB.
        rgb = pq_eotf(rgb) * (10000.0 / params.sdr_white);
        rgb = tonemap(rgb);
        rgb = bt2020_to_bt709(rgb);
        rgb = linear_to_srgb(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)));
    } else if (params.transfer == 2u) {
        // HDR HLG : EOTF → tone-map → gamut 2020→709 → sRGB.
        rgb = hlg_eotf(rgb);
        rgb = tonemap(rgb);
        rgb = bt2020_to_bt709(rgb);
        rgb = linear_to_srgb(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)));
    }
    // SDR : la vidéo est déjà encodée en gamma d'affichage, on la laisse telle
    // quelle (clamp de sécurité).
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
