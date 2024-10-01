use std::io::{BufRead, Read, Seek, Write};

use aes_gcm::{
    aead::{AeadCore, AeadMutInPlace, OsRng},
    Aes256Gcm, Key, KeyInit, Nonce,
};
use image::DynamicImage;
use revolt_config::{config, report_internal_error, FilesS3};
use revolt_result::{create_error, Result};

use aws_sdk_s3::{
    config::{Credentials, Region},
    Client, Config,
};

use base64::prelude::*;
use tempfile::NamedTempFile;

/// Size of the authentication tag in the buffer
pub const AUTHENTICATION_TAG_SIZE_BYTES: usize = 16;

/// Create an S3 client
pub fn create_client(s3_config: FilesS3) -> Client {
    let provider_name = "my-creds";
    let creds = Credentials::new(
        s3_config.access_key_id,
        s3_config.secret_access_key,
        None,
        None,
        provider_name,
    );

    let config = Config::builder()
        .region(Region::new(s3_config.region))
        .endpoint_url(s3_config.endpoint)
        .credentials_provider(creds)
        .build();

    Client::from_conf(config)
}

/// Create an AES-256-GCM cipher
pub fn create_cipher(key: &str) -> Aes256Gcm {
    let key = &BASE64_STANDARD.decode(key).expect("valid base64 string")[..];
    let key: &Key<Aes256Gcm> = key.into();
    Aes256Gcm::new(key)
}

/// Fetch a file from S3 (and decrypt it)
pub async fn fetch_from_s3(bucket_id: &str, path: &str, nonce: &str) -> Result<Vec<u8>> {
    let config = config().await;
    let client = create_client(config.files.s3);

    // Send a request for the file
    let mut obj =
        report_internal_error!(client.get_object().bucket(bucket_id).key(path).send().await)?;

    // Read the file from remote
    let mut buf = vec![];
    while let Some(bytes) = obj.body.next().await {
        let data = report_internal_error!(bytes)?;
        report_internal_error!(buf.write_all(&data))?;
        // is there a more efficient way to do this?
        // we just want the Vec<u8>
    }

    // File is not encrypted
    if nonce.is_empty() {
        return Ok(buf);
    }

    // Recover nonce as bytes
    let nonce = &BASE64_STANDARD.decode(nonce).unwrap()[..];
    let nonce: &Nonce<typenum::consts::U12> = nonce.into();

    // Decrypt the file
    create_cipher(&config.files.encryption_key)
        .decrypt_in_place(nonce, b"", &mut buf)
        .map_err(|_| create_error!(InternalError))?;

    Ok(buf)
}

/// Encrypt and upload a file to S3 (returning its nonce/IV)
pub async fn upload_to_s3(bucket_id: &str, path: &str, buf: &[u8]) -> Result<String> {
    let config = config().await;
    let client = create_client(config.files.s3);

    // Generate a nonce
    let nonce = Aes256Gcm::generate_nonce(&mut OsRng);

    // Extend the buffer for in-place encryption
    let mut buf = [buf, &[0; AUTHENTICATION_TAG_SIZE_BYTES]].concat();

    // Encrypt the file in place
    create_cipher(&config.files.encryption_key)
        .encrypt_in_place(&nonce, b"", &mut buf)
        .map_err(|_| create_error!(InternalError))?;

    // Upload the file to remote
    report_internal_error!(
        client
            .put_object()
            .bucket(bucket_id)
            .key(path)
            .body(buf.into())
            .send()
            .await
    )?;

    Ok(BASE64_STANDARD.encode(nonce))
}

/// Determine size of image at temp file
pub fn image_size(f: &NamedTempFile) -> Option<(usize, usize)> {
    if let Ok(size) = imagesize::size(f.path())
        .inspect_err(|err| tracing::error!("Failed to generate image size! {err:?}"))
    {
        Some((size.width, size.height))
    } else {
        None
    }
}

/// Determine size of video at temp file
pub fn video_size(f: &NamedTempFile) -> Option<(i64, i64)> {
    if let Ok(data) = ffprobe::ffprobe(f.path())
        .inspect_err(|err| tracing::error!("Failed to ffprobe file! {err:?}"))
    {
        // Use first valid stream
        for stream in data.streams {
            if let (Some(w), Some(h)) = (stream.width, stream.height) {
                return Some((w, h));
            }
        }

        None
    } else {
        None
    }
}

/// Decode image from reader
pub fn decode_image<R: Read + BufRead + Seek>(
    reader: &mut R,
    is_jxl: bool,
) -> Result<DynamicImage> {
    if is_jxl {
        Err(create_error!(LabelMe))
    } else {
        // Check if we can read using image-rs crate
        report_internal_error!(report_internal_error!(
            image::ImageReader::new(reader).with_guessed_format()
        )?
        .decode())
    }
}

/// Check whether given reader has a valid image
pub fn is_valid_image<R: Read + BufRead + Seek>(reader: &mut R, is_jxl: bool) -> bool {
    if is_jxl {
        // Check if we can read using jxl-oxide crate
        jxl_oxide::JxlImage::builder()
            .read(reader)
            .inspect_err(|err| tracing::error!("Failed to read JXL! {err:?}"))
            .is_ok()
    } else {
        !matches!(
            // Check if we can read using image-rs crate
            image::ImageReader::new(reader)
                .with_guessed_format()
                .inspect_err(|err| tracing::error!("Failed to read image! {err:?}"))
                .map(|f| f.decode()),
            Err(_) | Ok(Err(_))
        )
    }
}

/// Create thumbnail from given image
pub async fn create_thumbnail(image: DynamicImage, tag: &str) -> Vec<u8> {
    // Load configuration
    let config = config().await;
    let [w, h] = config.files.preview.get(tag).unwrap();

    // Create thumbnail
    //.resize(width as u32, height as u32, image::imageops::FilterType::Gaussian)
    // resize is about 2.5x slower,
    // thumbnail doesn't have terrible quality
    // so we use thumbnail
    let image = image.thumbnail(image.width().min(*w as u32), image.height().min(*h as u32));

    // Encode it into WEBP
    let encoder = webp::Encoder::from_image(&image).expect("Could not create encoder.");
    if config.files.webp_quality != 100.0 {
        encoder.encode(config.files.webp_quality).to_vec()
    } else {
        encoder.encode_lossless().to_vec()
    }
}
