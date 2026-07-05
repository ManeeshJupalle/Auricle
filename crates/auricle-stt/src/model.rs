use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};

use auricle_core::{Error, Result};
use sha2::{Digest, Sha256};

/// A downloadable ggml whisper model.
#[derive(Debug, Clone, Copy)]
pub struct ModelInfo {
    pub name: &'static str,
    pub file_name: &'static str,
    pub url: &'static str,
    /// Expected SHA-256 of the model file (lowercase hex).
    pub sha256: &'static str,
    pub size_bytes: u64,
}

/// Known models. Checksums and sizes captured from the Hugging Face LFS
/// metadata for `ggerganov/whisper.cpp` on 2026-07-04 via
/// `/api/models/ggerganov/whisper.cpp/tree/main`.
const MODELS: &[ModelInfo] = &[
    ModelInfo {
        name: "base.en",
        file_name: "ggml-base.en.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.en.bin",
        sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
        size_bytes: 147_964_211,
    },
    ModelInfo {
        name: "small.en",
        file_name: "ggml-small.en.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.en.bin",
        sha256: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
        size_bytes: 487_614_201,
    },
];

/// Look up a model by name ("base.en", "small.en"). Returns the list of
/// known names in the error message on a miss.
pub fn available_models() -> &'static [ModelInfo] {
    MODELS
}

/// Default model directory: `%LOCALAPPDATA%\auricle\models`.
pub fn default_data_dir() -> Result<PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")
        .ok_or_else(|| Error::Model("LOCALAPPDATA is not set".to_string()))?;
    Ok(PathBuf::from(base).join("auricle").join("models"))
}

/// Compute the SHA-256 of a file as lowercase hex.
pub fn file_sha256(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex(&hasher.finalize()))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Verify an existing file against the model's expected checksum.
pub(crate) fn verify_model_file(info: &ModelInfo, path: &Path) -> Result<()> {
    let actual = file_sha256(path)?;
    if actual != info.sha256 {
        return Err(Error::Model(format!(
            "checksum mismatch for {}: expected {}, got {actual}. \
             Delete the file and re-run to re-download.",
            path.display(),
            info.sha256
        )));
    }
    Ok(())
}

/// Ensure the model file exists in `dir` with a valid checksum, downloading
/// it if absent. Returns the model path. Progress is reported on stderr.
pub async fn ensure_model(info: &'static ModelInfo, dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(info.file_name);

    if path.exists() {
        let p = path.clone();
        tokio::task::spawn_blocking(move || verify_model_file(info, &p))
            .await
            .map_err(|e| Error::Model(format!("checksum task failed: {e}")))??;
        return Ok(path);
    }

    eprintln!(
        "downloading {} ({:.0} MB) from {} ...",
        info.file_name,
        info.size_bytes as f64 / 1_048_576.0,
        info.url
    );
    let part = dir.join(format!("{}.part", info.file_name));
    let result = download_verified(info, &part).await;
    if result.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    result?;
    std::fs::rename(&part, &path)?;
    eprintln!("model saved to {}", path.display());
    Ok(path)
}

async fn download_verified(info: &ModelInfo, part: &Path) -> Result<()> {
    let mut resp = reqwest::get(info.url)
        .await
        .map_err(|e| Error::Model(format!("download failed: {e}")))?
        .error_for_status()
        .map_err(|e| Error::Model(format!("download failed: {e}")))?;

    let mut file = std::fs::File::create(part)?;
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    let mut next_report: u64 = 0;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| Error::Model(format!("download interrupted: {e}")))?
    {
        hasher.update(&chunk);
        file.write_all(&chunk)?;
        received += chunk.len() as u64;
        if received >= next_report {
            eprintln!(
                "  {:>3.0}% ({:.0}/{:.0} MB)",
                100.0 * received as f64 / info.size_bytes as f64,
                received as f64 / 1_048_576.0,
                info.size_bytes as f64 / 1_048_576.0
            );
            next_report = received + info.size_bytes / 10;
        }
    }
    file.flush()?;
    drop(file);

    let actual = hex(&hasher.finalize());
    if actual != info.sha256 {
        return Err(Error::Model(format!(
            "downloaded {} failed checksum verification: expected {}, got {actual}",
            info.file_name, info.sha256
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NIST test vector: sha256("abc").
    #[test]
    fn sha256_of_known_bytes() {
        let dir = std::env::temp_dir().join("auricle-stt-test-sha");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("abc.txt");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(
            file_sha256(&p).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn checksum_mismatch_is_a_clean_error() {
        let dir = std::env::temp_dir().join("auricle-stt-test-badmodel");
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("ggml-bogus.bin");
        std::fs::write(&p, b"definitely not a whisper model").unwrap();

        let info = ModelInfo {
            name: "bogus",
            file_name: "ggml-bogus.bin",
            url: "https://invalid.example/x.bin",
            sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
            size_bytes: 30,
        };
        let err = verify_model_file(&info, &p).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("checksum mismatch"), "got: {msg}");
        assert!(msg.contains("ggml-bogus.bin"), "got: {msg}");
    }

    #[test]
    fn known_models_are_listed() {
        let names: Vec<_> = available_models().iter().map(|m| m.name).collect();
        assert_eq!(names, vec!["base.en", "small.en"]);
    }
}
