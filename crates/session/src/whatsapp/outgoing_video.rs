//! Convert an unsupported movie before it is described as an inline video.
//!
//! A MOV can be uploaded and its message can receive a server ack while the
//! recipient never gets a playable video. On macOS, AVFoundation first tries
//! a passthrough MP4 export for compatible H.264/AAC recordings, then a
//! re-encode for codecs that cannot be carried in that container. The
//! temporary source and output live only for this preparation.

#[cfg(target_os = "macos")]
mod imp {
    use std::fs;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    use wacore::time::Instant;

    const CONVERSION_ERROR: &str =
        "Este vídeo não pôde ser convertido para MP4. Tente enviá-lo como Documento.";

    pub(super) fn export_mp4(
        bytes: &[u8],
        source_extension: &str,
        preset: &str,
    ) -> Result<Vec<u8>, String> {
        if !matches!(source_extension, "mov" | "mp4" | "3gp" | "avi") {
            return Err(CONVERSION_ERROR.to_string());
        }
        let directory = tempfile::Builder::new()
            .prefix("oxidezap-video-")
            .tempdir()
            .map_err(|_| CONVERSION_ERROR.to_string())?;
        let source = directory.path().join(format!("source.{source_extension}"));
        let output = directory.path().join("converted.mp4");
        fs::write(&source, bytes).map_err(|_| CONVERSION_ERROR.to_string())?;

        let mut child = Command::new("/usr/bin/avconvert")
            .arg("--source")
            .arg(&source)
            .arg("--preset")
            .arg(preset)
            .arg("--output")
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
                        "A conversão do vídeo demorou demais. Tente enviar como Documento."
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
    pub(super) fn export_mp4(
        _bytes: &[u8],
        _source_extension: &str,
        _preset: &str,
    ) -> Result<Vec<u8>, String> {
        Err(
            "Este vídeo precisa ser convertido para MP4 antes do envio. Envie como Documento."
                .to_string(),
        )
    }
}

pub(super) fn to_mp4(bytes: &[u8], source_extension: &str) -> Result<Vec<u8>, String> {
    let passthrough = imp::export_mp4(bytes, source_extension, "PresetPassthrough");
    if let Ok(ref converted) = passthrough
        && super::outgoing::compatible_mp4_shape(converted).is_some()
    {
        return passthrough;
    }
    imp::export_mp4(bytes, source_extension, "PresetHighestQuality")
}
