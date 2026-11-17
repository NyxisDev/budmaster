//! Фото товаров: проверка, пережатие, хранение на диске.
//!
//! Загруженный файл никогда не сохраняется как есть. Сервер определяет формат по содержимому
//! (а не по заявленному типу), декодирует картинку и сохраняет заново как JPEG. Заодно
//! пропадают метаданные телефона — в том числе координаты места съёмки.

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::path::{Path, PathBuf};
use std::time::Duration;

use image::codecs::jpeg::JpegEncoder;
use axum::body::Bytes;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits, RgbImage};
use tokio::sync::Semaphore;
use uuid::Uuid;

/// Предел размера загружаемого файла.
pub const MAX_UPLOAD_BYTES: usize = 10 * 1024 * 1024;
/// Сколько фото может быть у товара.
pub const MAX_IMAGES_PER_PRODUCT: i64 = 5;

/// Картинка крупнее по любой стороне отклоняется ещё до декодирования: маленький файл
/// может заявлять огромные размеры и при распаковке занять гигабайты памяти.
const MAX_SIDE: u32 = 8000;
/// Память на раскодированную картинку. 160 МБ — это, например, 50-мегапиксельный снимок
/// телефона (8160×6120). Больше на одно фото сервер не тратит: дальше картинка только уменьшается.
const MAX_DECODE_BYTES: u64 = 160 * 1024 * 1024;
/// Размеры сохраняемых копий по длинной стороне.
const FULL_SIDE: u32 = 1600;
const THUMB_SIDE: u32 = 480;

/// Обработка одной картинки занимает до 160 МБ и ядро процессора,
/// поэтому одновременно обрабатываются не больше двух.
static SLOTS: Semaphore = Semaphore::const_new(2);
const QUEUE_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, thiserror::Error)]
pub enum ImageError {
    #[error("это не JPEG, PNG или WebP")]
    UnsupportedFormat,
    #[error("картинка слишком большая по размерам")]
    TooLarge,
    #[error("файл повреждён")]
    Corrupted,
    #[error("сервер занят обработкой других фото")]
    Busy,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub struct Processed {
    pub full: Vec<u8>,
    pub thumb: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

pub async fn process(bytes: Bytes) -> Result<Processed, ImageError> {
    let slot = tokio::time::timeout(QUEUE_TIMEOUT, SLOTS.acquire())
        .await
        .map_err(|_| ImageError::Busy)?
        .map_err(|error| ImageError::Other(error.into()))?;
    // Слот уходит внутрь потока обработки и освобождается, только когда обработка закончилась.
    // Если клиент оборвёт соединение, обработка доработает до конца, но новую в этот слот
    // никто не запустит — лимит «не больше двух одновременно» обрывами не обойти.
    tokio::task::spawn_blocking(move || {
        let _slot = slot;
        process_blocking(&bytes)
    })
    .await
    .map_err(|error| ImageError::Other(error.into()))?
}

fn process_blocking(bytes: &[u8]) -> Result<Processed, ImageError> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|_| ImageError::Corrupted)?;
    if !matches!(reader.format(), Some(ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::WebP)) {
        return Err(ImageError::UnsupportedFormat);
    }

    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_DECODE_BYTES);
    reader.limits(limits);

    let mut decoder = reader.into_decoder().map_err(classify)?;
    // Сколько памяти займёт раскодированная картинка, известно уже из заголовка файла.
    // Проверяем сами, не полагаясь на то, что каждый формат соблюдает лимит заранее.
    if decoder.total_bytes() > MAX_DECODE_BYTES {
        return Err(ImageError::TooLarge);
    }
    let orientation = decoder.orientation().ok();
    let decoded = DynamicImage::from_decoder(decoder).map_err(classify)?;

    // Уменьшение усреднением по площади: память нужна только под результат.
    let mut full = shrink(&decoded, FULL_SIDE);
    drop(decoded);
    // Телефон хранит поворот снимка в метаданных. Мы их выбрасываем, поэтому поворот
    // применяется к самой картинке (уже уменьшенной) — иначе вертикальные фото лягут набок.
    if let Some(orientation) = orientation {
        full.apply_orientation(orientation);
    }
    let thumb = flatten(&shrink(&full, THUMB_SIDE));
    let full = flatten(&full);
    Ok(Processed {
        width: full.width(),
        height: full.height(),
        full: encode_jpeg(&full, 85)?,
        thumb: encode_jpeg(&thumb, 80)?,
    })
}

fn classify(error: image::ImageError) -> ImageError {
    match error {
        image::ImageError::Limits(_) => ImageError::TooLarge,
        image::ImageError::Unsupported(_) => ImageError::UnsupportedFormat,
        _ => ImageError::Corrupted,
    }
}

/// Уменьшает до `max_side` по длинной стороне; маленькие не растягивает.
fn shrink(image: &DynamicImage, max_side: u32) -> DynamicImage {
    if image.width() > max_side || image.height() > max_side {
        image.thumbnail(max_side, max_side)
    } else {
        image.clone()
    }
}

/// Кладёт картинку на белый фон: у JPEG нет прозрачности, и прозрачные области PNG
/// иначе стали бы чёрными.
fn flatten(image: &DynamicImage) -> RgbImage {
    let rgba = image.to_rgba8();
    let mut rgb = RgbImage::new(rgba.width(), rgba.height());
    for (out, pixel) in rgb.pixels_mut().zip(rgba.pixels()) {
        let alpha = u32::from(pixel[3]);
        for channel in 0..3 {
            let blended = (u32::from(pixel[channel]) * alpha + 255 * (255 - alpha) + 127) / 255;
            out[channel] = u8::try_from(blended).unwrap_or(u8::MAX);
        }
    }
    rgb
}

fn encode_jpeg(image: &RgbImage, quality: u8) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, quality)
        .encode_image(image)
        .map_err(|error| ImageError::Other(error.into()))?;
    Ok(out)
}

/// Имена файлов задаёт сервер. Из запроса в путь на диске не попадает ничего.
pub fn file_names(image_id: Uuid) -> (String, String) {
    (format!("{image_id}.jpg"), format!("{image_id}_thumb.jpg"))
}

pub fn products_dir(media_dir: &Path) -> PathBuf {
    media_dir.join("products")
}

/// Сохраняет файл целиком или не сохраняет вовсе: пишет во временный файл, сбрасывает
/// его на диск и только потом переименовывает. Временный файл при ошибке удаляется.
pub async fn store(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;

    tokio::fs::create_dir_all(dir).await?;
    let temp = dir.join(format!(".{name}.tmp"));
    let result = async {
        let mut file = tokio::fs::File::create(&temp).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        tokio::fs::rename(&temp, dir.join(name)).await
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temp).await;
    }
    result
}

/// Удаляет записанные файлы, если загрузка не дошла до конца — в том числе когда запрос
/// оборвался на полпути. Отменить удаление можно вызовом [`StoredFiles::keep`].
pub struct StoredFiles {
    paths: Vec<PathBuf>,
    keep: AtomicBool,
}

impl StoredFiles {
    pub fn new(dir: &Path, names: &[&str]) -> Self {
        Self { paths: names.iter().map(|name| dir.join(name)).collect(), keep: AtomicBool::new(false) }
    }

    pub fn keep(&self) {
        self.keep.store(true, Ordering::SeqCst);
    }
}

impl Drop for StoredFiles {
    fn drop(&mut self) {
        if !self.keep.load(Ordering::SeqCst) {
            for path in &self.paths {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}

/// Удаляет файлы фото. Отсутствие файла — не ошибка.
pub async fn remove(dir: &Path, names: &[&str]) {
    for name in names {
        if let Err(error) = tokio::fs::remove_file(dir.join(name)).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(?error, name, "не удалось удалить файл фото");
        }
    }
}

#[cfg(test)]
mod tests {
    use image::{ImageEncoder, Rgba, RgbaImage};

    use super::*;

    fn png(width: u32, height: u32, pixel: Rgba<u8>) -> Vec<u8> {
        let image = RgbaImage::from_pixel(width, height, pixel);
        let mut out = Vec::new();
        image::codecs::png::PngEncoder::new(&mut out)
            .write_image(image.as_raw(), width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        out
    }

    #[test]
    fn large_image_is_downscaled_and_reencoded_as_jpeg() {
        let result = process_blocking(&png(3200, 1600, Rgba([200, 30, 30, 255]))).unwrap();
        assert_eq!((result.width, result.height), (1600, 800));
        assert_eq!(&result.full[..3], &[0xFF, 0xD8, 0xFF], "JPEG");
        let thumb = image::load_from_memory(&result.thumb).unwrap();
        assert_eq!((thumb.width(), thumb.height()), (480, 240));
    }

    #[test]
    fn small_image_is_not_upscaled_and_transparency_becomes_white() {
        let result = process_blocking(&png(100, 50, Rgba([0, 0, 0, 0]))).unwrap();
        assert_eq!((result.width, result.height), (100, 50));
        let pixel = image::load_from_memory(&result.full).unwrap().to_rgb8().get_pixel(10, 10).0;
        assert!(pixel.iter().all(|channel| *channel > 245), "прозрачный фон стал белым: {pixel:?}");
    }

    #[test]
    fn non_images_and_disguised_files_are_rejected() {
        assert!(matches!(process_blocking(b"<svg onload=alert(1)>"), Err(ImageError::UnsupportedFormat)));
        assert!(matches!(process_blocking(b""), Err(ImageError::UnsupportedFormat)));
        assert!(matches!(process_blocking(b"GIF89a\x01\x00\x01\x00"), Err(ImageError::UnsupportedFormat)));
        // Начало настоящего PNG, дальше мусор.
        let mut broken = png(10, 10, Rgba([1, 2, 3, 255]));
        broken.truncate(30);
        assert!(matches!(process_blocking(&broken), Err(ImageError::Corrupted)));
    }

    /// PNG, заявляющий заданные размеры, с правильной контрольной суммой заголовка.
    /// Пикселей в нём нет: проверка размеров должна сработать раньше, чем до них дойдёт дело.
    pub(crate) fn png_claiming(width: u32, height: u32) -> Vec<u8> {
        let mut file = png(1, 1, Rgba([0, 0, 0, 255]));
        file[16..20].copy_from_slice(&width.to_be_bytes());
        file[20..24].copy_from_slice(&height.to_be_bytes());
        // CRC блока IHDR считается по типу и данным блока (байты 12..29).
        let crc = crc32(&file[12..29]);
        file[29..33].copy_from_slice(&crc.to_be_bytes());
        file
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    #[test]
    fn huge_dimensions_are_rejected_before_decoding() {
        // 60000×60000: распаковка потребовала бы больше 14 ГБ. 8001 px — чуть выше предела стороны.
        for (width, height) in [(60_000, 60_000), (8001, 10), (10, 8001)] {
            let result = process_blocking(&png_claiming(width, height));
            assert!(matches!(result, Err(ImageError::TooLarge)), "{width}×{height}");
        }
        // В пределах сторон, но больше памяти на раскодирование (8000×8000×4 байта > 160 МБ).
        let result = process_blocking(&png_claiming(8000, 8000));
        assert!(matches!(result, Err(ImageError::TooLarge)), "{:?}", result.err());
    }

    #[test]
    fn exif_rotation_is_applied_and_metadata_is_dropped() {
        // JPEG 40×20 с блоком EXIF: «повернуть на 90°» и строка, изображающая координаты.
        let image = RgbImage::from_pixel(40, 20, image::Rgb([10, 200, 10]));
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 90).encode_image(&image).unwrap();
        let mut tiff = b"MM\0\x2a\0\0\0\x08".to_vec();
        tiff.extend_from_slice(&[0, 1]); // одна запись
        tiff.extend_from_slice(&[0x01, 0x12, 0, 3, 0, 0, 0, 1, 0, 6, 0, 0]); // Orientation = 6
        tiff.extend_from_slice(&[0, 0, 0, 0]); // следующей таблицы нет
        tiff.extend_from_slice(b"GPS 50.0N 36.7E");
        let mut app1 = b"Exif\0\0".to_vec();
        app1.extend_from_slice(&tiff);
        let length = u16::try_from(app1.len() + 2).unwrap();
        let mut with_exif = vec![0xFF, 0xD8, 0xFF, 0xE1];
        with_exif.extend_from_slice(&length.to_be_bytes());
        with_exif.extend_from_slice(&app1);
        with_exif.extend_from_slice(&jpeg[2..]);

        let result = process_blocking(&with_exif).unwrap();
        assert_eq!((result.width, result.height), (20, 40), "снимок повёрнут");
        let contains = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
        for output in [&result.full, &result.thumb] {
            assert!(!contains(output, b"Exif"), "метаданных нет");
            assert!(!contains(output, b"GPS"), "координат нет");
        }
    }

    #[test]
    fn file_names_are_server_generated() {
        let id = Uuid::nil();
        let (full, thumb) = file_names(id);
        assert_eq!(full, "00000000-0000-0000-0000-000000000000.jpg");
        assert_eq!(thumb, "00000000-0000-0000-0000-000000000000_thumb.jpg");
    }
}
