//! The list of PHP and nginx builds Wisp can install.
//!
//! Both projects publish plain Apache-style directory listings, so the catalog
//! is scraped from those indexes rather than a JSON API. The lists are cached in
//! memory by the UI and refreshed on demand.

use super::download::{self, Result};

pub const PHP_RELEASES: &str = "https://windows.php.net/downloads/releases/";
pub const PHP_ARCHIVES: &str = "https://windows.php.net/downloads/releases/archives/";
pub const NGINX_DOWNLOAD: &str = "https://nginx.org/download/";

/// The oldest PHP line offered, as the user asked for 7.4 and up.
const PHP_MIN: (u32, u32) = (7, 4);

/// The oldest nginx line offered; older ones are history, not building blocks.
const NGINX_MIN_MINOR: u32 = 20;

/// A downloadable build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: String,
    pub url: String,
    /// PHP only: the Visual C++ runtime the build links against (16 = VS 2019,
    /// 17 = VS 2022). `None` for nginx.
    pub vc: Option<u16>,
}

#[derive(Clone, Debug, Default)]
pub struct Catalog {
    /// Newest first.
    pub php: Vec<Release>,
    /// Newest first.
    pub nginx: Vec<Release>,
}

impl Catalog {
    /// Fetch and parse both indexes. Runs on the background executor.
    pub fn fetch() -> Result<Self> {
        let releases = download::fetch_text(PHP_RELEASES)?;
        let archives = download::fetch_text(PHP_ARCHIVES)?;
        let nginx = download::fetch_text(NGINX_DOWNLOAD)?;
        Ok(Self {
            php: parse_php(&[(&releases, PHP_RELEASES), (&archives, PHP_ARCHIVES)]),
            nginx: parse_nginx(&nginx),
        })
    }
}

/// Parse one or more PHP listing pages. Pages earlier in the slice win when a
/// version appears in both, so the canonical `releases/` URL is preferred over
/// the archive copy.
pub fn parse_php(pages: &[(&str, &str)]) -> Vec<Release> {
    let mut seen: Vec<String> = Vec::new();
    let mut out: Vec<Release> = Vec::new();
    for (html, base) in pages {
        for href in hrefs(html) {
            if let Some(release) = php_release(href, base) {
                if seen.contains(&release.version) {
                    continue;
                }
                seen.push(release.version.clone());
                out.push(release);
            }
        }
    }
    sort_desc(&mut out);
    out
}

/// Parse the nginx download index.
pub fn parse_nginx(html: &str) -> Vec<Release> {
    let mut out: Vec<Release> = hrefs(html)
        .into_iter()
        .filter_map(nginx_release)
        .collect();
    sort_desc(&mut out);
    out.truncate(24);
    out
}

fn php_release(href: &str, base: &str) -> Option<Release> {
    let file = href.rsplit('/').next()?;
    let stem = file.strip_prefix("php-")?;
    if !file.ends_with("-x64.zip") || !file.contains("-nts-Win32-vs") {
        return None;
    }
    let version = stem.split('-').next()?;
    if !is_stable(version) || !at_least(version, PHP_MIN) {
        return None;
    }
    let vc = file
        .split("-vs")
        .nth(1)?
        .split('-')
        .next()?
        .parse::<u16>()
        .ok()?;
    Some(Release {
        version: version.to_string(),
        url: format!("{base}{file}"),
        vc: Some(vc),
    })
}

fn nginx_release(href: &str) -> Option<Release> {
    let file = href.rsplit('/').next()?;
    let version = file.strip_prefix("nginx-")?.strip_suffix(".zip")?;
    if !is_stable(version) {
        return None;
    }
    let key = version_key(version);
    if key.first().copied() != Some(1) || key.get(1).copied().unwrap_or(0) < NGINX_MIN_MINOR {
        return None;
    }
    Some(Release {
        version: version.to_string(),
        url: format!("{NGINX_DOWNLOAD}{file}"),
        vc: None,
    })
}

/// Every `href="..."` value in a listing, in document order.
fn hrefs(html: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = html;
    while let Some(start) = rest.find("href=\"") {
        rest = &rest[start + 6..];
        if let Some(end) = rest.find('"') {
            out.push(&rest[..end]);
            rest = &rest[end + 1..];
        } else {
            break;
        }
    }
    out
}

/// A plain `x.y.z` version, with no release-candidate suffix.
fn is_stable(version: &str) -> bool {
    let mut parts = version.split('.');
    let (Some(a), Some(b), Some(c), None) = (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    [a, b, c]
        .iter()
        .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

fn version_key(version: &str) -> Vec<u32> {
    version
        .split('.')
        .filter_map(|part| part.parse::<u32>().ok())
        .collect()
}

fn at_least(version: &str, min: (u32, u32)) -> bool {
    let key = version_key(version);
    let major = key.first().copied().unwrap_or(0);
    let minor = key.get(1).copied().unwrap_or(0);
    (major, minor) >= min
}

fn sort_desc(releases: &mut [Release]) {
    releases.sort_by_key(|release| std::cmp::Reverse(version_key(&release.version)));
}

#[cfg(test)]
mod tests {
    use super::{parse_nginx, parse_php, Release};

    const PHP: &str = r#"
        <a href="php-8.4.26-nts-Win32-vs17-x64.zip">php-8.4.26-nts-Win32-vs17-x64.zip</a>
        <a href="php-8.4.26-nts-Win32-vs17-x64.zip.cdx.json">meta</a>
        <a href="php-8.3.35-nts-Win32-vs16-x64.zip">php-8.3.35-nts-Win32-vs16-x64.zip</a>
        <a href="php-8.5.0RC1-nts-Win32-vs17-x64.zip">rc</a>
        <a href="php-7.3.33-nts-Win32-vs16-x64.zip">too old</a>
        <a href="php-8.4.26-ts-Win32-vs17-x64.zip">thread safe</a>
        <a href="php-8.4.26-nts-Win32-vs17-x86.zip">32-bit</a>
    "#;

    const NGINX: &str = r#"
        <a href="nginx-1.27.0.zip">nginx-1.27.0.zip</a>
        <a href="nginx-1.27.0.zip.asc">sig</a>
        <a href="nginx-1.26.2.zip">nginx-1.26.2.zip</a>
        <a href="nginx-0.7.52.zip">ancient</a>
        <a href="nginx-1.27.0/pgp/">dir</a>
    "#;

    #[test]
    fn php_keeps_only_nts_x64_stable_builds_from_74() {
        let releases = parse_php(&[(PHP, "https://example.test/")]);
        let versions: Vec<&str> = releases.iter().map(|r| r.version.as_str()).collect();
        assert_eq!(versions, vec!["8.4.26", "8.3.35"]);
        assert_eq!(releases[0].vc, Some(17));
        assert_eq!(releases[1].vc, Some(16));
        assert_eq!(
            releases[0].url,
            "https://example.test/php-8.4.26-nts-Win32-vs17-x64.zip"
        );
    }

    #[test]
    fn an_earlier_page_wins_a_duplicate() {
        let older = r#"<a href="php-8.3.35-nts-Win32-vs16-x64.zip">x</a>"#;
        let newer = r#"<a href="php-8.3.35-nts-Win32-vs16-x64.zip">x</a>"#;
        let releases = parse_php(&[(newer, "https://a/"), (older, "https://b/")]);
        assert_eq!(releases.len(), 1);
        assert_eq!(releases[0].url, "https://a/php-8.3.35-nts-Win32-vs16-x64.zip");
    }

    #[test]
    fn nginx_is_newest_first_and_skips_ancient_builds() {
        let releases = parse_nginx(NGINX);
        let versions: Vec<&str> = releases.iter().map(|r| r.version.as_str()).collect();
        assert_eq!(versions, vec!["1.27.0", "1.26.2"]);
        assert_eq!(
            releases[0].url,
            "https://nginx.org/download/nginx-1.27.0.zip"
        );
        assert!(releases[0].vc.is_none());
    }

    #[test]
    fn empty_pages_yield_nothing() {
        assert!(parse_php(&[("", "https://x/")]).is_empty());
        assert!(parse_nginx("").is_empty());
        let _: &[Release] = &[];
    }
}
