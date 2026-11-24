//! The GitHub client both update sources share, and the digest check every
//! download passes.

use progressor::ProgressExt as _;
use sha2::{Digest as _, Sha256};
use zenwave::Client as _;

use super::UpdateError;

/// The GitHub REST client of both update sources.
#[derive(Debug)]
pub(super) struct GitHub {
    api_base: String,
    token: Option<String>,
}

impl GitHub {
    /// A client of the API at `api_base`. `token` authenticates API calls;
    /// the CI path needs one, the release path only raises its rate limit.
    pub(super) fn new(api_base: &str, token: Option<String>) -> Self {
        Self {
            api_base: api_base.trim_end_matches('/').to_owned(),
            token,
        }
    }

    /// The API's base URL, without a trailing slash.
    pub(super) fn api_base(&self) -> &str {
        &self.api_base
    }

    /// `GET url` as an API call.
    pub(super) async fn get(&self, url: &str) -> Result<bytes::Bytes, UpdateError> {
        self.request(url, true).await
    }

    /// `GET url` of a public download, which carries no credentials.
    pub(super) async fn get_public(&self, url: &str) -> Result<bytes::Bytes, UpdateError> {
        self.request(url, false).await
    }

    /// `GET url` reporting the body's running total to `progress` as
    /// `downloaded`/`total`; `total` is `announced` when the response sent no
    /// length.
    pub(super) async fn get_bytes(
        &self,
        url: &str,
        announced: Option<u64>,
        progress: &mut (impl FnMut(u64, Option<u64>) + Send),
    ) -> Result<bytes::Bytes, UpdateError> {
        self.download_bytes(url, true, announced, progress).await
    }

    /// `GET url` reporting a public download's running total.
    pub(super) async fn get_public_bytes(
        &self,
        url: &str,
        progress: &mut (impl FnMut(u64, Option<u64>) + Send),
    ) -> Result<bytes::Bytes, UpdateError> {
        self.download_bytes(url, false, None, progress).await
    }

    /// `GET url` through `zenwave`'s concurrent `Range` downloader, streaming
    /// live progress snapshots via `progressor`.
    async fn download_bytes(
        &self,
        url: &str,
        authenticated: bool,
        announced: Option<u64>,
        progress: &mut (impl FnMut(u64, Option<u64>) + Send),
    ) -> Result<bytes::Bytes, UpdateError> {
        let mut backend = zenwave::client();
        let mut builder = backend
            .get(url)?
            .header(
                zenwave::header::USER_AGENT.as_str(),
                concat!("may-agent/", env!("CARGO_PKG_VERSION")),
            )?
            .header("Accept", "application/vnd.github+json")?
            .header("X-GitHub-Api-Version", "2022-11-28")?;
        if authenticated && let Some(token) = &self.token {
            builder = builder.bearer_auth(token);
        }
        Ok(builder
            .download()
            .bytes()
            .observe(|update| {
                progress(update.downloaded, update.total.or(announced));
            })
            .await?)
    }

    /// `GET url` with the shared GitHub headers and credentials, returned as
    /// the response's `bytes`.
    async fn request(&self, url: &str, authenticated: bool) -> Result<bytes::Bytes, UpdateError> {
        let mut backend = zenwave::client().error_for_status();
        let mut builder = backend
            .get(url)?
            .header(
                zenwave::header::USER_AGENT.as_str(),
                concat!("may-agent/", env!("CARGO_PKG_VERSION")),
            )?
            .header("Accept", "application/vnd.github+json")?
            .header("X-GitHub-Api-Version", "2022-11-28")?;
        if authenticated && let Some(token) = &self.token {
            builder = builder.bearer_auth(token);
        }
        Ok(builder.bytes().await?)
    }
}

/// Checks `bytes` against the hex sha256 `expected`.
///
/// # Errors
/// [`UpdateError::DigestMismatch`] when the digests differ.
pub(super) fn verify_sha256(bytes: &[u8], expected: &str) -> Result<(), UpdateError> {
    let actual = hex::encode(Sha256::digest(bytes));
    if actual.eq_ignore_ascii_case(expected) {
        Ok(())
    } else {
        Err(UpdateError::DigestMismatch {
            expected: expected.to_owned(),
            actual,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use may_protocol::test_support::{ApiReply, ApiRequest, serve_api};

    use super::*;

    /// Deterministic binary payload exceeding `zenwave`'s 1 MiB split threshold
    /// and 2 MiB default chunk size so multi-chunk Range downloads are exercised.
    const TEST_ARCHIVE_LEN: usize = 5 * 1024 * 1024 + 137;

    fn test_archive() -> Vec<u8> {
        (0..TEST_ARCHIVE_LEN)
            .map(|i| u8::try_from((i.wrapping_mul(31).wrapping_add(7)) % 256).expect("fits in u8"))
            .collect()
    }

    fn parse_byte_range(header: &str, total: usize) -> Option<(usize, usize)> {
        let spec = header.strip_prefix("bytes=")?;
        let (start, end) = spec.split_once('-')?;
        let start: usize = start.parse().ok()?;
        let last = total.checked_sub(1)?;
        let end: usize = if end.is_empty() {
            last
        } else {
            end.parse::<usize>().ok()?.min(last)
        };
        (start <= end && end < total).then_some((start, end))
    }

    fn range_reply(request: &ApiRequest, payload: &[u8], etag: &str) -> ApiReply {
        let total = payload.len();
        if let Some((start, end)) = parse_byte_range(&request.range, total) {
            ApiReply::bytes(206, payload[start..=end].to_vec())
                .header("accept-ranges", "bytes")
                .header("content-range", format!("bytes {start}-{end}/{total}"))
                .header("etag", etag)
        } else {
            ApiReply::bytes(200, payload.to_vec())
                .header("accept-ranges", "bytes")
                .header("etag", etag)
        }
    }

    #[test]
    fn a_matching_digest_passes_in_either_case() {
        // `printf abc | shasum -a 256`
        let digest = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        verify_sha256(b"abc", digest).expect("the digest matches");
        verify_sha256(b"abc", &digest.to_uppercase()).expect("case does not matter");
    }

    #[test]
    fn a_different_digest_is_refused_with_both_values() {
        let error = verify_sha256(b"abd", "00").expect_err("the digest differs");
        assert!(
            matches!(&error, UpdateError::DigestMismatch { expected, actual }
                if expected == "00" && actual.len() == 64),
            "{error:?}"
        );
    }

    #[tokio::test]
    async fn concurrent_range_download_reassembles_archive_verifies_sha256_and_streams_progress() {
        let payload = Arc::new(test_archive());
        let expected_digest = hex::encode(Sha256::digest(payload.as_slice()));
        let expected_len = payload.len() as u64;

        let served = Arc::clone(&payload);
        let (base, requests) = serve_api(move |request, _| match request.path.as_str() {
            "/may.tar.xz" => range_reply(request, &served, "\"release-v1\""),
            _ => ApiReply::json(404, r#"{"message":"Not Found"}"#),
        })
        .await;

        let github = GitHub::new(&base, Some("unused-token".into()));
        let mut updates = Vec::new();
        let downloaded = github
            .get_public_bytes(&format!("{base}/may.tar.xz"), &mut |done, total| {
                updates.push((done, total));
            })
            .await
            .expect("concurrent Range download succeeds");

        assert_eq!(downloaded.as_ref(), payload.as_slice());
        verify_sha256(&downloaded, &expected_digest).expect("reassembled archive matches sha256");

        let recorded = requests.lock().expect("requests").clone();
        assert!(
            recorded.len() >= 3,
            "expected probe plus multiple Range chunks, got {} requests: {recorded:?}",
            recorded.len()
        );
        assert_eq!(recorded[0].range, "bytes=0-0");
        assert!(recorded.iter().all(|req| req.authorization.is_empty()));
        assert!(recorded.iter().all(|req| req.range.starts_with("bytes=")));

        assert!(
            updates.len() >= 2,
            "expected progressive updates across chunks, got {updates:?}"
        );
        assert!(updates.windows(2).all(|pair| pair[0].0 <= pair[1].0));
        assert!(
            updates
                .iter()
                .all(|(_, total)| *total == Some(expected_len))
        );
        assert_eq!(
            updates.last().copied(),
            Some((expected_len, Some(expected_len)))
        );
    }

    #[tokio::test]
    async fn authenticated_redirect_strips_authorization_on_storage_chunks() {
        let payload = Arc::new(test_archive());
        let expected_digest = hex::encode(Sha256::digest(payload.as_slice()));
        let expected_len = payload.len() as u64;

        let served = Arc::clone(&payload);
        let (storage_base, storage_requests) =
            serve_api(move |request, _| match request.path.as_str() {
                "/signed/artifact.zip?sig=ok" => range_reply(request, &served, "\"ci-zip-v1\""),
                _ => ApiReply::json(404, r#"{"message":"Not Found"}"#),
            })
            .await;

        let redirect_target = format!("{storage_base}/signed/artifact.zip?sig=ok");
        let (api_base, api_requests) = serve_api(move |request, _| match request.path.as_str() {
            "/repos/May-Agent/may/actions/artifacts/1/zip" => {
                ApiReply::bytes(302, Vec::new()).header("location", redirect_target.as_str())
            }
            _ => ApiReply::json(404, r#"{"message":"Not Found"}"#),
        })
        .await;

        let github = GitHub::new(&api_base, Some("secret-token".into()));
        let mut updates = Vec::new();
        let downloaded = github
            .get_bytes(
                &format!("{api_base}/repos/May-Agent/may/actions/artifacts/1/zip"),
                Some(expected_len),
                &mut |done, total| updates.push((done, total)),
            )
            .await
            .expect("redirected concurrent download succeeds");

        assert_eq!(downloaded.as_ref(), payload.as_slice());
        verify_sha256(&downloaded, &expected_digest).expect("redirected archive matches sha256");

        let api_reqs = api_requests.lock().expect("api requests").clone();
        assert_eq!(
            api_reqs.len(),
            1,
            "redirect origin is hit once: {api_reqs:?}"
        );
        assert_eq!(api_reqs[0].authorization, "Bearer secret-token");

        let storage_reqs = storage_requests.lock().expect("storage requests").clone();
        assert!(
            storage_reqs.len() >= 3,
            "expected probe plus multiple storage chunks, got {}",
            storage_reqs.len()
        );
        assert!(
            storage_reqs.iter().all(|req| req.authorization.is_empty()),
            "cross-origin storage requests must not carry Authorization: {storage_reqs:?}"
        );
        assert_eq!(
            updates.last().copied(),
            Some((expected_len, Some(expected_len)))
        );
    }

    #[tokio::test]
    async fn server_ignoring_range_falls_back_to_single_200_response() {
        let mut payload = b"fallback binary payload \x00\xff\xfe ".to_vec();
        payload.extend_from_slice("日本語 한국어 中文".as_bytes());
        let payload = Arc::new(payload);
        let expected_digest = hex::encode(Sha256::digest(payload.as_slice()));
        let expected_len = payload.len() as u64;

        let served = Arc::clone(&payload);
        let (base, requests) = serve_api(move |request, _| match request.path.as_str() {
            "/fallback.tar.xz" => ApiReply::bytes(200, served.as_ref().clone()),
            _ => ApiReply::json(404, r#"{"message":"Not Found"}"#),
        })
        .await;

        let github = GitHub::new(&base, None);
        let mut updates = Vec::new();
        let downloaded = github
            .get_public_bytes(&format!("{base}/fallback.tar.xz"), &mut |done, total| {
                updates.push((done, total));
            })
            .await
            .expect("200 OK fallback succeeds");

        assert_eq!(downloaded.as_ref(), payload.as_slice());
        verify_sha256(&downloaded, &expected_digest).expect("fallback archive matches sha256");

        let recorded = requests.lock().expect("requests").clone();
        assert_eq!(
            recorded.len(),
            1,
            "200 OK fallback must stream the first response without re-requesting: {recorded:?}"
        );
        assert_eq!(
            updates.last().copied(),
            Some((expected_len, Some(expected_len)))
        );
    }
}
