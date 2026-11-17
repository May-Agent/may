//! The GitHub client both update sources share, and the digest check every
//! download passes.

use sha2::{Digest as _, Sha256};

use super::UpdateError;

/// The GitHub REST client of both update sources.
#[derive(Debug)]
pub(super) struct GitHub {
    client: reqwest::Client,
    api_base: String,
    token: Option<String>,
}

impl GitHub {
    /// A client of the API at `api_base`. `token` authenticates API calls;
    /// the CI path needs one, the release path only raises its rate limit.
    pub(super) fn new(api_base: &str, token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::builder()
                .user_agent(concat!("may-agent/", env!("CARGO_PKG_VERSION")))
                .build()
                .expect("the HTTP client builds with its default configuration"),
            api_base: api_base.trim_end_matches('/').to_owned(),
            token,
        }
    }

    /// The API's base URL, without a trailing slash.
    pub(super) fn api_base(&self) -> &str {
        &self.api_base
    }

    /// `GET url` as an API call.
    pub(super) async fn get(&self, url: &str) -> Result<reqwest::Response, UpdateError> {
        let mut request = self
            .client
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        Ok(request.send().await?.error_for_status()?)
    }

    /// `GET url` of a public download, which carries no credentials.
    pub(super) async fn get_public(&self, url: &str) -> Result<bytes::Bytes, UpdateError> {
        Ok(self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?)
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
    use super::*;

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
}
