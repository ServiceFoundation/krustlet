//! OCI distribution client
//!
//! *Note*: This client is very feature poor. We hope to expand this to be a complete
//! OCI distribution client in the future.

use crate::errors::*;
use crate::manifest::OciManifest;
use crate::secrets::RegistryAuth;
use crate::secrets::*;
use crate::Reference;

use anyhow::Context;
use futures_util::future;
use futures_util::stream::StreamExt;
use hyperx::header::Header;
use log::debug;
use reqwest::header::HeaderMap;
use std::collections::HashMap;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use www_authenticate::{Challenge, ChallengeFields, RawChallenge, WwwAuthenticate};

/// The data for an image or module.
#[derive(Clone)]
pub struct ImageData {
    /// The content of the image or module.
    pub content: Vec<u8>,
    /// The digest of the image or module.
    pub digest: Option<String>,
}

/// The OCI client connects to an OCI registry and fetches OCI images.
///
/// An OCI registry is a container registry that adheres to the OCI Distribution
/// specification. DockerHub is one example, as are ACR and GCR. This client
/// provides a native Rust implementation for pulling OCI images.
///
/// Some OCI registries support completely anonymous access. But most require
/// at least an Oauth2 handshake. Typlically, you will want to create a new
/// client, and then run the `auth()` method, which will attempt to get
/// a read-only bearer token. From there, pulling images can be done with
/// the `pull_*` functions.
///
/// For true anonymous access, you can skip `auth()`. This is not recommended
/// unless you are sure that the remote registry does not require Oauth2.
#[derive(Default)]
pub struct Client {
    config: ClientConfig,
    tokens: HashMap<String, RegistryToken>,
    client: reqwest::Client,
}

/// A source that can provide a `ClientConfig`.
/// If you are using this crate in your own application, you can implement this
/// trait on your configuration type so that it can be passed to `Client::from_source`.
pub trait ClientConfigSource {
    /// Provides a `ClientConfig`.
    fn client_config(&self) -> ClientConfig;
}

impl Client {
    /// Create a new client with the supplied config
    pub fn new(config: ClientConfig) -> Self {
        Self {
            config,
            tokens: HashMap::new(),
            client: reqwest::Client::new(),
        }
    }

    /// Create a new client with the supplied config
    pub fn from_source(config_source: &impl ClientConfigSource) -> Self {
        Self::new(config_source.client_config())
    }

    /// Pull an image and return the bytes
    ///
    /// The client will check if it's already been authenticated and if
    /// not will attempt to do.
    pub async fn pull_image(
        &mut self,
        image: &Reference,
        auth: &RegistryAuth,
    ) -> anyhow::Result<ImageData> {
        debug!("Pulling image: {:?}", image);

        if !self.tokens.contains_key(image.registry()) {
            self.auth(image, auth).await?;
        }

        let (manifest, digest) = self.pull_manifest(image).await?;

        let layers = manifest.layers.into_iter().map(|layer| {
            // This avoids moving `self` which is &mut Self
            // into the async block. We only want to capture
            // as &Self
            let this = &self;
            async move {
                let mut out: Vec<u8> = Vec::new();
                debug!("Pulling image layer");
                this.pull_layer(image, &layer.digest, &mut out).await?;
                Ok::<_, anyhow::Error>(out)
            }
        });

        let layers = future::try_join_all(layers).await?;
        let mut result = Vec::new();
        for layer in layers {
            // TODO: this simply overwrites previous layers with the latest one
            result = layer;
        }

        Ok(ImageData {
            content: result,
            digest: Some(digest),
        })
    }

    /// Perform an OAuth v2 auth request if necessary.
    ///
    /// This performs authorization and then stores the token internally to be used
    /// on other requests.
    async fn auth(
        &mut self,
        image: &Reference,
        authentication: &RegistryAuth,
    ) -> anyhow::Result<()> {
        debug!("Authorzing for image: {:?}", image);
        // The version request will tell us where to go.
        let url = format!(
            "{}://{}/v2/",
            self.config.protocol.scheme_for(image.registry()),
            image.registry()
        );
        let res = self.client.get(&url).send().await?;
        let dist_hdr = match res.headers().get(reqwest::header::WWW_AUTHENTICATE) {
            Some(h) => h,
            None => return Ok(()),
        };

        let auth = WwwAuthenticate::parse_header(&dist_hdr.as_bytes().into())?;
        // If challenge_opt is not set it means that no challenge was present, even though the header
        // was present. Since we do not handle basic auth, it could be the case that the upstream service
        // is in compatibility mode with a Docker v1 registry.
        let challenge_opt = match auth.get::<BearerChallenge>() {
            Some(co) => co,
            None => return Ok(()),
        };

        // Right now, we do read-only auth.
        let pull_perms = format!("repository:{}:pull", image.repository());
        let challenge = &challenge_opt[0];
        let realm = challenge.realm.as_ref().unwrap();
        let service = challenge.service.as_ref().unwrap();

        // TODO: At some point in the future, we should support sending a secret to the
        // server for auth. This particular workflow is for read-only public auth.
        debug!("Making authentication call to {}", realm);
        let auth_res = self
            .client
            .get(realm)
            .query(&[("service", service), ("scope", &pull_perms)])
            .apply_authentication(authentication)
            .send()
            .await?;

        match auth_res.status() {
            reqwest::StatusCode::OK => {
                let text = auth_res.text().await?;
                debug!("Received response from auth request: {}", text);
                let token: RegistryToken = serde_json::from_str(&text)
                    .context("Failed to decode registry token from auth request")?;
                debug!("Succesfully authorized for image '{:?}'", image);
                self.tokens.insert(image.registry().to_owned(), token);
                Ok(())
            }
            _ => {
                let reason = auth_res.text().await?;
                debug!("Failed to authenticate for image '{:?}': {}", image, reason);
                Err(anyhow::anyhow!("failed to authenticate: {}", reason))
            }
        }
    }

    /// Fetch a manifest's digest from the remote OCI Distribution service.
    ///
    /// If the connection has already gone through authentication, this will
    /// use the bearer token. Otherwise, this will attempt an anonymous pull.
    pub async fn fetch_manifest_digest(
        &mut self,
        image: &Reference,
        auth: &RegistryAuth,
    ) -> anyhow::Result<String> {
        if !self.tokens.contains_key(image.registry()) {
            self.auth(image, auth).await?;
        }

        let url = self.to_v2_manifest_url(image);
        debug!("Pulling image manifest from {}", url);
        let request = self.client.get(&url);

        let res = request.headers(self.auth_headers(image)).send().await?;

        // The OCI spec technically does not allow any codes but 200, 500, 401, and 404.
        // Obviously, HTTP servers are going to send other codes. This tries to catch the
        // obvious ones (200, 4XX, 5XX). Anything else is just treated as an error.
        match res.status() {
            reqwest::StatusCode::OK => digest_header_value(&res),
            s if s.is_client_error() => {
                // According to the OCI spec, we should see an error in the message body.
                let err = res.json::<OciEnvelope>().await?;
                // FIXME: This should not have to wrap the error.
                Err(anyhow::anyhow!("{} on {}", err.errors[0], url))
            }
            s if s.is_server_error() => Err(anyhow::anyhow!("Server error at {}", url)),
            s => Err(anyhow::anyhow!(
                "An unexpected error occured: code={}, message='{}'",
                s,
                res.text().await?
            )),
        }
    }

    /// Pull a manifest from the remote OCI Distribution service.
    ///
    /// If the connection has already gone through authentication, this will
    /// use the bearer token. Otherwise, this will attempt an anonymous pull.
    async fn pull_manifest(&self, image: &Reference) -> anyhow::Result<(OciManifest, String)> {
        let url = self.to_v2_manifest_url(image);
        debug!("Pulling image manifest from {}", url);
        let request = self.client.get(&url);

        let res = request.headers(self.auth_headers(image)).send().await?;

        // The OCI spec technically does not allow any codes but 200, 500, 401, and 404.
        // Obviously, HTTP servers are going to send other codes. This tries to catch the
        // obvious ones (200, 4XX, 5XX). Anything else is just treated as an error.
        match res.status() {
            reqwest::StatusCode::OK => {
                let digest = digest_header_value(&res)?;
                let text = res.text().await?;
                debug!("Parsing response as OciManifest: {}", text);
                let manifest = serde_json::from_str(&text).with_context(|| {
                    format!(
                        "Failed to parse response from pulling manifest for '{:?}' as an OciManifest",
                        image
                    )
                })?;
                Ok((manifest, digest))
            }
            s if s.is_client_error() => {
                // According to the OCI spec, we should see an error in the message body.
                let err = res.json::<OciEnvelope>().await?;
                // FIXME: This should not have to wrap the error.
                Err(anyhow::anyhow!("{} on {}", err.errors[0], url))
            }
            s if s.is_server_error() => Err(anyhow::anyhow!("Server error at {}", url)),
            s => Err(anyhow::anyhow!(
                "An unexpected error occured: code={}, message='{}'",
                s,
                res.text().await?
            )),
        }
    }

    /// Pull a single layer from an OCI registy.
    ///
    /// This pulls the layer for a particular image that is identified by
    /// the given digest. The image reference is used to find the
    /// repository and the registry, but it is not used to verify that
    /// the digest is a layer inside of the image. (The manifest is
    /// used for that.)
    async fn pull_layer<T: AsyncWrite + Unpin>(
        &self,
        image: &Reference,
        digest: &str,
        mut out: T,
    ) -> anyhow::Result<()> {
        let url = self.to_v2_blob_url(image.registry(), image.repository(), digest);
        let mut stream = self
            .client
            .get(&url)
            .headers(self.auth_headers(image))
            .send()
            .await?
            .bytes_stream();

        while let Some(bytes) = stream.next().await {
            out.write_all(&bytes?).await?;
        }

        Ok(())
    }

    /// Convert a Reference to a v2 manifest URL.
    fn to_v2_manifest_url(&self, reference: &Reference) -> String {
        if let Some(digest) = reference.digest() {
            format!(
                "{}://{}/v2/{}/manifests/{}",
                self.config.protocol.scheme_for(reference.registry()),
                reference.registry(),
                reference.repository(),
                digest,
            )
        } else {
            format!(
                "{}://{}/v2/{}/manifests/{}",
                self.config.protocol.scheme_for(reference.registry()),
                reference.registry(),
                reference.repository(),
                reference.tag().unwrap_or("latest")
            )
        }
    }

    /// Convert a Reference to a v2 blob (layer) URL.
    fn to_v2_blob_url(&self, registry: &str, repository: &str, digest: &str) -> String {
        format!(
            "{}://{}/v2/{}/blobs/{}",
            self.config.protocol.scheme_for(registry),
            registry,
            repository,
            digest,
        )
    }

    /// Generate the headers necessary for authentication.
    ///
    /// If the struct has Some(bearer), this will insert the bearer token in an
    /// Authorization header. It will also set the Accept header, which must
    /// be set on all OCI Registry request.
    fn auth_headers(&self, image: &Reference) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("Accept", "application/vnd.docker.distribution.manifest.v2+json,application/vnd.docker.distribution.manifest.list.v2+json,application/vnd.oci.image.manifest.v1+json".parse().unwrap());

        if let Some(token) = self.tokens.get(image.registry()) {
            headers.insert("Authorization", token.bearer_token().parse().unwrap());
        }
        headers
    }
}

/// A client configuration
#[derive(Debug, Clone, Default)]
pub struct ClientConfig {
    /// Which protocol the client should use
    pub protocol: ClientProtocol,
}

/// The protocol that the client should use to connect
#[derive(Debug, Clone, PartialEq)]
pub enum ClientProtocol {
    #[allow(missing_docs)]
    Http,
    #[allow(missing_docs)]
    Https,
    #[allow(missing_docs)]
    HttpsExcept(Vec<String>),
}

impl Default for ClientProtocol {
    fn default() -> Self {
        ClientProtocol::Https
    }
}

impl ClientProtocol {
    fn scheme_for(&self, registry: &str) -> &str {
        match self {
            ClientProtocol::Https => "https",
            ClientProtocol::Http => "http",
            ClientProtocol::HttpsExcept(exceptions) => {
                if exceptions.contains(&registry.to_owned()) {
                    "http"
                } else {
                    "https"
                }
            }
        }
    }
}

/// A token granted during the OAuth2-like workflow for OCI registries.
#[derive(serde::Deserialize, Default)]
struct RegistryToken {
    #[serde(alias = "access_token")]
    token: String,
}

impl RegistryToken {
    fn bearer_token(&self) -> String {
        format!("Bearer {}", self.token)
    }
}

#[derive(Clone)]
struct BearerChallenge {
    pub realm: Option<String>,
    pub service: Option<String>,
    pub scope: Option<String>,
}

impl Challenge for BearerChallenge {
    fn challenge_name() -> &'static str {
        "Bearer"
    }

    fn from_raw(raw: RawChallenge) -> Option<Self> {
        match raw {
            RawChallenge::Token68(_) => None,
            RawChallenge::Fields(mut map) => Some(BearerChallenge {
                realm: map.remove("realm"),
                scope: map.remove("scope"),
                service: map.remove("service"),
            }),
        }
    }

    fn into_raw(self) -> RawChallenge {
        let mut map = ChallengeFields::new();
        if let Some(realm) = self.realm {
            map.insert_static_quoting("realm", realm);
        }
        if let Some(scope) = self.scope {
            map.insert_static_quoting("scope", scope);
        }
        if let Some(service) = self.service {
            map.insert_static_quoting("service", service);
        }
        RawChallenge::Fields(map)
    }
}

fn digest_header_value(response: &reqwest::Response) -> anyhow::Result<String> {
    let headers = response.headers();
    let digest_header = headers.get("Docker-Content-Digest");
    match digest_header {
        None => Err(anyhow::anyhow!("resgistry did not return a digest header")),
        Some(hv) => hv
            .to_str()
            .map(|s| s.to_string())
            .map_err(anyhow::Error::new),
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::convert::TryFrom;

    const HELLO_IMAGE_NO_TAG: &str = "webassembly.azurecr.io/hello-wasm";
    const HELLO_IMAGE_TAG: &str = "webassembly.azurecr.io/hello-wasm:v1";
    const HELLO_IMAGE_DIGEST: &str = "webassembly.azurecr.io/hello-wasm@sha256:51d9b231d5129e3ffc267c9d455c49d789bf3167b611a07ab6e4b3304c96b0e7";
    const HELLO_IMAGE_TAG_AND_DIGEST: &str = "webassembly.azurecr.io/hello-wasm:v1@sha256:51d9b231d5129e3ffc267c9d455c49d789bf3167b611a07ab6e4b3304c96b0e7";
    const TEST_IMAGES: &'static [&str] = &[
        // TODO(jlegrone): this image cannot be pulled currently because no `latest`
        //                 tag exists on the image repository. Re-enable this image
        //                 in tests once `latest` is published.
        // HELLO_IMAGE_NO_TAG,
        HELLO_IMAGE_TAG,
        HELLO_IMAGE_DIGEST,
        HELLO_IMAGE_TAG_AND_DIGEST,
    ];

    #[test]
    fn test_to_v2_blob_url() {
        let image = Reference::try_from(HELLO_IMAGE_TAG).expect("failed to parse reference");
        let blob_url = Client::default().to_v2_blob_url(
            image.registry(),
            image.repository(),
            "sha256:deadbeef",
        );
        assert_eq!(
            blob_url,
            "https://webassembly.azurecr.io/v2/hello-wasm/blobs/sha256:deadbeef"
        )
    }

    #[test]
    fn test_to_v2_manifest() {
        let c = Client::default();

        for &(image, expected_uri) in [
            (HELLO_IMAGE_NO_TAG, "https://webassembly.azurecr.io/v2/hello-wasm/manifests/latest"), // TODO: confirm this is the right translation when no tag
            (HELLO_IMAGE_TAG, "https://webassembly.azurecr.io/v2/hello-wasm/manifests/v1"),
            (HELLO_IMAGE_DIGEST, "https://webassembly.azurecr.io/v2/hello-wasm/manifests/sha256:51d9b231d5129e3ffc267c9d455c49d789bf3167b611a07ab6e4b3304c96b0e7"),
            (HELLO_IMAGE_TAG_AND_DIGEST, "https://webassembly.azurecr.io/v2/hello-wasm/manifests/sha256:51d9b231d5129e3ffc267c9d455c49d789bf3167b611a07ab6e4b3304c96b0e7"),
            ].iter() {
                let reference = Reference::try_from(image).expect("failed to parse reference");
                assert_eq!(c.to_v2_manifest_url(&reference), expected_uri);
        }
    }

    #[test]
    fn manifest_url_generation_respects_http_protocol() {
        let c = Client::new(ClientConfig {
            protocol: ClientProtocol::Http,
        });
        let reference = Reference::try_from("webassembly.azurecr.io/hello:v1".to_owned())
            .expect("Could not parse reference");
        assert_eq!(
            "http://webassembly.azurecr.io/v2/hello/manifests/v1",
            c.to_v2_manifest_url(&reference)
        );
    }

    #[test]
    fn blob_url_generation_respects_http_protocol() {
        let c = Client::new(ClientConfig {
            protocol: ClientProtocol::Http,
        });
        let reference = Reference::try_from("webassembly.azurecr.io/hello@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned())
            .expect("Could not parse reference");
        assert_eq!(
            "http://webassembly.azurecr.io/v2/hello/blobs/sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            c.to_v2_blob_url(
                &reference.registry(),
                reference.repository(),
                reference.digest().unwrap()
            )
        );
    }

    #[test]
    fn manifest_url_generation_uses_https_if_not_on_exception_list() {
        let insecure_registries = vec!["localhost".to_owned(), "oci.registry.local".to_owned()];
        let protocol = ClientProtocol::HttpsExcept(insecure_registries);
        let c = Client::new(ClientConfig { protocol });
        let reference = Reference::try_from("webassembly.azurecr.io/hello:v1".to_owned())
            .expect("Could not parse reference");
        assert_eq!(
            "https://webassembly.azurecr.io/v2/hello/manifests/v1",
            c.to_v2_manifest_url(&reference)
        );
    }

    #[test]
    fn manifest_url_generation_uses_http_if_on_exception_list() {
        let insecure_registries = vec!["localhost".to_owned(), "oci.registry.local".to_owned()];
        let protocol = ClientProtocol::HttpsExcept(insecure_registries);
        let c = Client::new(ClientConfig { protocol });
        let reference = Reference::try_from("oci.registry.local/hello:v1".to_owned())
            .expect("Could not parse reference");
        assert_eq!(
            "http://oci.registry.local/v2/hello/manifests/v1",
            c.to_v2_manifest_url(&reference)
        );
    }

    #[test]
    fn blob_url_generation_uses_https_if_not_on_exception_list() {
        let insecure_registries = vec!["localhost".to_owned(), "oci.registry.local".to_owned()];
        let protocol = ClientProtocol::HttpsExcept(insecure_registries);
        let c = Client::new(ClientConfig { protocol });
        let reference = Reference::try_from("webassembly.azurecr.io/hello@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned())
            .expect("Could not parse reference");
        assert_eq!(
            "https://webassembly.azurecr.io/v2/hello/blobs/sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            c.to_v2_blob_url(
                &reference.registry(),
                reference.repository(),
                reference.digest().unwrap()
            )
        );
    }

    #[test]
    fn blob_url_generation_uses_http_if_on_exception_list() {
        let insecure_registries = vec!["localhost".to_owned(), "oci.registry.local".to_owned()];
        let protocol = ClientProtocol::HttpsExcept(insecure_registries);
        let c = Client::new(ClientConfig { protocol });
        let reference = Reference::try_from("oci.registry.local/hello@sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff".to_owned())
            .expect("Could not parse reference");
        assert_eq!(
            "http://oci.registry.local/v2/hello/blobs/sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
            c.to_v2_blob_url(
                &reference.registry(),
                reference.repository(),
                reference.digest().unwrap()
            )
        );
    }

    #[tokio::test]
    async fn test_auth() {
        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");
            let mut c = Client::default();
            c.auth(&reference, &RegistryAuth::Anonymous)
                .await
                .expect("result from auth request");

            let tok = c
                .tokens
                .get(reference.registry())
                .expect("token is available");
            // We test that the token is longer than a minimal hash.
            assert!(tok.token.len() > 64);
        }
    }

    #[tokio::test]
    async fn test_pull_manifest() {
        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");
            // Currently, pull_manifest does not perform Authz, so this will fail.
            let c = Client::default();
            c.pull_manifest(&reference)
                .await
                .expect_err("pull manifest should fail");

            // But this should pass
            let mut c = Client::default();
            c.auth(&reference, &RegistryAuth::Anonymous)
                .await
                .expect("authenticated");
            let (manifest, _) = c
                .pull_manifest(&reference)
                .await
                .expect("pull manifest should not fail");

            // The test on the manifest checks all fields. This is just a brief sanity check.
            assert_eq!(manifest.schema_version, 2);
            assert!(!manifest.layers.is_empty());
        }
    }

    #[tokio::test]
    async fn test_fetch_digest() {
        let mut c = Client::default();

        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");
            c.fetch_manifest_digest(&reference, &RegistryAuth::Anonymous)
                .await
                .expect("pull manifest should not fail");

            // This should pass
            let reference = Reference::try_from(image).expect("failed to parse reference");
            let mut c = Client::default();
            c.auth(&reference, &RegistryAuth::Anonymous)
                .await
                .expect("authenticated");
            let digest = c
                .fetch_manifest_digest(&reference, &RegistryAuth::Anonymous)
                .await
                .expect("pull manifest should not fail");

            assert_eq!(
                digest,
                "sha256:51d9b231d5129e3ffc267c9d455c49d789bf3167b611a07ab6e4b3304c96b0e7"
            );
        }
    }

    #[tokio::test]
    async fn test_pull_layer() {
        let mut c = Client::default();

        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");
            c.auth(&reference, &RegistryAuth::Anonymous)
                .await
                .expect("authenticated");
            let (manifest, _) = c
                .pull_manifest(&reference)
                .await
                .expect("failed to pull manifest");

            // Pull one specific layer
            let mut file: Vec<u8> = Vec::new();
            let layer0 = &manifest.layers[0];

            c.pull_layer(&reference, &layer0.digest, &mut file)
                .await
                .expect("Pull layer into vec");

            // The manifest says how many bytes we should expect.
            assert_eq!(file.len(), layer0.size as usize);
        }
    }

    #[tokio::test]
    async fn test_pull_image() {
        for &image in TEST_IMAGES {
            let reference = Reference::try_from(image).expect("failed to parse reference");

            let image_data = Client::default()
                .pull_image(&reference, &RegistryAuth::Anonymous)
                .await
                .expect("failed to pull manifest");

            assert!(!image_data.content.is_empty());
            assert!(image_data.digest.is_some());
        }
    }
}
