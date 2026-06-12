//! Décodage vidéo **accéléré matériellement** (VAAPI, NVDEC/CUDA, DXVA2,
//! D3D11VA, VideoToolbox, VDPAU) via FFI FFmpeg.
//!
//! `ffmpeg-the-third` n'expose pas l'API d'accélération : on configure
//! directement l'`AVCodecContext` (création d'un `hw_device_ctx`, callback
//! `get_format` sélectionnant le format matériel), puis on **rapatrie** les
//! trames GPU vers la mémoire système (`av_hwframe_transfer_data`) pour
//! réutiliser le pipeline RGBA logiciel existant.
//!
//! Tout est conçu autour d'un **repli logiciel sûr** : si aucun périphérique
//! n'est disponible (machine sans GPU, pilote absent, codec non géré), la
//! configuration échoue silencieusement et le décodage logiciel habituel
//! prend le relais. C'est le chemin emprunté lorsque [`setup`] renvoie
//! `None`.

use ffmpeg_the_third as ffmpeg;
use ffmpeg_the_third::ffi;
use std::cell::Cell;
use std::ptr;

thread_local! {
    /// Format de pixel matériel attendu par le décodeur du thread courant.
    /// Lu par le callback `get_format` (qui s'exécute dans ce même thread).
    static HW_PIX_FMT: Cell<ffi::AVPixelFormat> = const { Cell::new(ffi::AVPixelFormat::NONE) };
}

/// Types de périphériques essayés, par ordre de préférence selon la
/// plateforme.
fn preferred_types() -> &'static [ffi::AVHWDeviceType] {
    #[cfg(target_os = "linux")]
    {
        &[
            ffi::AVHWDeviceType::VAAPI,
            ffi::AVHWDeviceType::CUDA,
            ffi::AVHWDeviceType::VDPAU,
        ]
    }
    #[cfg(target_os = "windows")]
    {
        &[
            ffi::AVHWDeviceType::D3D11VA,
            ffi::AVHWDeviceType::DXVA2,
            ffi::AVHWDeviceType::CUDA,
        ]
    }
    #[cfg(target_os = "macos")]
    {
        &[ffi::AVHWDeviceType::VIDEOTOOLBOX]
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        &[]
    }
}

/// Nom lisible d'un type de périphérique (journalisation).
fn type_name(t: ffi::AVHWDeviceType) -> &'static str {
    match t {
        ffi::AVHWDeviceType::VAAPI => "VAAPI",
        ffi::AVHWDeviceType::CUDA => "CUDA/NVDEC",
        ffi::AVHWDeviceType::VDPAU => "VDPAU",
        ffi::AVHWDeviceType::D3D11VA => "D3D11VA",
        ffi::AVHWDeviceType::DXVA2 => "DXVA2",
        ffi::AVHWDeviceType::VIDEOTOOLBOX => "VideoToolbox",
        _ => "?",
    }
}

/// Contexte de décodage matériel actif. Libère le périphérique au drop.
pub struct HwAccel {
    device: *mut ffi::AVBufferRef,
    hw_pixel: ffmpeg::format::Pixel,
    pub name: &'static str,
}

impl Drop for HwAccel {
    fn drop(&mut self) {
        // SAFETY : `device` provient d'`av_buffer_ref`/`av_hwdevice_ctx_create`.
        unsafe { ffi::av_buffer_unref(&mut self.device) };
    }
}

impl HwAccel {
    /// Format de pixel produit par le décodeur matériel (ex. `VAAPI`).
    pub fn hw_pixel(&self) -> ffmpeg::format::Pixel {
        self.hw_pixel
    }
}

/// Callback `get_format` : choisit le format matériel s'il est proposé, sinon
/// se rabat sur le premier format logiciel (le décodage continue alors en
/// logiciel sans casser).
///
/// # Safety
/// Appelé par FFmpeg avec une liste de formats terminée par `NONE`.
unsafe extern "C" fn get_hw_format(
    _ctx: *mut ffi::AVCodecContext,
    mut formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    let wanted = HW_PIX_FMT.with(|c| c.get());
    let mut first = ffi::AVPixelFormat::NONE;
    while !formats.is_null() && *formats != ffi::AVPixelFormat::NONE {
        if *formats == wanted {
            return wanted;
        }
        if first == ffi::AVPixelFormat::NONE {
            first = *formats;
        }
        formats = formats.add(1);
    }
    first
}

/// Tente d'activer l'accélération matérielle sur un `AVCodecContext` **avant
/// son ouverture**. Renvoie `None` (et laisse le contexte intact pour le
/// décodage logiciel) si aucun périphérique compatible n'est disponible.
///
/// # Safety
/// `avctx` doit être un `AVCodecContext` valide et non encore ouvert.
pub unsafe fn setup(avctx: *mut ffi::AVCodecContext) -> Option<HwAccel> {
    // Soupape de secours : force le décodage logiciel.
    if std::env::var_os("OXIPLAY_NO_HWACCEL").is_some() {
        return None;
    }
    let codec = ffi::avcodec_find_decoder((*avctx).codec_id);
    if codec.is_null() {
        return None;
    }

    for &dev_type in preferred_types() {
        // Cherche une config matérielle du codec correspondant au type.
        let mut i = 0;
        loop {
            let config = ffi::avcodec_get_hw_config(codec, i);
            if config.is_null() {
                break;
            }
            i += 1;
            let methods = (*config).methods;
            let device_ctx_method =
                ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX.0 as std::os::raw::c_int;
            if methods & device_ctx_method == 0 || (*config).device_type != dev_type {
                continue;
            }

            // Tente de créer le périphérique.
            let mut device: *mut ffi::AVBufferRef = ptr::null_mut();
            let rc =
                ffi::av_hwdevice_ctx_create(&mut device, dev_type, ptr::null(), ptr::null_mut(), 0);
            if rc < 0 || device.is_null() {
                log::debug!(
                    "périphérique {} indisponible (rc={rc})",
                    type_name(dev_type)
                );
                break; // type suivant
            }

            (*avctx).hw_device_ctx = ffi::av_buffer_ref(device);
            (*avctx).get_format = Some(get_hw_format);
            HW_PIX_FMT.with(|c| c.set((*config).pix_fmt));
            let name = type_name(dev_type);
            log::info!("accélération matérielle : {name}");
            return Some(HwAccel {
                device,
                hw_pixel: ffmpeg::format::Pixel::from((*config).pix_fmt),
                name,
            });
        }
    }
    None
}

/// Rapatrie une trame GPU vers une trame logicielle réutilisable par le
/// pipeline RGBA. Renvoie `None` en cas d'échec (l'appelant ignore alors la
/// trame).
pub fn transfer(hw_frame: &ffmpeg::frame::Video) -> Option<ffmpeg::frame::Video> {
    let mut sw = ffmpeg::frame::Video::empty();
    // SAFETY : les deux pointeurs sont des AVFrame valides ; `transfer_data`
    // alloue le tampon de destination, `copy_props` recopie pts/durée.
    unsafe {
        if ffi::av_hwframe_transfer_data(sw.as_mut_ptr(), hw_frame.as_ptr(), 0) < 0 {
            return None;
        }
        ffi::av_frame_copy_props(sw.as_mut_ptr(), hw_frame.as_ptr());
    }
    Some(sw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_types_non_empty_on_desktop() {
        // Sur les plateformes de bureau, au moins un type est proposé.
        #[cfg(any(target_os = "linux", target_os = "windows", target_os = "macos"))]
        assert!(!preferred_types().is_empty());
    }

    #[test]
    fn type_names_are_known() {
        assert_eq!(type_name(ffi::AVHWDeviceType::VAAPI), "VAAPI");
        assert_eq!(type_name(ffi::AVHWDeviceType::VIDEOTOOLBOX), "VideoToolbox");
    }
}
