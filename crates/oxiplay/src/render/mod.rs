//! Rendu vidéo GPU « zéro copie » + HDR (feature `gpu`).
//!
//! Convertit les plans **YUV** décodés en RGB (matrices BT.601/709/2020,
//! tone-mapping PQ/HLG) dans un shader wgpu (`yuv_hdr.wgsl`) et produit une
//! `wgpu::Texture` importable comme [`slint::Image`]. Partage le `device`/
//! `queue` de Slint (obtenus via `set_rendering_notifier`).
//!
//! État : pipeline **compilé et vérifié** ; le câblage runtime (sortie YUV du
//! décodeur, notifier Slint) et la justesse visuelle se finalisent sur un
//! écran. Voir `docs/RENDER_WGPU.md`.

use crate::video::YuvFrame;
use slint::wgpu_28::wgpu;
use std::cell::RefCell;

thread_local! {
    /// Le renderer vit sur le thread de l'événementiel Slint (où le device est
    /// capturé par le notifier et où les images sont présentées). Pas besoin
    /// qu'il soit `Send` : initialisation et usage se font sur ce même thread.
    static RENDERER: RefCell<Option<GpuVideoRenderer>> = const { RefCell::new(None) };
}

/// Initialise le renderer GPU à partir du device/queue fournis par Slint
/// (appelé depuis le notifier de rendu, à `RenderingSetup`). Active dès lors
/// l'extraction des plans YUV côté décodeur ([`crate::video::GPU_ACTIVE`]).
pub fn init_renderer(device: wgpu::Device, queue: wgpu::Queue) {
    RENDERER.with(|r| {
        *r.borrow_mut() = Some(GpuVideoRenderer::new(device, queue));
    });
    crate::video::GPU_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Convertit des plans YUV en image Slint via le pipeline GPU (YUV→RGB + HDR).
/// Retourne `None` si le renderer n'est pas encore prêt ou si l'import échoue —
/// l'appelant retombe alors sur le chemin RGBA logiciel.
pub fn render_yuv_to_image(frame: &YuvFrame) -> Option<slint::Image> {
    RENDERER.with(|r| {
        let mut slot = r.borrow_mut();
        let renderer = slot.as_mut()?;
        let texture = renderer.render(frame);
        slint::Image::try_from(texture).ok()
    })
}

/// Textures d'entrée (un plan = une texture R8), recréées si la géométrie change.
struct Planes {
    y: wgpu::Texture,
    u: wgpu::Texture,
    v: wgpu::Texture,
    width: u32,
    height: u32,
    chroma_width: u32,
    chroma_height: u32,
}

/// Pipeline de rendu vidéo GPU, partageant le device/queue de Slint.
pub struct GpuVideoRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::RenderPipeline,
    bind_group_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    uniform: wgpu::Buffer,
    planes: Option<Planes>,
}

impl GpuVideoRenderer {
    /// Construit le pipeline à partir du device/queue fournis par Slint.
    pub fn new(device: wgpu::Device, queue: wgpu::Queue) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("oxiplay-yuv-hdr"),
            source: wgpu::ShaderSource::Wgsl(include_str!("yuv_hdr.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("oxiplay-yuv-bgl"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 4,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("oxiplay-yuv-pl"),
            bind_group_layouts: &[&bind_group_layout],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("oxiplay-yuv-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("oxiplay-yuv-sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });

        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("oxiplay-yuv-params"),
            size: 16, // 3× u32 + 1× f32
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            device,
            queue,
            pipeline,
            bind_group_layout,
            sampler,
            uniform,
            planes: None,
        }
    }

    /// Convertit une image YUV en texture RGBA prête pour Slint.
    pub fn render(&mut self, frame: &YuvFrame) -> wgpu::Texture {
        self.ensure_planes(frame);
        let planes = self.planes.as_ref().expect("plans initialisés");

        // Upload des trois plans (le sampler bilinéaire upscale la chroma).
        write_plane(
            &self.queue,
            &planes.y,
            &frame.y,
            frame.y_stride,
            frame.height,
        );
        write_plane(
            &self.queue,
            &planes.u,
            &frame.u,
            frame.uv_stride,
            frame.chroma_height,
        );
        write_plane(
            &self.queue,
            &planes.v,
            &frame.v,
            frame.uv_stride,
            frame.chroma_height,
        );

        // Paramètres colorimétriques (Params { matrix, full_range, transfer,
        // sdr_white }), 16 octets little-endian.
        let mut params = [0u8; 16];
        params[0..4].copy_from_slice(&frame.matrix.to_le_bytes());
        params[4..8].copy_from_slice(&frame.full_range.to_le_bytes());
        params[8..12].copy_from_slice(&frame.transfer.to_le_bytes());
        params[12..16].copy_from_slice(&203.0f32.to_le_bytes()); // blanc SDR de réf.
        self.queue.write_buffer(&self.uniform, 0, &params);

        let view = |t: &wgpu::Texture| t.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = self.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("oxiplay-yuv-bg"),
            layout: &self.bind_group_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view(&planes.y)),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view(&planes.u)),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(&view(&planes.v)),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: self.uniform.as_entire_binding(),
                },
            ],
        });

        let output = self.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("oxiplay-yuv-output"),
            size: wgpu::Extent3d {
                width: frame.width,
                height: frame.height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let output_view = output.create_view(&wgpu::TextureViewDescriptor::default());

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("oxiplay-yuv-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("oxiplay-yuv-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &output_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1); // triangle plein écran
        }
        self.queue.submit(Some(encoder.finish()));
        output
    }

    /// (Re)crée les textures d'entrée si la géométrie a changé.
    fn ensure_planes(&mut self, frame: &YuvFrame) {
        let ok = matches!(
            &self.planes,
            Some(p) if p.width == frame.width && p.height == frame.height
                && p.chroma_width == frame.chroma_width && p.chroma_height == frame.chroma_height
        );
        if ok {
            return;
        }
        let make = |w: u32, h: u32, label: &str| {
            self.device.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            })
        };
        self.planes = Some(Planes {
            y: make(frame.width, frame.height, "oxiplay-plane-y"),
            u: make(frame.chroma_width, frame.chroma_height, "oxiplay-plane-u"),
            v: make(frame.chroma_width, frame.chroma_height, "oxiplay-plane-v"),
            width: frame.width,
            height: frame.height,
            chroma_width: frame.chroma_width,
            chroma_height: frame.chroma_height,
        });
    }
}

/// Entrée de layout pour une texture R8 échantillonnable (un plan YUV).
fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

/// Téléverse un plan (stride éventuellement aligné par FFmpeg) dans sa texture.
fn write_plane(queue: &wgpu::Queue, texture: &wgpu::Texture, data: &[u8], stride: u32, rows: u32) {
    let size = texture.size();
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        data,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(stride),
            rows_per_image: Some(rows),
        },
        wgpu::Extent3d {
            width: size.width,
            height: size.height,
            depth_or_array_layers: 1,
        },
    );
}
