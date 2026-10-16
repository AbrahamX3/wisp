//! Fetching files over HTTPS and unpacking the zip archives PHP and nginx ship.

use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("could not reach {url}: {message}")]
    Http { url: String, message: String },

    #[error("{0}")]
    Io(#[from] std::io::Error),

    #[error("the archive could not be read: {0}")]
    Zip(#[from] zip::result::ZipError),
}

pub type Result<T> = std::result::Result<T, DownloadError>;

/// GET a small text document (a directory listing).
pub fn get_text(url: &str) -> Result<String> {
    let mut response = ureq::get(url)
        .call()
        .map_err(|error| http_error(url, error))?;
    response
        .body_mut()
        .read_to_string()
        .map_err(|error| http_error(url, error))
}

/// A generous ceiling for runtime archives; PHP builds are around 33 MB.
const MAX_BODY: u64 = 1024 * 1024 * 1024;

/// GET a file and write it to `dest`, reporting how far along it is.
///
/// `progress` is called with a fraction between 0 and 1 as bytes arrive, and
/// only when the server said how large the file is; archives served without a
/// length simply report nothing.
pub fn download_to_reporting(
    url: &str,
    dest: &Path,
    progress: &dyn Fn(f32),
) -> Result<()> {
    let mut response = ureq::get(url)
        .call()
        .map_err(|error| http_error(url, error))?;
    let expected = response
        .headers()
        .get("content-length")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|length| *length > 0);

    let mut reader = response.body_mut().as_reader();
    let mut bytes: Vec<u8> = Vec::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut reader, &mut buffer)?;
        if read == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() as u64 > MAX_BODY {
            return Err(DownloadError::Io(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "the download is larger than Wisp will accept",
            )));
        }
        if let Some(expected) = expected {
            progress((bytes.len() as f32 / expected as f32).min(1.0));
        }
    }
    progress(1.0);

    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, bytes)?;
    Ok(())
}

/// GET a file and write it to `dest`.
pub fn download_to(url: &str, dest: &Path) -> Result<()> {
    let mut response = ureq::get(url)
        .call()
        .map_err(|error| http_error(url, error))?;
    let bytes = response
        .body_mut()
        .with_config()
        .limit(MAX_BODY)
        .read_to_vec()
        .map_err(|error| http_error(url, error))?;
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(dest, bytes)?;
    Ok(())
}

/// Unpack a zip into `dest`, dropping the first `strip` path components (nginx
/// archives wrap everything in a `nginx-<version>/` folder; PHP's do not).
pub fn extract_zip(archive: &Path, dest: &Path, strip: usize) -> Result<()> {
    let file = std::fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)?;

    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let name = entry.name()?.replace('\\', "/");
        let Some(relative) = strip_components(&name, strip) else {
            continue;
        };

        let mut out = dest.to_path_buf();
        for component in relative.split('/') {
            // Skip empty segments, `.`, and anything that would escape `dest`.
            if component.is_empty() || component == "." || component == ".." {
                continue;
            }
            out.push(component);
        }
        if out == dest {
            continue;
        }

        if entry.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut target = std::fs::File::create(&out)?;
        std::io::copy(&mut entry, &mut target)?;
    }
    Ok(())
}

/// Remove the leading `strip` components, returning the remainder.
fn strip_components(name: &str, strip: usize) -> Option<String> {
    let mut parts = name.split('/').filter(|part| !part.is_empty());
    for _ in 0..strip {
        parts.next()?;
    }
    Some(parts.collect::<Vec<_>>().join("/"))
}

fn http_error(url: &str, error: ureq::Error) -> DownloadError {
    DownloadError::Http {
        url: url.to_string(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_zip, strip_components};

    #[test]
    fn keeps_everything_when_nothing_is_stripped() {
        assert_eq!(
            strip_components("php.exe", 0).as_deref(),
            Some("php.exe")
        );
        assert_eq!(
            strip_components("ext/php_curl.dll", 0).as_deref(),
            Some("ext/php_curl.dll")
        );
    }

    #[test]
    fn drops_the_wrapping_folder_for_nginx() {
        assert_eq!(
            strip_components("nginx-1.27.0/nginx.exe", 1).as_deref(),
            Some("nginx.exe")
        );
        assert_eq!(
            strip_components("nginx-1.27.0/conf/nginx.conf", 1).as_deref(),
            Some("conf/nginx.conf")
        );
    }

    #[test]
    fn a_bare_wrapper_folder_leaves_nothing() {
        assert_eq!(strip_components("nginx-1.27.0/", 1).as_deref(), Some(""));
    }

    #[test]
    fn extracts_an_archive_and_strips_the_wrapper_folder() {
        use std::io::Write as _;

        let mut buffer = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut buffer);
            let options = zip::write::SimpleFileOptions::default();
            writer.add_directory("nginx-1.27.0/", options).unwrap();
            writer.start_file("nginx-1.27.0/nginx.exe", options).unwrap();
            writer.write_all(b"binary").unwrap();
            writer
                .start_file("nginx-1.27.0/conf/nginx.conf", options)
                .unwrap();
            writer.write_all(b"events {}").unwrap();
            writer.finish().unwrap();
        }

        let work = std::env::temp_dir().join("wisp-extract-test");
        let _ = std::fs::remove_dir_all(&work);
        std::fs::create_dir_all(&work).unwrap();
        let archive = work.join("archive.zip");
        std::fs::write(&archive, buffer.into_inner()).unwrap();

        let out = work.join("out");
        extract_zip(&archive, &out, 1).unwrap();

        assert!(out.join("nginx.exe").is_file());
        assert!(out.join("conf/nginx.conf").is_file());
        assert!(!out.join("nginx-1.27.0").exists(), "wrapper folder is gone");

        let _ = std::fs::remove_dir_all(&work);
    }
}
