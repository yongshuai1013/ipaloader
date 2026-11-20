use std::sync::OnceLock;

use super::middleware::WasmProxyMiddleware;
use plist::Dictionary;
use plist_macro::plist_to_xml_string;
use plist_macro::pretty_print_dictionary;
use reqwest::{
    ClientBuilder,
    header::{HeaderMap, HeaderValue},
};
use reqwest_middleware::ClientBuilder as MwClientBuilder;
use rootcause::prelude::*;
use tracing::debug;
use tracing::warn;

#[cfg(not(feature = "wasm"))]
use crate::sideload::cert_identity::APPLE_ROOT;
use crate::{SideloadError, anisette::AnisetteClientInfo, util::plist::PlistDataExtract};

const URL_BAG: &str = "https://gsa.apple.com/grandslam/GsService2/lookup";

/// Capture hook for the outgoing GrandSlam request text (ipaloader
/// branch `capture-srp`, built for the SideStore 503 byte-level
/// comparison): the host crate registers a sink that appends each
/// formatted block to its diagnostic log. When no sink is registered
/// (every other consumer of this crate) capture is a no-op, and
/// request construction and sending are completely untouched — the
/// capture only reads values that were already built for sending.
static SRP_REQUEST_CAPTURE: OnceLock<fn(&str)> = OnceLock::new();

/// Register the sink that receives one formatted text block per
/// outgoing GrandSlam request (the URL bag lookup plus every plist
/// request, which includes the SRP init/complete pair). Called once
/// by the host at startup; later calls are ignored.
pub fn set_srp_request_capture(sink: fn(&str)) {
    let _ = SRP_REQUEST_CAPTURE.set(sink);
}

fn emit_srp_capture(block: &str) {
    if let Some(sink) = SRP_REQUEST_CAPTURE.get() {
        sink(block);
    }
}

/// The step name of a plist request, taken from the body's Request
/// dictionary ("init", "complete", "apptokens", ...). Falls back to
/// "plist" when the body carries no operation tag.
fn srp_step_name(body: &Dictionary) -> String {
    body.get("Request")
        .and_then(|v| v.as_dictionary())
        .and_then(|request| request.get("o"))
        .and_then(|o| o.as_string())
        .unwrap_or("plist")
        .to_string()
}

/// Render one outgoing request for the capture log: step name,
/// method, full URL, every header on its own line, then the body
/// verbatim between the `===SRP-REQ <step> body begin/end===`
/// delimiters. A body that is not clean UTF-8 text is rendered as
/// base64 of the exact bytes instead, so the log always holds the
/// request byte-for-byte.
fn format_srp_capture(
    step: &str,
    method: &str,
    url: &str,
    headers: &HeaderMap,
    body: Option<&[u8]>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("===SRP-REQ {step} begin===\n"));
    out.push_str(&format!("{method} {url}\n"));
    for (name, value) in headers.iter() {
        match value.to_str() {
            Ok(v) => out.push_str(&format!("{name}: {v}\n")),
            Err(_) => out.push_str(&format!(
                "{name}: <non-text header value, base64> {}\n",
                base64_encode(value.as_bytes())
            )),
        }
    }
    match body {
        Some(bytes) if !bytes.is_empty() => {
            out.push_str(&format!("===SRP-REQ {step} body begin===\n"));
            match std::str::from_utf8(bytes) {
                Ok(text)
                    if text
                        .chars()
                        .all(|c| !c.is_control() || matches!(c, '\n' | '\r' | '\t')) =>
                {
                    out.push_str(text);
                    if !text.ends_with('\n') {
                        out.push('\n');
                    }
                }
                _ => {
                    out.push_str(
                        "(body is not plain UTF-8 text; base64 of the exact bytes follows)\n",
                    );
                    out.push_str(&base64_encode(bytes));
                    out.push('\n');
                }
            }
            out.push_str(&format!("===SRP-REQ {step} body end===\n"));
        }
        _ => out.push_str("(no request body)\n"),
    }
    out.push_str(&format!("===SRP-REQ {step} end===\n"));
    out
}

/// Minimal RFC 4648 base64 encoder (standard alphabet, with
/// padding), so the capture needs no extra dependency.
fn base64_encode(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = ((chunk[0] as u32) << 16)
            | ((*chunk.get(1).unwrap_or(&0) as u32) << 8)
            | (*chunk.get(2).unwrap_or(&0) as u32);
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub struct GrandSlam {
    pub client: reqwest_middleware::ClientWithMiddleware,
    pub client_info: AnisetteClientInfo,
    url_bag: Dictionary,
}

/// Build the shared TLS config for the GrandSlam / developer-services
/// client: one rustls root store holding BOTH the WebPKI public roots
/// and the Apple Root CA (see the ipaloader patch note in
/// `GrandSlam::build_reqwest_client` for why both families are needed).
#[cfg(not(feature = "wasm"))]
fn apple_and_webpki_tls_config() -> Result<rustls::ClientConfig, Report> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    roots.add(rustls::pki_types::CertificateDer::from(APPLE_ROOT))?;
    let mut config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    // Mirror the ALPN reqwest itself sets for its default
    // HttpVersionPref::All, so protocol negotiation is unchanged from a
    // reqwest-built config.
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

impl GrandSlam {
    /// Create a new GrandSlam instance
    ///
    /// # Arguments
    /// - `client`: The reqwest client to use for requests
    pub async fn new(
        client_info: AnisetteClientInfo,
        debug: bool,
        proxy_url: Option<String>,
    ) -> Result<Self, Report> {
        let client =
            Self::build_reqwest_client(debug, proxy_url).context("Failed to build HTTP client")?;
        let base_headers = Self::base_headers(&client_info, false)?;
        let url_bag = Self::fetch_url_bag(&client, base_headers).await?;
        Ok(Self {
            client,
            client_info,
            url_bag,
        })
    }

    /// Fetch the URL bag from GrandSlam and cache it
    pub async fn fetch_url_bag(
        client: &reqwest_middleware::ClientWithMiddleware,
        mut base_headers: HeaderMap,
    ) -> Result<Dictionary, Report> {
        debug!("Fetching URL bag from GrandSlam");
        emit_srp_capture(&format_srp_capture(
            "lookup",
            "GET",
            URL_BAG,
            &base_headers,
            None,
        ));
        let resp = client
            .get(URL_BAG)
            .headers(base_headers)
            .send()
            .await
            .context("Failed to fetch URL Bag")?
            .text()
            .await
            .context("Failed to read URL Bag response text")?;

        let dict: Dictionary =
            plist::from_bytes(resp.as_bytes()).context("Failed to parse URL Bag plist")?;
        let urls = dict
            .get("urls")
            .and_then(|v| v.as_dictionary())
            .cloned()
            .ok_or_else(|| report!("URL Bag plist missing 'urls' dictionary"))?;

        Ok(urls)
    }

    pub fn get_url(&self, key: &str) -> Result<String, Report> {
        let url = self
            .url_bag
            .get_string(key)
            .context("Unable to find key in URL bag")?;
        Ok(url)
    }

    pub fn get(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .get(url)
            .headers(Self::base_headers(&self.client_info, false)?);

        Ok(builder)
    }

    pub fn get_sms(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .get(url)
            .headers(Self::base_headers(&self.client_info, true)?);

        Ok(builder)
    }

    pub fn put_sms(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .put(url)
            .headers(Self::base_headers(&self.client_info, true)?);

        Ok(builder)
    }

    pub fn post(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .post(url)
            .headers(Self::base_headers(&self.client_info, false)?);

        Ok(builder)
    }

    pub fn post_sms(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .post(url)
            .headers(Self::base_headers(&self.client_info, true)?);

        Ok(builder)
    }

    pub fn patch(&self, url: &str) -> Result<reqwest_middleware::RequestBuilder, Report> {
        let builder = self
            .client
            .patch(url)
            .headers(Self::base_headers(&self.client_info, false)?);

        Ok(builder)
    }

    pub async fn plist_request(
        &self,
        url: &str,
        body: &Dictionary,
        additional_headers: Option<HeaderMap>,
        retry_limit: Option<u32>,
    ) -> Result<Dictionary, Report> {
        let body_string = plist_to_xml_string(body);
        if SRP_REQUEST_CAPTURE.get().is_some() {
            // Read-only capture of the request exactly as it is about
            // to be sent below: the base headers from post() with the
            // caller's additional headers merged over them (the same
            // per-name insert semantics reqwest applies), plus the
            // serialized body. A failure to rebuild the base headers
            // here only skips the capture; post() below surfaces any
            // real error exactly as before.
            if let Ok(mut merged) = Self::base_headers(&self.client_info, false) {
                if let Some(extra) = &additional_headers {
                    for (name, value) in extra.iter() {
                        merged.insert(name.clone(), value.clone());
                    }
                }
                emit_srp_capture(&format_srp_capture(
                    &srp_step_name(body),
                    "POST",
                    url,
                    &merged,
                    Some(body_string.as_bytes()),
                ));
            }
        }
        let result = self
            .post(url)?
            .headers(
                additional_headers
                    .clone()
                    .unwrap_or_else(reqwest::header::HeaderMap::new),
            )
            .body(body_string)
            .send()
            .await
            .context("Failed to send grandslam request")?;

        if let Some(retries) = retry_limit
            && result.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
        {
            if retries > 0 {
                warn!(
                    "Received 429 Too Many Requests, retrying request ({} retries left)",
                    retries
                );
                return Box::pin(self.plist_request(
                    url,
                    body,
                    additional_headers,
                    Some(retries - 1),
                ))
                .await;
            } else {
                bail!("Received 429 Too Many Requests, no retries left");
            }
        }

        let resp = result
            .error_for_status()
            .context("Received error response from grandslam")?
            .text()
            .await
            .context("Failed to read grandslam response as text")?;

        let dict: Dictionary = plist::from_bytes(resp.as_bytes())
            .context("Failed to parse grandslam response plist")
            .attach_with(|| resp.clone())?;

        let response_plist = dict
            .get("Response")
            .and_then(|v| v.as_dictionary())
            .cloned()
            .ok_or_else(|| {
                report!("grandslam response missing 'Response'")
                    .attach(pretty_print_dictionary(&dict))
            })?;

        Ok(response_plist)
    }

    fn base_headers(
        client_info: &AnisetteClientInfo,
        sms: bool,
    ) -> Result<reqwest::header::HeaderMap, Report> {
        let mut headers = reqwest::header::HeaderMap::new();
        if !sms {
            headers.insert("Content-Type", HeaderValue::from_static("text/x-xml-plist"));
            headers.insert("Accept", HeaderValue::from_static("text/x-xml-plist"));
        } else {
            headers.insert("Content-Type", HeaderValue::from_static("application/json"));
            headers.insert("Accept", HeaderValue::from_static("application/json"));
        }
        headers.insert(
            "X-Mme-Client-Info",
            HeaderValue::from_str(&client_info.client_info)?,
        );
        headers.insert(
            "User-Agent",
            HeaderValue::from_str(&client_info.user_agent)?,
        );
        headers.insert(
            "X-Xcode-Version",
            HeaderValue::from_static("27.0 (27A5218g)"),
        );
        headers.insert(
            "X-Apple-App-Info",
            HeaderValue::from_static("com.apple.gs.xcode.auth"),
        );
        headers.insert(
            reqwest::header::CONNECTION,
            HeaderValue::from_static("close"),
        );

        Ok(headers)
    }

    /// Build a reqwest client trusting the WebPKI roots plus the
    /// Apple root certificate
    ///
    /// # Arguments
    /// - `debug`: DANGER, If true, accept invalid certificates and enable verbose connection logging
    /// # Errors
    /// Returns an error if the reqwest client cannot be built
    pub fn build_reqwest_client(
        #[cfg(not(feature = "wasm"))] debug: bool,
        #[cfg(feature = "wasm")] _debug: bool,
        proxy_url: Option<String>,
    ) -> Result<reqwest_middleware::ClientWithMiddleware, Report> {
        #[cfg(not(feature = "wasm"))]
        let client = ClientBuilder::new()
            // ipaloader patch (vendored isideload 0.5.2): hand reqwest
            // one preconfigured rustls config whose root store holds
            // the WebPKI roots AND the Apple Root CA.
            //
            // Upstream's `.add_root_certificate(APPLE_ROOT)` cannot
            // build on Android with reqwest 0.13 — in the default
            // (platform verifier) mode extra roots are unsupported
            // there and construction always fails with
            // "rustls-platform-verifier could not load extra certs".
            // The first workaround, `.tls_certs_only([APPLE_ROOT])`,
            // fixed GrandSlam (its endpoints chain to the Apple Root
            // CA) but broke the developer-services requests that share
            // this client through DeveloperSession:
            // developerservices2.apple.com chains to a public CA root
            // and failed on device with
            // InvalidCertificate(UnknownIssuer). A preconfigured
            // config never goes through the extra-roots code path and
            // trusts both root families, so both endpoint families
            // verify.
            .tls_backend_preconfigured(apple_and_webpki_tls_config()?)
            .http1_title_case_headers()
            .danger_accept_invalid_certs(debug)
            .connection_verbose(debug)
            // .pool_max_idle_per_host(0)
            .build()?;
        #[cfg(feature = "wasm")]
        let client = ClientBuilder::new().build()?;

        let builder = MwClientBuilder::new(client);
        let builder = if let Some(proxy_url) = proxy_url {
            builder.with(WasmProxyMiddleware::new(proxy_url))
        } else {
            builder
        };
        Ok(builder.build())
    }
}

pub trait GrandSlamErrorChecker {
    fn check_grandslam_error(self) -> Result<Dictionary, Report<SideloadError>>;
}

impl GrandSlamErrorChecker for Dictionary {
    fn check_grandslam_error(self) -> Result<Self, Report<SideloadError>> {
        let result = match self.get("Status") {
            Some(plist::Value::Dictionary(d)) => d,
            _ => &self,
        };

        if result.get_signed_integer("ec").unwrap_or(0) != 0 {
            bail!(SideloadError::AuthWithMessage(
                result.get_signed_integer("ec").unwrap_or(-1),
                result.get_str("em").unwrap_or("Unknown error").to_string(),
            ))
        }

        Ok(self)
    }
}
