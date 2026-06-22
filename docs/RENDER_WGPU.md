# Rendu vidéo wgpu « zéro copie » + HDR

État : **conception + shader prêts** ; câblage runtime à faire **sur une machine
avec écran/GPU** (le rendu ne se valide pas en headless). Ce document est le
guide d'implémentation, calé sur l'API réelle de Slint 1.16.

## Pourquoi

Aujourd'hui (chemin CPU, `decoder/video.rs` → `ui/mod.rs`) :

```
décodeur → swscale YUV→RGBA (CPU) → Vec compact (copie) → SharedPixelBuffer (copie) → upload GPU Slint
```

En 4K60 c'est ~6 Go/s de trafic mémoire après décodage (cf. audit) : c'est le
plafond. De plus, RGBA8 ne peut pas porter le HDR, et la conversion
colorimétrique reste approximative.

Cible (zéro copie) :

```
décodeur → plans YUV (Arc) → upload textures GPU → shader YUV→RGB + colorimétrie + tone-map HDR → texture wgpu → Image Slint
```

Plus aucune conversion ni copie RGBA sur CPU ; le HDR est géré dans le shader.

## L'API Slint qui rend ça possible (vérifiée dans 1.16)

- Feature crate **`unstable-wgpu-28`** (+ renderer `renderer-femtovg-wgpu`).
- `i-slint-core/graphics/wgpu_28.rs` expose **`impl TryFrom<wgpu_28::Texture>
  for slint::Image`** → on convertit une texture wgpu en `Image` et on
  l'assigne à la propriété `video-frame`.
- Pour partager **le même device wgpu** que Slint : configurer le backend via
  `slint::BackendSelector::new().require_wgpu_28(WGPUConfiguration::...)` (ou
  laisser Slint créer le device et le récupérer dans le callback de
  `window().set_rendering_notifier(...)` qui fournit le `GraphicsAPI::WGPU28
  { device, queue, .. }`). Il **faut** le device/queue de Slint pour que la
  texture produite soit consommable par son moteur de rendu.
- **Pas de dépendance `wgpu` séparée** : Slint ré-exporte le crate exact sous
  `slint::wgpu_28::wgpu` — l'utiliser garantit l'unité de version. Un exemple
  complet (allouer une texture, la poser en `Image`) figure dans la doc du
  module `slint::wgpu_28`.

## Changements de pipeline

1. **`video/mod.rs` — `VideoFrameData`** : ajouter une variante portant les
   plans YUV au lieu du RGBA :
   ```rust
   pub enum FramePixels {
       Rgba(Vec<u8>),                 // chemin CPU actuel (fallback)
       Yuv { y: Plane, u: Plane, v: Plane, // planaire
             space: ColorSpace, range: ColorRange, transfer: ColorTransfer },
   }
   struct Plane { data: Vec<u8>, stride: usize, width: u32, height: u32 }
   ```
   Conserver `pts_us`, `width`, `height`.

2. **`decoder/video.rs`** : sous la feature `gpu`, **ne pas** faire le swscale
   RGBA ; à la place, copier les plans YUV (déjà alignés) du `frame::Video`
   décodé (ou rapatrié du HW) et remplir `FramePixels::Yuv`. Les métadonnées
   colorimétriques viennent de `decoded.color_space()/color_range()` et du
   `color_trc` (déjà lus pour la fiche média et la correction swscale).
   Le filtre vidéo (yadif/transpose/eq) reste en amont, inchangé.

3. **Nouveau module `render/` (feature `gpu`)** :
   - `mod.rs` : `GpuRenderer { device, queue, pipeline, bind_group_layout,
     sampler, uniform_buffer, textures: Option<YuvTextures> }`.
   - À l'init : créer le `RenderPipeline` à partir de `yuv_hdr.wgsl`
     (déjà écrit, voir `src/render/yuv_hdr.wgsl`), un sampler bilinéaire,
     un uniform buffer `Params`.
   - Par image : (ré)allouer les 3 textures R8 si la géométrie change ;
     `queue.write_texture` pour chaque plan ; mettre à jour `Params`
     (matrix/range/transfer selon les métadonnées) ; encoder une passe de
     rendu vers une **texture RGBA de sortie** ; renvoyer cette texture.
   - `slint::Image::try_from(output_texture)` → assignée à `video-frame`.

4. **`app/mod.rs` — `make_frame_sink`** : sous `gpu`, router les
   `FramePixels::Yuv` vers le `GpuRenderer` (sur le thread UI, via
   `invoke_from_event_loop`, comme aujourd'hui) au lieu de `frame_to_image`.

## Le shader (`src/render/yuv_hdr.wgsl`) — déjà écrit

- YUV→RGB avec coefficients **BT.601 / 709 / 2020** et dé-quantification
  plage limitée/complète.
- **HDR** : EOTF **PQ** (SMPTE 2084) et **HLG** (ARIB STD-B67), tone-mapping
  Reinhard étendu, conversion de gamut **BT.2020→BT.709**, puis OETF sRGB.
- SDR : passe-plat (la vidéo est déjà en gamma d'affichage).

À affiner **sur écran** : choix du tone-mapper (Reinhard vs Hable/ACES), le
`sdr_white` de référence (203 nits par défaut), et la justesse du gamut.

## Étapes (avec frontière de validation)

| Étape | Vérifiable headless | Besoin écran |
|---|---|---|
| Shader WGSL | ✅ (relecture) | justesse couleurs |
| Feature `gpu` + deps wgpu-28 | ✅ `cargo build --features gpu` | — |
| `FramePixels::Yuv` + décodeur YUV | ✅ compile | — |
| Module `render/` (pipeline wgpu) | ✅ compile | rendu réel |
| Partage device Slint + `Image::try_from` | ⚠️ compile | **indispensable** |
| HDR (PQ/HLG) bout en bout | ❌ | **indispensable** (contenu HDR) |

## Repli

La feature `gpu` est **désactivée par défaut** : le chemin CPU actuel
(swscale RGBA, déjà colorimétriquement correct depuis le correctif BT.709/2020)
reste le défaut et le repli si l'init wgpu échoue. Aucune régression.
