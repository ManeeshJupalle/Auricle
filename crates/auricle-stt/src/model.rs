use std::fs::{File, OpenOptions};
use std::io::{Read, Write as _};
use std::path::{Path, PathBuf};
use std::time::Duration;

use auricle_core::{Error, Result};

use crate::http_error;
use reqwest::header::{CONTENT_RANGE, RANGE};
use reqwest::{Client, Response, StatusCode};
use sha2::{Digest, Sha256};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Per read, never total: a healthy download runs for many minutes, a stalled
/// one should fail and leave its `.part` for the next attempt to resume.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

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
/// metadata for `ggerganov/whisper.cpp` on 2026-07-04 (multilingual rows on
/// 2026-11-06) via `/api/models/ggerganov/whisper.cpp/tree/main`.
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
    ModelInfo {
        name: "base",
        file_name: "ggml-base.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-base.bin",
        sha256: "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
        size_bytes: 147_951_465,
    },
    ModelInfo {
        name: "small",
        file_name: "ggml-small.bin",
        url: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-small.bin",
        sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
        size_bytes: 487_601_967,
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
    download_verified(&http_client(READ_TIMEOUT)?, info, &part).await?;
    std::fs::rename(&part, &path)?;
    eprintln!("model saved to {}", path.display());
    Ok(path)
}

fn http_client(read_timeout: Duration) -> Result<Client> {
    Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(read_timeout)
        .build()
        .map_err(|e| Error::Model(format!("building http client: {e}")))
}

/// Bring `part` up to the complete, verified model. An interrupted transfer
/// leaves `part` for the next run to resume; a failed checksum removes it,
/// since resuming from those bytes could never succeed.
async fn download_verified(client: &Client, info: &ModelInfo, part: &Path) -> Result<()> {
    let have = std::fs::metadata(part).map_or(0, |m| m.len());
    // Full size is a run killed before its rename, so only the checksum is
    // left to do. Larger cannot be a prefix of this file.
    let have = if have > info.size_bytes { 0 } else { have };
    if have < info.size_bytes {
        fetch_into_part(client, info, part, have).await?;
    }

    // Hashes the whole file, so bytes from an earlier run are verified too.
    let path = part.to_path_buf();
    let actual = tokio::task::spawn_blocking(move || file_sha256(&path))
        .await
        .map_err(|e| Error::Model(format!("checksum task failed: {e}")))??;
    if actual != info.sha256 {
        let _ = std::fs::remove_file(part);
        return Err(Error::Model(format!(
            "downloaded {} failed checksum verification: expected {}, got {actual}",
            info.file_name, info.sha256
        )));
    }
    Ok(())
}

async fn request_from(client: &Client, info: &ModelInfo, offset: u64) -> Result<Response> {
    let mut request = client.get(info.url);
    if offset > 0 {
        request = request.header(RANGE, format!("bytes={offset}-"));
    }
    request
        .send()
        .await
        .map_err(|e| Error::Model(format!("download failed: {}", http_error(&e))))
}

/// First byte of a `Content-Range: bytes 80-199/200` response.
fn content_range_start(resp: &Response) -> Option<u64> {
    let value = resp.headers().get(CONTENT_RANGE)?.to_str().ok()?;
    let (start, _) = value.strip_prefix("bytes ")?.split_once('-')?;
    start.parse().ok()
}

/// Append to `part` from byte `have`, or rewrite it when the server can't
/// serve that offset.
async fn fetch_into_part(client: &Client, info: &ModelInfo, part: &Path, have: u64) -> Result<()> {
    let mut resp = request_from(client, info, have).await?;
    let resumed = have > 0
        && resp.status() == StatusCode::PARTIAL_CONTENT
        && content_range_start(&resp) == Some(have);
    // A 200 already carries the whole file (Range ignored). Anything else,
    // such as 416 or a 206 for another offset, can't extend `part`.
    if have > 0 && !resumed && resp.status() != StatusCode::OK {
        resp = request_from(client, info, 0).await?;
    }
    let mut resp = resp
        .error_for_status()
        .map_err(|e| Error::Model(format!("download failed: {}", http_error(&e))))?;

    let mut file = if resumed {
        eprintln!("  resuming from {:.0} MB", have as f64 / 1_048_576.0);
        OpenOptions::new().append(true).open(part)?
    } else {
        File::create(part)?
    };
    let mut received: u64 = if resumed { have } else { 0 };
    let mut next_report: u64 = 0;
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| Error::Model(format!("download interrupted: {}", http_error(&e))))?
    {
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn model_bytes() -> Vec<u8> {
        (0..200u8).collect()
    }

    /// `ensure_model` takes a `&'static ModelInfo`; leaking is fine in a test.
    fn test_model(url: String) -> &'static ModelInfo {
        let bytes = model_bytes();
        Box::leak(Box::new(ModelInfo {
            name: "test",
            file_name: "ggml-test.bin",
            url: Box::leak(url.into_boxed_str()),
            sha256: Box::leak(hex(&Sha256::digest(&bytes)).into_boxed_str()),
            size_bytes: bytes.len() as u64,
        }))
    }

    /// A fresh directory, with `.part` holding `partial` when given.
    fn model_dir(test: &str, partial: Option<&[u8]>) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("auricle-stt-model-{test}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(bytes) = partial {
            std::fs::write(dir.join("ggml-test.bin.part"), bytes).unwrap();
        }
        dir
    }

    #[tokio::test]
    async fn interrupted_download_resumes_from_the_part_file() {
        let bytes = model_bytes();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/m.bin"))
            .and(header("range", "bytes=80-"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("content-range", "bytes 80-199/200")
                    .set_body_bytes(&bytes[80..]),
            )
            .expect(1)
            .mount(&server)
            .await;
        let dir = model_dir("resume", Some(&bytes[..80]));

        let info = test_model(format!("{}/m.bin", server.uri()));
        let saved = ensure_model(info, &dir).await.unwrap();

        assert_eq!(std::fs::read(&saved).unwrap(), bytes);
        assert!(
            !dir.join("ggml-test.bin.part").exists(),
            "renamed into place"
        );
    }

    #[tokio::test]
    async fn server_ignoring_range_rewrites_the_part_file() {
        let bytes = model_bytes();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/m.bin"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .expect(1)
            .mount(&server)
            .await;
        // Stale bytes that would fail the checksum if they were appended to.
        let dir = model_dir("ignored", Some(&[0xff; 80]));

        let info = test_model(format!("{}/m.bin", server.uri()));
        let saved = ensure_model(info, &dir).await.unwrap();

        assert_eq!(std::fs::read(&saved).unwrap(), bytes);
    }

    #[tokio::test]
    async fn refused_range_falls_back_to_a_full_download() {
        let bytes = model_bytes();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(header_exists("range"))
            .respond_with(ResponseTemplate::new(416))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bytes.clone()))
            .expect(1)
            .mount(&server)
            .await;
        let dir = model_dir("refused", Some(&bytes[..80]));

        let info = test_model(format!("{}/m.bin", server.uri()));
        let saved = ensure_model(info, &dir).await.unwrap();

        assert_eq!(std::fs::read(&saved).unwrap(), bytes);
    }

    #[tokio::test]
    async fn complete_part_file_only_needs_verifying() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;
        let dir = model_dir("complete", Some(&model_bytes()));

        let info = test_model(format!("{}/m.bin", server.uri()));
        let saved = ensure_model(info, &dir).await.unwrap();

        assert_eq!(std::fs::read(&saved).unwrap(), model_bytes());
    }

    #[tokio::test]
    async fn resumed_bytes_that_fail_the_checksum_are_discarded() {
        let bytes = model_bytes();
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(206)
                    .insert_header("content-range", "bytes 80-199/200")
                    .set_body_bytes(&bytes[80..]),
            )
            .mount(&server)
            .await;
        let dir = model_dir("corrupt", Some(&[0xff; 80]));

        let info = test_model(format!("{}/m.bin", server.uri()));
        let err = ensure_model(info, &dir).await.unwrap_err().to_string();

        assert!(err.contains("failed checksum verification"), "{err}");
        assert!(!dir.join("ggml-test.bin").exists(), "never renamed");
        assert!(
            !dir.join("ggml-test.bin.part").exists(),
            "the next attempt must start clean"
        );
    }

    #[tokio::test]
    async fn download_stalled_mid_body_fails_and_keeps_its_progress() {
        // Raw socket: wiremock can delay a response but not stall a body
        // halfway through. `hold` keeps the connection open and silent.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/m.bin", listener.local_addr().unwrap());
        let (hold, wait) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let _ = socket.read(&mut [0u8; 2048]);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 200\r\n\r\n")
                .unwrap();
            socket.write_all(&model_bytes()[..80]).unwrap();
            let _ = wait.recv();
        });
        let dir = model_dir("stall", None);
        let part = dir.join("ggml-test.bin.part");
        let client = http_client(Duration::from_secs(1)).unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(10),
            download_verified(&client, test_model(url), &part),
        )
        .await
        .expect("a stalled download must not hang");

        let err = result.unwrap_err().to_string();
        assert!(err.contains("download interrupted"), "{err}");
        assert!(err.contains("timed out"), "{err}");
        assert_eq!(
            std::fs::metadata(&part).unwrap().len(),
            80,
            "kept to resume"
        );
        drop(hold);
    }

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
        assert_eq!(names, vec!["base.en", "small.en", "base", "small"]);
    }
}
