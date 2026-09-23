//! Use macOS ImageIO (through `sips`) for photos the small portable decoder
//! cannot read. Only the resulting JPEG is eligible for an inline photo.

#[cfg(target_os = "macos")]
mod imp {
    use std::fs;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    use wacore::time::Instant;

    const CONVERSION_ERROR: &str =
        "Esta imagem não pôde ser convertida para JPEG. Tente enviá-la como Documento.";

    pub(super) fn to_jpeg(bytes: &[u8], source_extension: &str) -> Result<Vec<u8>, String> {
        if !matches!(source_extension, "heic" | "avif" | "bmp" | "tiff") {
            return Err(CONVERSION_ERROR.to_string());
        }
        let directory = tempfile::Builder::new()
            .prefix("oxidezap-image-")
            .tempdir()
            .map_err(|_| CONVERSION_ERROR.to_string())?;
        let source = directory.path().join(format!("source.{source_extension}"));
        let output = directory.path().join("converted.jpg");
        fs::write(&source, bytes).map_err(|_| CONVERSION_ERROR.to_string())?;

        let mut child = Command::new("/usr/bin/sips")
            .arg("-s")
            .arg("format")
            .arg("jpeg")
            .arg(&source)
            .arg("--out")
            .arg(&output)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| CONVERSION_ERROR.to_string())?;

        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break,
                Ok(Some(_)) => return Err(CONVERSION_ERROR.to_string()),
                Err(_) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CONVERSION_ERROR.to_string());
                }
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(
                        "A conversão da imagem demorou demais. Tente enviar como Documento."
                            .to_string(),
                    );
                }
                Ok(None) => thread::sleep(Duration::from_millis(100)),
            }
        }

        fs::read(&output).map_err(|_| CONVERSION_ERROR.to_string())
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub(super) fn to_jpeg(_bytes: &[u8], _source_extension: &str) -> Result<Vec<u8>, String> {
        Err("Esta imagem precisa ser convertida antes do envio. Envie como Documento.".to_string())
    }
}

pub(super) fn to_jpeg(bytes: &[u8], source_extension: &str) -> Result<Vec<u8>, String> {
    imp::to_jpeg(bytes, source_extension)
}
