//! Pushing and pulling ocicas artifacts against an OCI registry.
//!
//! A [`Store`] wraps an oci-client [`Client`] plus a `reqwest` client for the
//! transparent-zstd blob push (which needs a per-request `Content-Encoding` that
//! oci-client cannot set). An optional local chunk CAS makes repeated pulls
//! incremental: only chunks absent from the cache directory are fetched.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use oci_client::client::{Certificate, CertificateEncoding, ClientConfig, ClientProtocol};
use oci_client::errors::OciDistributionError;
use oci_client::manifest::{OciDescriptor, OciImageManifest, OciManifest};
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinSet};

use crate::cas::{assemble, build, digest_hex, sha256_hex};
use crate::error::{Error, Result};
use crate::index::{
    ARTIFACT_TYPE, ChunkRef, Index, MEDIA_TYPE_CHUNK, MEDIA_TYPE_INDEX, unmarshal_index,
};

/// Bounds parallel chunk transfers (pull fetches and push uploads).
const FETCH_CONCURRENCY: usize = 8;

/// Connect timeout for every registry request. Without it a cache host whose
/// packets are dropped (e.g. a blocked egress route) stalls on the SYN for the
/// OS default (~2 min) per request. The cache is best-effort — the caller falls
/// back to running the task uncached — so an unreachable registry must fail fast.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Per-read inactivity timeout, so a transfer that stalls mid-stream aborts
/// instead of hanging (reset on each chunk received, not a cap on total time).
const READ_TIMEOUT: Duration = Duration::from_secs(30);

const IMAGE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";
const EMPTY_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.empty.v1+json";

/// Capability header a cooperating vk-registry sets on its `GET /v2/` probe, so
/// an auto-mode client knows it may push transparent-zstd chunks.
const TRANSPARENT_ZSTD_HEADER: &str = "x-virtkit-transparent-zstd";

/// Configures [`Store::open`].
#[derive(Debug, Default, Clone)]
pub struct RemoteOptions {
    pub username: String,
    pub password: String,
    /// Extra trust anchor (a self-signed corp registry).
    pub ca_file: Option<PathBuf>,
    /// Local chunk CAS directory (`None` = fetch into memory).
    pub cache_dir: Option<PathBuf>,
    /// Talk to the registry without TLS (local development).
    pub plain_http: bool,
}

/// Counts of the uploads performed and skipped (already present) and the bytes
/// actually sent — the deduplication, observable per push.
#[derive(Debug, Default, Clone, Copy)]
pub struct PushStats {
    pub pushed: i64,
    pub skipped: i64,
    pub bytes: i64,
}

#[derive(Default)]
struct Counters {
    pushed: AtomicI64,
    skipped: AtomicI64,
    bytes: AtomicI64,
}

/// An OCI registry store for ocicas artifacts.
pub struct Store {
    client: Client,
    http: reqwest::Client,
    auth: RegistryAuth,
    /// Basic credentials for the raw transparent-zstd push (`None` = anonymous).
    basic: Option<(String, String)>,
    /// `host/repo` reference base; a per-tag [`Reference`] is built from it.
    base: String,
    registry: String,
    repository: String,
    scheme: &'static str,
    cache_dir: Option<PathBuf>,
    transparent: bool,
}

impl Store {
    /// Open `host/repo` as a store, probing the registry for the transparent-zstd
    /// capability.
    pub async fn open(reference: &str, opts: RemoteOptions) -> Result<Self> {
        let parsed: Reference = reference
            .parse()
            .map_err(|e| Error::format(format!("parsing OCI reference {reference:?}: {e}")))?;
        let registry = parsed.resolve_registry().to_string();
        let repository = parsed.repository().to_string();

        let mut cfg = ClientConfig::default();
        if let Some(ca) = &opts.ca_file {
            let pem = std::fs::read(ca)?;
            cfg.extra_root_certificates.push(Certificate {
                encoding: CertificateEncoding::Pem,
                data: pem,
            });
        }
        if opts.plain_http {
            cfg.protocol = ClientProtocol::Http;
        }
        cfg.connect_timeout = Some(CONNECT_TIMEOUT);
        cfg.read_timeout = Some(READ_TIMEOUT);
        let client = Client::new(cfg);

        let mut builder = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT);
        if let Some(ca) = &opts.ca_file {
            let pem = std::fs::read(ca)?;
            builder = builder
                .add_root_certificate(reqwest::Certificate::from_pem(&pem).map_err(req_err)?);
        }
        let http = builder.build().map_err(req_err)?;

        let (auth, basic) = if opts.username.is_empty() {
            (RegistryAuth::Anonymous, None)
        } else {
            (
                RegistryAuth::Basic(opts.username.clone(), opts.password.clone()),
                Some((opts.username.clone(), opts.password.clone())),
            )
        };

        let scheme = if opts.plain_http { "http" } else { "https" };
        let transparent = detect_transparent(&http, scheme, &registry).await?;

        Ok(Store {
            client,
            http,
            auth,
            basic,
            base: reference.to_string(),
            registry,
            repository,
            scheme,
            cache_dir: opts.cache_dir,
            transparent,
        })
    }

    /// Whether uploads use the transparent-zstd scheme. [`build`] must be told,
    /// so the index records it and restore knows not to decompress.
    pub fn transparent(&self) -> bool {
        self.transparent
    }

    fn reference(&self, tag: &str) -> Result<Reference> {
        format!("{}:{tag}", self.base)
            .parse()
            .map_err(|e| Error::format(format!("parsing OCI reference: {e}")))
    }

    /// Fetch just the manifest annotations of a tag, or `None` if the tag does
    /// not exist — the cheap "is this entry already pushed and matching" check.
    pub async fn resolve_annotations(&self, tag: &str) -> Result<Option<BTreeMap<String, String>>> {
        let image = self.reference(tag)?;
        match self.client.pull_manifest(&image, &self.auth).await {
            Ok((OciManifest::Image(m), _)) => {
                if m.artifact_type.as_deref() != Some(ARTIFACT_TYPE) {
                    return Err(Error::format(format!(
                        "{tag} is not a {ARTIFACT_TYPE} artifact"
                    )));
                }
                Ok(Some(m.annotations.unwrap_or_default()))
            }
            Ok((OciManifest::ImageIndex(_), _)) => Err(Error::format(format!(
                "{tag} is an image index, not an artifact"
            ))),
            Err(OciDistributionError::ImageManifestNotFoundError(_)) => Ok(None),
            Err(e) => Err(oci_err(e)),
        }
    }

    /// Chunk the file set under `base_dir` and push the missing chunks plus the
    /// index and manifest, tagging it `tag`. Chunks already in the registry are
    /// skipped (cross-entry dedup).
    pub async fn push(
        &self,
        tag: &str,
        base_dir: &Path,
        paths: &[String],
        annotations: BTreeMap<String, String>,
    ) -> Result<PushStats> {
        let image = self.reference(tag)?;
        self.client
            .store_auth_if_needed(image.resolve_registry(), &self.auth)
            .await;

        // Bounded channel so at most FETCH_CONCURRENCY frames are buffered beyond
        // the one being built: build (on the blocking pool) blocks on a full
        // channel, natural backpressure.
        let (tx, mut rx) = mpsc::channel::<(ChunkRef, Vec<u8>)>(FETCH_CONCURRENCY);
        let base = base_dir.to_path_buf();
        let owned_paths = paths.to_vec();
        let transparent = self.transparent;
        let build_handle = tokio::task::spawn_blocking(move || {
            let tx = tx;
            build(
                &base,
                &owned_paths,
                |refc, frame| {
                    tx.blocking_send((refc.clone(), frame.to_vec()))
                        .map_err(|_| Error::format("upload channel closed"))
                },
                transparent,
            )
        });

        let counters = Arc::new(Counters::default());
        let sem = Arc::new(Semaphore::new(FETCH_CONCURRENCY));
        let mut uploads: JoinSet<Result<()>> = JoinSet::new();
        while let Some((refc, frame)) = rx.recv().await {
            let permit = sem
                .clone()
                .acquire_owned()
                .await
                .map_err(|_| Error::format("upload semaphore closed"))?;
            let client = self.client.clone();
            let http = self.http.clone();
            let image = image.clone();
            let basic = self.basic.clone();
            let scheme = self.scheme;
            let registry = self.registry.clone();
            let repository = self.repository.clone();
            let counters = counters.clone();
            uploads.spawn(async move {
                let _permit = permit;
                upload_chunk(
                    &client,
                    &http,
                    &image,
                    transparent,
                    scheme,
                    &registry,
                    &repository,
                    &basic,
                    &refc,
                    frame,
                    &counters,
                )
                .await
            });
        }

        // build finished (tx dropped, channel closed): surface its result first,
        // then any upload error.
        let idx = build_handle.await.map_err(join_err)??;
        while let Some(res) = uploads.join_next().await {
            res.map_err(join_err)??;
        }

        self.push_index(&image, &idx, &annotations).await?;
        Ok(PushStats {
            pushed: counters.pushed.load(Ordering::Relaxed),
            skipped: counters.skipped.load(Ordering::Relaxed),
            bytes: counters.bytes.load(Ordering::Relaxed),
        })
    }

    async fn push_index(
        &self,
        image: &Reference,
        idx: &Index,
        annotations: &BTreeMap<String, String>,
    ) -> Result<()> {
        let raw = idx.marshal()?;
        let index_desc = descriptor(MEDIA_TYPE_INDEX, sha256_hex(&raw), raw.len() as i64);
        self.push_if_absent(image, &index_desc, raw).await?;

        let config = b"{}".to_vec();
        let config_desc = descriptor(
            EMPTY_CONFIG_MEDIA_TYPE,
            sha256_hex(&config),
            config.len() as i64,
        );
        self.push_if_absent(image, &config_desc, config).await?;

        let mut layers = vec![index_desc];
        let mut seen: HashSet<&str> = HashSet::new();
        for c in &idx.chunks {
            if seen.insert(c.digest.as_str()) {
                layers.push(descriptor(MEDIA_TYPE_CHUNK, c.digest.clone(), c.size));
            }
        }
        let manifest = OciImageManifest {
            schema_version: 2,
            media_type: Some(IMAGE_MANIFEST_MEDIA_TYPE.to_string()),
            config: config_desc,
            layers,
            subject: None,
            artifact_type: Some(ARTIFACT_TYPE.to_string()),
            annotations: Some(annotations.clone()),
        };
        self.client
            .push_manifest(image, &OciManifest::Image(manifest))
            .await
            .map_err(oci_err)?;
        Ok(())
    }

    async fn push_if_absent(
        &self,
        image: &Reference,
        desc: &OciDescriptor,
        data: Vec<u8>,
    ) -> Result<()> {
        if self
            .client
            .blob_exists(image, &desc.digest)
            .await
            .map_err(oci_err)?
        {
            return Ok(());
        }
        self.client
            .push_blob(image, data, &desc.digest)
            .await
            .map_err(oci_err)?;
        Ok(())
    }

    /// Resolve `tag`, fetch the missing chunks (through the local CAS when
    /// configured) and assemble the file set under `dir`. Returns the index and
    /// the manifest annotations.
    pub async fn pull(&self, tag: &str, dir: &Path) -> Result<(Index, BTreeMap<String, String>)> {
        let image = self.reference(tag)?;
        let (manifest, _digest) = self
            .client
            .pull_manifest(&image, &self.auth)
            .await
            .map_err(oci_err)?;
        let OciManifest::Image(m) = manifest else {
            return Err(Error::format(format!("{tag} is not an image manifest")));
        };
        if m.artifact_type.as_deref() != Some(ARTIFACT_TYPE) {
            return Err(Error::format(format!(
                "{tag} is not a {ARTIFACT_TYPE} artifact"
            )));
        }
        let index_desc = m
            .layers
            .iter()
            .find(|l| l.media_type == MEDIA_TYPE_INDEX)
            .ok_or_else(|| Error::format(format!("{tag} has no index layer")))?;
        let raw_idx = pull_blob_bytes(&self.client, &image, index_desc).await?;
        let idx = unmarshal_index(&raw_idx)?;

        let source = self.chunk_source(&image, &idx).await?;
        let owned_idx = idx.clone();
        let dir = dir.to_path_buf();
        tokio::task::spawn_blocking(move || assemble(&owned_idx, &dir, source))
            .await
            .map_err(join_err)??;

        let annotations = m.annotations.unwrap_or_default();
        Ok((idx, annotations))
    }

    /// Prefetch the chunks absent from the local CAS (or all chunks into memory
    /// when no cache dir) and return a synchronous source feeding [`assemble`].
    async fn chunk_source(
        &self,
        image: &Reference,
        idx: &Index,
    ) -> Result<Box<dyn FnMut(&str) -> Result<Vec<u8>> + Send>> {
        let mut want: BTreeMap<String, ChunkRef> = BTreeMap::new();
        for c in &idx.chunks {
            want.insert(c.digest.clone(), c.clone());
        }

        if let Some(dir) = self.cache_dir.clone() {
            let sem = Arc::new(Semaphore::new(FETCH_CONCURRENCY));
            let mut set: JoinSet<Result<()>> = JoinSet::new();
            for c in want.values() {
                if cas_valid(&dir, &c.digest) {
                    continue;
                }
                let permit = sem
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| Error::format("fetch semaphore closed"))?;
                let client = self.client.clone();
                let image = image.clone();
                let dir = dir.clone();
                let c = c.clone();
                set.spawn(async move {
                    let _permit = permit;
                    let data = pull_blob_bytes(
                        &client,
                        &image,
                        &descriptor(MEDIA_TYPE_CHUNK, c.digest.clone(), c.size),
                    )
                    .await?;
                    cas_write(&dir, &c.digest, &data)
                });
            }
            while let Some(res) = set.join_next().await {
                res.map_err(join_err)??;
            }
            Ok(Box::new(move |d: &str| read_cas(&dir, d)))
        } else {
            let mut mem: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            for c in want.values() {
                let data = pull_blob_bytes(
                    &self.client,
                    image,
                    &descriptor(MEDIA_TYPE_CHUNK, c.digest.clone(), c.size),
                )
                .await?;
                mem.insert(c.digest.clone(), data);
            }
            Ok(Box::new(move |d: &str| {
                mem.get(d)
                    .cloned()
                    .ok_or_else(|| Error::format(format!("chunk {d} not fetched")))
            }))
        }
    }
}

fn descriptor(media_type: &str, digest: String, size: i64) -> OciDescriptor {
    OciDescriptor {
        media_type: media_type.to_string(),
        digest,
        size,
        urls: None,
        annotations: None,
        artifact_type: None,
    }
}

async fn pull_blob_bytes(
    client: &Client,
    image: &Reference,
    desc: &OciDescriptor,
) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(desc.size.max(0) as usize);
    client
        .pull_blob(image, desc, &mut buf)
        .await
        .map_err(oci_err)?;
    Ok(buf)
}

#[allow(clippy::too_many_arguments)]
async fn upload_chunk(
    client: &Client,
    http: &reqwest::Client,
    image: &Reference,
    transparent: bool,
    scheme: &str,
    registry: &str,
    repository: &str,
    basic: &Option<(String, String)>,
    refc: &ChunkRef,
    frame: Vec<u8>,
    counters: &Counters,
) -> Result<()> {
    if client
        .blob_exists(image, &refc.digest)
        .await
        .map_err(oci_err)?
    {
        counters.skipped.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    let n = frame.len() as i64;
    if transparent {
        push_blob_zstd(
            http,
            scheme,
            registry,
            repository,
            basic,
            &refc.digest,
            frame,
        )
        .await?;
    } else {
        client
            .push_blob(image, frame, &refc.digest)
            .await
            .map_err(oci_err)?;
    }
    counters.pushed.fetch_add(1, Ordering::Relaxed);
    counters.bytes.fetch_add(n, Ordering::Relaxed);
    Ok(())
}

/// Probe `GET /v2/` for the vk-registry capability header. A reachable registry
/// that lacks the header (a plain registry, or any non-connectivity error) reads
/// as "not supported": fall back to the compressed-digest scheme every OCI
/// registry accepts. A connect/timeout failure means the registry is unreachable
/// and every later operation would fail too, so it surfaces as [`Error::Network`]
/// — letting `Store::open` fail fast and the caller warn instead of stalling.
async fn detect_transparent(http: &reqwest::Client, scheme: &str, registry: &str) -> Result<bool> {
    let url = format!("{scheme}://{registry}/v2/");
    match http.get(&url).send().await {
        Ok(resp) => Ok(resp.headers().contains_key(TRANSPARENT_ZSTD_HEADER)),
        Err(e) if e.is_connect() || e.is_timeout() => Err(req_err(e)),
        Err(_) => Ok(false),
    }
}

/// Upload an already-zstd-compressed frame keyed by the digest of its
/// uncompressed form, tagging the body `Content-Encoding: zstd`. A monolithic
/// OCI upload issued directly (oci-client cannot set the per-request encoding);
/// vk-registry stores the frame and serves canonical bytes back.
async fn push_blob_zstd(
    http: &reqwest::Client,
    scheme: &str,
    registry: &str,
    repository: &str,
    basic: &Option<(String, String)>,
    digest: &str,
    frame: Vec<u8>,
) -> Result<()> {
    let with_auth = |req: reqwest::RequestBuilder| match basic {
        Some((u, p)) => req.basic_auth(u, Some(p)),
        None => req,
    };

    let uploads = format!("{scheme}://{registry}/v2/{repository}/blobs/uploads/");
    let resp = with_auth(
        http.post(&uploads)
            .header(reqwest::header::CONTENT_LENGTH, "0"),
    )
    .send()
    .await
    .map_err(req_err)?;
    if resp.status() != reqwest::StatusCode::ACCEPTED {
        return Err(Error::format(format!(
            "begin blob upload: HTTP {}",
            resp.status()
        )));
    }
    let location = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| Error::format("upload session returned no Location"))?;
    let location = if location.starts_with('/') {
        format!("{scheme}://{registry}{location}")
    } else {
        location.to_string()
    };

    let resp = with_auth(
        http.put(location)
            .query(&[("digest", digest)])
            .header(reqwest::header::CONTENT_ENCODING, "zstd")
            .body(frame),
    )
    .send()
    .await
    .map_err(req_err)?;
    if resp.status() != reqwest::StatusCode::CREATED && resp.status() != reqwest::StatusCode::OK {
        return Err(Error::format(format!("blob PUT: HTTP {}", resp.status())));
    }
    Ok(())
}

fn cas_file(dir: &Path, digest: &str) -> Option<PathBuf> {
    digest_hex(digest).map(|hex| dir.join("sha256").join(hex))
}

fn cas_valid(dir: &Path, digest: &str) -> bool {
    cas_file(dir, digest)
        .and_then(|p| std::fs::metadata(p).ok())
        .is_some_and(|m| m.is_file())
}

/// Store a chunk atomically (tmp + rename), so concurrent pulls never observe a
/// half-written entry.
fn cas_write(dir: &Path, digest: &str, data: &[u8]) -> Result<()> {
    let hex = digest_hex(digest).ok_or_else(|| Error::format(format!("bad digest {digest:?}")))?;
    let subdir = dir.join("sha256");
    std::fs::create_dir_all(&subdir)?;
    let tmp = subdir.join(format!(".{hex}.tmp.{}", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, subdir.join(hex))?;
    Ok(())
}

/// Read a chunk from the local CAS, verifying its digest; a corrupted entry is
/// removed and reported so the restore fails rather than trusting bad bytes.
fn read_cas(dir: &Path, digest: &str) -> Result<Vec<u8>> {
    let path =
        cas_file(dir, digest).ok_or_else(|| Error::format(format!("bad digest {digest:?}")))?;
    let data = std::fs::read(&path)?;
    if sha256_hex(&data) != digest {
        let _ = std::fs::remove_file(&path);
        return Err(Error::format(format!("corrupt CAS entry {digest}")));
    }
    Ok(data)
}

fn oci_err(e: OciDistributionError) -> Error {
    if let OciDistributionError::RequestError(re) = &e
        && (re.is_connect() || re.is_timeout())
    {
        return Error::network(format!("oci: {e}"));
    }
    Error::format(format!("oci: {e}"))
}

fn req_err(e: reqwest::Error) -> Error {
    if e.is_connect() || e.is_timeout() {
        return Error::network(format!("http: {e}"));
    }
    Error::format(format!("http: {e}"))
}

fn join_err(e: JoinError) -> Error {
    Error::format(format!("task join: {e}"))
}
