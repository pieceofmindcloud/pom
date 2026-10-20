//! Links that leave POM in the desktop shells (WebView2, WebKitGTK and the
//! macOS WebKit panel): a purchase, the site, a GitHub page. The embedded
//! view only shows this machine's own interface, so such a link opens in the
//! system browser and POM keeps running where it was.

/// Whether `url` leaves this machine's interface: a web address on another
/// host, or a `mailto:`. Loopback addresses (`127.0.0.1`, `localhost`, `::1`)
/// are the node itself.
pub fn is_external(url: &str) -> bool {
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    match parsed.scheme() {
        "mailto" => true,
        "http" | "https" => match parsed.host() {
            Some(url::Host::Domain(domain)) => {
                let domain = domain.to_ascii_lowercase();
                domain != "localhost" && !domain.ends_with(".localhost")
            }
            Some(url::Host::Ipv4(address)) => !address.is_loopback(),
            Some(url::Host::Ipv6(address)) => !address.is_loopback(),
            None => false,
        },
        _ => false,
    }
}

/// Opens `url` in the system's default browser. Only web and mail addresses
/// are handed over, never a local file or another scheme.
pub fn open_in_browser(url: &str) {
    let Ok(parsed) = url::Url::parse(url) else {
        return;
    };
    if !matches!(parsed.scheme(), "http" | "https" | "mailto") {
        return;
    }
    let url = parsed.as_str();
    #[cfg(target_os = "linux")]
    let result = std::process::Command::new("xdg-open").arg(url).spawn();
    #[cfg(target_os = "macos")]
    let result = std::process::Command::new("open").arg(url).spawn();
    // `url.dll` takes the address as one argument: `cmd /c start` would read
    // the `&` of a query string as a command separator.
    #[cfg(windows)]
    let result = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("rundll32")
            .arg("url.dll,FileProtocolHandler")
            .arg(url)
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    let result: std::io::Result<std::process::Child> =
        Err(std::io::Error::other("no browser opener on this system"));
    if let Err(error) = result {
        tracing::warn!(%url, %error, "could not open the link in the browser");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_node_itself_is_not_external_and_the_site_is() {
        assert!(!is_external("https://127.0.0.1:8080/admin-ui/plugins"));
        assert!(!is_external("http://localhost:8080/admin-ui/"));
        assert!(!is_external("https://[::1]:8080/"));
        assert!(!is_external("https://pom.localhost/"));
        assert!(!is_external("javascript:alert(1)"));
        assert!(!is_external("about:blank"));
        assert!(is_external(
            "https://pieceofmind.cloud/comprar/?plugin=logs"
        ));
        assert!(is_external(
            "https://github.com/pieceofmindcloud/pom-plugin-ghostly"
        ));
        assert!(is_external("http://192.168.1.20:8080/"));
        assert!(is_external("mailto:contact@pieceofmind.cloud"));
    }
}
