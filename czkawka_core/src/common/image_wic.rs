//! Image decoding through Windows Imaging Component, the system codec registry used by Explorer.
//!
//! Order used by `image.rs`, so a complete installed codec wins over a partial built-in one:
//! third-party WIC decoder, then image-rs, then Microsoft WIC decoders.

use std::collections::HashMap;
use std::sync::OnceLock;

use image::{DynamicImage, RgbaImage};
use log::{debug, warn};
use windows::Win32::Foundation::{GENERIC_READ, RPC_E_CHANGED_MODE};
use windows::Win32::Graphics::Imaging::{
    CLSID_WICImagingFactory, GUID_VendorMicrosoft, GUID_VendorMicrosoftBuiltIn, GUID_WICPixelFormat32bppRGBA, IWICBitmapDecoderInfo, IWICImagingFactory, WICBitmapDitherTypeNone,
    WICBitmapPaletteTypeCustom, WICComponentEnumerateDefault, WICDecodeMetadataCacheOnDemand, WICDecoder,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize, IEnumUnknown};
use windows::core::{HSTRING, Interface};

/// Who provides the WIC decoder registered for an extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WicDecoderOrigin {
    Microsoft,
    ThirdParty,
}

/// Keeps COM initialized for the current thread while alive.
struct ComGuard {
    uninitialize: bool,
}

impl ComGuard {
    fn new() -> Result<Self, String> {
        // SAFETY: plain COM initialization; balanced by CoUninitialize in Drop when it succeeded.
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == RPC_E_CHANGED_MODE {
            // The thread already runs COM in single-threaded mode (e.g. the GTK main thread): usable as is.
            return Ok(Self { uninitialize: false });
        }
        hr.ok().map_err(|e| format!("COM initialization failed: {e}"))?;
        Ok(Self { uninitialize: true })
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninitialize {
            // SAFETY: matches the successful CoInitializeEx of this guard, on the same thread.
            unsafe { CoUninitialize() };
        }
    }
}

fn imaging_factory() -> Result<IWICImagingFactory, String> {
    // SAFETY: COM is initialized on this thread by the caller's ComGuard.
    unsafe { CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER) }.map_err(|e| format!("cannot create the WIC imaging factory: {e}"))
}

/// Lower-case extensions (without dot) of every installed WIC decoder, with who provides it.
/// A third-party decoder wins when Microsoft also registers one for the same extension.
pub(crate) fn wic_decoder_extensions() -> &'static HashMap<String, WicDecoderOrigin> {
    static EXTENSIONS: OnceLock<HashMap<String, WicDecoderOrigin>> = OnceLock::new();
    EXTENSIONS.get_or_init(|| {
        let extensions = enumerate_decoders().unwrap_or_else(|e| {
            warn!("Cannot list the installed WIC image decoders: {e}");
            HashMap::new()
        });
        debug!("WIC image decoders: {extensions:?}");
        extensions
    })
}

pub(crate) fn wic_decoder_origin(extension: &str) -> Option<WicDecoderOrigin> {
    wic_decoder_extensions().get(extension).copied()
}

/// Extensions of the WIC decoders installed by third parties (not shipped by Microsoft).
pub(crate) fn third_party_extensions() -> impl Iterator<Item = &'static str> {
    wic_decoder_extensions()
        .iter()
        .filter(|(_, origin)| **origin == WicDecoderOrigin::ThirdParty)
        .map(|(extension, _)| extension.as_str())
}

fn enumerate_decoders() -> Result<HashMap<String, WicDecoderOrigin>, String> {
    let _com = ComGuard::new()?;
    let factory = imaging_factory()?;
    // SAFETY: valid factory; the flags are the documented WICComponentType / enumerate options.
    let decoders: IEnumUnknown =
        unsafe { factory.CreateComponentEnumerator(WICDecoder.0 as u32, WICComponentEnumerateDefault.0 as u32) }.map_err(|e| format!("cannot enumerate WIC decoders: {e}"))?;

    let mut extensions = HashMap::new();
    loop {
        let mut item = [None];
        let mut fetched = 0;
        // SAFETY: one-element output buffer and fetched counter, as IEnumUnknown::Next expects.
        let hr = unsafe { decoders.Next(&mut item, Some(&mut fetched)) };
        if hr.is_err() || fetched == 0 {
            break;
        }
        let Some(Ok(info)) = item[0].take().map(|unknown| unknown.cast::<IWICBitmapDecoderInfo>()) else {
            continue;
        };
        let origin = decoder_origin(&info);
        for extension in decoder_file_extensions(&info) {
            let entry = extensions.entry(extension).or_insert(origin);
            if origin == WicDecoderOrigin::ThirdParty {
                *entry = origin;
            }
        }
    }
    Ok(extensions)
}

fn decoder_origin(info: &IWICBitmapDecoderInfo) -> WicDecoderOrigin {
    // SAFETY: valid decoder info obtained from the WIC enumerator.
    match unsafe { info.GetVendorGUID() } {
        Ok(vendor) if vendor == GUID_VendorMicrosoft || vendor == GUID_VendorMicrosoftBuiltIn => WicDecoderOrigin::Microsoft,
        _ => WicDecoderOrigin::ThirdParty,
    }
}

/// Parses the decoder's comma separated extension list (".dds,.DDS") into "dds".
fn decoder_file_extensions(info: &IWICBitmapDecoderInfo) -> Vec<String> {
    let mut length = 0;
    // SAFETY: a zero-sized buffer only queries the required length; WIC reports it with an
    // "insufficient buffer" error, so only the length matters here.
    let _ = unsafe { info.GetFileExtensions(&mut [], &mut length) };
    if length == 0 {
        return Vec::new();
    }
    let mut buffer = vec![0u16; length as usize];
    // SAFETY: the buffer has the length WIC asked for.
    if unsafe { info.GetFileExtensions(&mut buffer, &mut length) }.is_err() {
        return Vec::new();
    }
    let text = String::from_utf16_lossy(buffer.split(|&c| c == 0).next().unwrap_or_default());
    text.split(',')
        .map(|ext| ext.trim().trim_start_matches('.').to_lowercase())
        .filter(|ext| !ext.is_empty())
        .collect()
}

/// Decodes the first frame of an image file through WIC, like Explorer does.
pub(crate) fn decode_with_wic(path: &str, max_pixels: u32) -> Result<DynamicImage, String> {
    let _com = ComGuard::new()?;
    let factory = imaging_factory()?;
    // SAFETY: valid factory and a null-terminated path held by the HSTRING for the whole call.
    let decoder = unsafe { factory.CreateDecoderFromFilename(&HSTRING::from(path), None, GENERIC_READ, WICDecodeMetadataCacheOnDemand) }
        .map_err(|e| format!("no Windows image decoder could open the file: {e}"))?;
    // SAFETY: valid decoder; frame 0 always exists for a successfully opened image.
    let frame = unsafe { decoder.GetFrame(0) }.map_err(|e| format!("cannot read the image frame: {e}"))?;

    let (mut width, mut height) = (0, 0);
    // SAFETY: valid frame and output pointers.
    unsafe { frame.GetSize(&mut width, &mut height) }.map_err(|e| format!("cannot read the image size: {e}"))?;
    if width == 0 || height == 0 {
        return Err("the image has no pixels".to_string());
    }
    if u64::from(width) * u64::from(height) > u64::from(max_pixels) {
        return Err(format!("the image is too large ({width}x{height})"));
    }

    // SAFETY: valid factory.
    let converter = unsafe { factory.CreateFormatConverter() }.map_err(|e| format!("cannot convert the image: {e}"))?;
    // SAFETY: valid converter and frame; the converter only reads from the frame.
    unsafe { converter.Initialize(&frame, &GUID_WICPixelFormat32bppRGBA, WICBitmapDitherTypeNone, None, 0.0, WICBitmapPaletteTypeCustom) }
        .map_err(|e| format!("cannot convert the image to RGBA: {e}"))?;

    let stride = width as usize * 4;
    let mut pixels = vec![0u8; stride * height as usize];
    // SAFETY: the buffer holds exactly height rows of `stride` bytes.
    unsafe { converter.CopyPixels(std::ptr::null(), stride as u32, &mut pixels) }.map_err(|e| format!("cannot read the image pixels: {e}"))?;

    RgbaImage::from_raw(width, height, pixels)
        .map(DynamicImage::ImageRgba8)
        .ok_or_else(|| "the decoded image buffer has an unexpected size".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builtin_decoders_are_listed_as_microsoft() {
        assert_eq!(wic_decoder_origin("png"), Some(WicDecoderOrigin::Microsoft));
        assert_eq!(wic_decoder_origin("jpg"), Some(WicDecoderOrigin::Microsoft));
        assert_eq!(wic_decoder_origin("txt"), None);
    }

    #[test]
    fn test_third_party_extensions_exclude_microsoft_decoders() {
        let extensions: Vec<&str> = third_party_extensions().collect();
        assert!(!extensions.contains(&"png"));
        assert!(!extensions.contains(&"jpg"));
        assert!(extensions.iter().all(|ext| wic_decoder_origin(ext) == Some(WicDecoderOrigin::ThirdParty)));
    }

    #[test]
    fn test_decode_matches_image_rs() {
        let path = "test_resources/images/normal.jpg";
        let wic = decode_with_wic(path, u32::MAX).unwrap();
        let reference = image::open(path).unwrap();
        assert_eq!((wic.width(), wic.height()), (reference.width(), reference.height()));
    }

    #[test]
    fn test_decode_rejects_too_large_images() {
        decode_with_wic("test_resources/images/normal.jpg", 1).unwrap_err();
    }
}
