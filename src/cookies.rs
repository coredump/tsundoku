//! Cookie loading utilities for scrapers.
//!
//! Supports Netscape HTTP cookie files, commonly exported by browser extensions.

use reqwest::Url;
use reqwest::cookie::Jar;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use thiserror::Error;

/// Cookie entry parsed from a Netscape cookie file.
#[derive(Debug, Clone, PartialEq, Eq)]
struct NetscapeCookie {
    domain: String,
    include_subdomains: bool,
    path: String,
    secure: bool,
    expires_unix: Option<u64>,
    name: String,
    value: String,
    http_only: bool,
}

/// Errors that can occur while loading cookies.
#[derive(Error, Debug)]
pub enum CookieError {
    /// Failed to read or walk the filesystem.
    #[error("Failed to read cookie file: {0}")]
    Io(#[from] std::io::Error),

    /// Cookie file contains an invalid line.
    #[error("Invalid Netscape cookie line: {0}")]
    InvalidLine(String),

    /// Cookie domain could not be converted into a URL.
    #[error("Invalid cookie domain: {0}")]
    InvalidDomain(String),
}

/// Loads cookies from a Netscape cookie file into a reqwest cookie jar.
pub fn load_netscape_cookie_jar(
    config_dir: &Path,
    name_tokens: &[&str],
) -> Result<(Arc<Jar>, Option<PathBuf>), CookieError> {
    let jar = Arc::new(Jar::default());
    let cookie_path = find_cookie_file(config_dir, name_tokens)?;
    if let Some(path) = &cookie_path {
        let cookies = parse_netscape_cookie_file(path)?;
        add_cookies_to_jar(&jar, &cookies)?;
    }
    Ok((jar, cookie_path))
}

fn find_cookie_file(root: &Path, name_tokens: &[&str]) -> Result<Option<PathBuf>, std::io::Error> {
    let mut best: Option<(PathBuf, std::time::SystemTime)> = None;
    find_cookie_file_recursive(root, name_tokens, &mut best)?;
    Ok(best.map(|(path, _)| path))
}

fn find_cookie_file_recursive(
    dir: &Path,
    name_tokens: &[&str],
    best: &mut Option<(PathBuf, std::time::SystemTime)>,
) -> Result<(), std::io::Error> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            find_cookie_file_recursive(&path, name_tokens, best)?;
            continue;
        }

        let file_name = match path.file_name().and_then(OsStr::to_str) {
            Some(name) => name.to_ascii_lowercase(),
            None => continue,
        };

        if !file_name.ends_with(".txt") {
            continue;
        }

        if !name_tokens
            .iter()
            .all(|token| file_name.contains(&token.to_ascii_lowercase()))
        {
            continue;
        }

        let modified = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH);

        let should_replace = match best {
            Some((_, best_time)) => modified > *best_time,
            None => true,
        };

        if should_replace {
            *best = Some((path, modified));
        }
    }

    Ok(())
}

fn parse_netscape_cookie_file(path: &Path) -> Result<Vec<NetscapeCookie>, CookieError> {
    let content = std::fs::read_to_string(path)?;
    let mut cookies = Vec::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let (http_only, line) = if let Some(stripped) = line.strip_prefix("#HttpOnly_") {
            (true, stripped)
        } else if line.starts_with('#') {
            continue;
        } else {
            (false, line)
        };

        let mut parts = line.splitn(7, '\t');
        let domain = parts
            .next()
            .ok_or_else(|| CookieError::InvalidLine(line.to_string()))?;
        let include_subdomains = parts
            .next()
            .ok_or_else(|| CookieError::InvalidLine(line.to_string()))?
            .eq_ignore_ascii_case("true");
        let path = parts
            .next()
            .ok_or_else(|| CookieError::InvalidLine(line.to_string()))?;
        let secure = parts
            .next()
            .ok_or_else(|| CookieError::InvalidLine(line.to_string()))?
            .eq_ignore_ascii_case("true");
        let expires_raw = parts
            .next()
            .ok_or_else(|| CookieError::InvalidLine(line.to_string()))?;
        let name = parts
            .next()
            .ok_or_else(|| CookieError::InvalidLine(line.to_string()))?;
        let value = parts
            .next()
            .ok_or_else(|| CookieError::InvalidLine(line.to_string()))?;

        let expires_unix = expires_raw
            .parse::<u64>()
            .ok()
            .and_then(|ts| if ts == 0 { None } else { Some(ts) });

        cookies.push(NetscapeCookie {
            domain: domain.to_string(),
            include_subdomains,
            path: path.to_string(),
            secure,
            expires_unix,
            name: name.to_string(),
            value: value.to_string(),
            http_only,
        });
    }

    Ok(cookies)
}

fn add_cookies_to_jar(jar: &Jar, cookies: &[NetscapeCookie]) -> Result<(), CookieError> {
    for cookie in cookies {
        let host = cookie.domain.trim_start_matches('.');
        if host.is_empty() {
            return Err(CookieError::InvalidDomain(cookie.domain.clone()));
        }

        let url = Url::parse(&format!("https://{}/", host))
            .map_err(|_| CookieError::InvalidDomain(cookie.domain.clone()))?;

        let mut cookie_str = format!("{}={}", cookie.name, cookie.value);
        cookie_str.push_str(&format!("; Path={}", cookie.path));

        if cookie.include_subdomains {
            cookie_str.push_str(&format!("; Domain={}", cookie.domain));
        }

        if cookie.secure {
            cookie_str.push_str("; Secure");
        }

        if cookie.http_only {
            cookie_str.push_str("; HttpOnly");
        }

        jar.add_cookie_str(&cookie_str, &url);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_parse_netscape_cookie_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pixiv-cookies.txt");
        let content = r#"
# Netscape HTTP Cookie File
.pixiv.net	TRUE	/	TRUE	2145916800	PHPSESSID	abc123
#HttpOnly_.pixiv.net	FALSE	/	FALSE	0	p_ab_id	idvalue
        "#;
        std::fs::write(&path, content).unwrap();

        let cookies = parse_netscape_cookie_file(&path).unwrap();
        assert_eq!(cookies.len(), 2);
        assert_eq!(cookies[0].domain, ".pixiv.net");
        assert!(cookies[0].include_subdomains);
        assert!(cookies[0].secure);
        assert_eq!(cookies[0].name, "PHPSESSID");
        assert_eq!(cookies[0].value, "abc123");
        assert!(!cookies[0].http_only);

        assert_eq!(cookies[1].domain, ".pixiv.net");
        assert!(!cookies[1].include_subdomains);
        assert!(!cookies[1].secure);
        assert_eq!(cookies[1].name, "p_ab_id");
        assert_eq!(cookies[1].value, "idvalue");
        assert!(cookies[1].http_only);
        assert_eq!(cookies[1].expires_unix, None);
    }

    #[test]
    fn test_find_cookie_file_picks_latest() {
        let dir = TempDir::new().unwrap();
        let first = dir.path().join("pixiv-cookies.txt");
        let second = dir.path().join("pixiv-cookies-latest.txt");

        std::fs::write(&first, "example").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&second, "example").unwrap();

        let found = find_cookie_file(dir.path(), &["pixiv"]).unwrap();
        assert_eq!(found.unwrap(), second);
    }

    #[test]
    fn test_parse_invalid_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("pixiv-cookies.txt");
        std::fs::write(&path, "invalid-line").unwrap();

        let err = parse_netscape_cookie_file(&path).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Invalid Netscape cookie line"));
    }
}
