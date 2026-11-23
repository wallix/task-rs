//! Pushing and pulling ocicas artifacts against an OCI registry.
//!
//! A [`Store`] wraps an oci-client [`Client`] plus a `reqwest` client for the
//! transparent-zstd chunk transfers (which need a per-request `Content-Encoding`
//! or `Accept-Encoding` that oci-client cannot set). An optional local chunk CAS
//! makes repeated pulls incremental: only chunks absent from the cache directory
//! are fetched. It keeps each chunk as it arrived ([`Codec`]).

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use oci_client::client::{Certificate, CertificateEncoding, ClientConfig, ClientProtocol};
use oci_client::errors::{OciDistributionError, OciErrorCode};
use oci_client::manifest::{OciDescriptor, OciImageManifest, OciManifest};
use oci_client::{Client, Reference};
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinSet};

use crate::auth::Auth;
use crate::cas::{assemble, build_unless, digest_hex, sha256_hex};
use crate::error::{Error, Result, with_causes};
use crate::index::{
    ARTIFACT_TYPE, ChunkRef, Index, MAX_CHUNK_BLOB, MAX_INDEX_BLOB, MEDIA_TYPE_CHUNK,
    MEDIA_TYPE_INDEX, unmarshal_index, zstd_decode_chunk_into,
};
use crate::restore::{self, Written};
use crate::retry::{is_transport, with_retry};

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

/// The header a registry states a served blob's digest in.
const DOCKER_CONTENT_DIGEST: &str = "Docker-Content-Digest";

/// Configures [`Store::open`].
#[derive(Debug, Default, Clone)]
pub struct RemoteOptions {
    /// HTTP Basic credentials; an empty `username` disables them.
    pub username: String,
    pub password: String,
    /// Static bearer token (a vk-registry API key), preferred over Basic.
    pub token: String,
    /// Extra trust anchor (a self-signed corp registry).
    pub ca_file: Option<PathBuf>,
    /// Local chunk CAS directory (`None` = fetch into memory).
    pub cache_dir: Option<PathBuf>,
    /// Talk to the registry without TLS (local development).
    pub plain_http: bool,
    /// Require a successful probe advertising the transparent-zstd scheme:
    /// [`Store::open`] fails without one. When unset, the scheme is detected
    /// and a registry without it gets compressed-digest chunks.
    pub require_transparent: bool,
}

/// A resolved entry whose index has been fetched and validated, but whose
/// chunks have not: see [`Store::pull_index`].
pub struct Pulled {
    image: Reference,
    /// The validated index: the file list a restore would write.
    pub index: Index,
    /// The manifest annotations.
    pub annotations: BTreeMap<String, String>,
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
    /// Shared by `oci-client` requests and raw transparent-zstd transfers.
    auth: Auth,
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
    /// capability. With [`RemoteOptions::require_transparent`], a probe that
    /// does not establish it is an error.
    pub async fn open(reference: &str, opts: RemoteOptions) -> Result<Self> {
        let parsed: Reference = reference
            .parse()
            .map_err(|e| Error::format(format!("parsing OCI reference {reference:?}: {e}")))?;
        let registry = parsed.resolve_registry().to_string();
        let repository = parsed.repository().to_string();

        // One read for both clients: `oci-client` takes the PEM bytes, the
        // plain `reqwest` client the parsed certificates.
        let ca = opts
            .ca_file
            .as_deref()
            .map(crate::tls::ca_roots)
            .transpose()?;

        let (ca_pem, ca_certs) = match ca {
            Some(ca) => (Some(ca.pem), ca.certs),
            None => (None, Vec::new()),
        };

        let mut cfg = ClientConfig::default();
        if let Some(pem) = ca_pem {
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

        let mut builder = raw_client()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT);
        for root in ca_certs {
            builder = builder.add_root_certificate(root);
        }
        let http = builder.build().map_err(req_err)?;

        let auth = if !opts.token.is_empty() {
            Auth::Bearer {
                token: opts.token.clone(),
            }
        } else if opts.username.is_empty() {
            Auth::None
        } else {
            Auth::Basic {
                user: opts.username.clone(),
                pass: opts.password.clone(),
            }
        };

        let scheme = if opts.plain_http { "http" } else { "https" };
        let transparent =
            detect_transparent(&http, scheme, &registry, &auth, opts.require_transparent).await?;

        Ok(Store {
            client,
            http,
            auth,
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
        let (client, image, auth) = (&self.client, &image, &self.auth.registry_auth());
        // A miss is decided on the raw error, before the retry sees it as final.
        let manifest = with_retry(|| async move {
            match client.pull_manifest(image, auth).await {
                Ok((manifest, _)) => Ok(Some(manifest)),
                Err(e) if is_missing(&e) => Ok(None),
                Err(e) => Err(oci_err(e)),
            }
        })
        .await?;
        match manifest {
            Some(OciManifest::Image(m)) => {
                if m.artifact_type.as_deref() != Some(ARTIFACT_TYPE) {
                    return Err(Error::format(format!(
                        "{tag} is not a {ARTIFACT_TYPE} artifact"
                    )));
                }
                Ok(Some(m.annotations.unwrap_or_default()))
            }
            Some(OciManifest::ImageIndex(_)) => Err(Error::format(format!(
                "{tag} is an image index, not an artifact"
            ))),
            None => Ok(None),
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
            .store_auth_if_needed(image.resolve_registry(), &self.auth.registry_auth())
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
            build_unless(
                &base,
                &owned_paths,
                |refc, frame| {
                    tx.blocking_send((refc.clone(), frame.to_vec()))
                        .map_err(|_| Error::format("upload channel closed"))
                },
                transparent,
                // The push gave up and dropped its end.
                || tx.is_closed(),
            )
        });

        let counters = Arc::new(Counters::default());
        let mut uploads: JoinSet<Result<()>> = JoinSet::new();
        let queued: Result<()> = async {
            loop {
                // The builder can go a long while without a frame (it emits a
                // chunk once, however often the stream repeats it), so the
                // uploads are watched while waiting for one, not only when a
                // slot is needed.
                let (refc, frame) = tokio::select! {
                    biased;
                    Some(res) = uploads.join_next(), if !uploads.is_empty() => {
                        res.map_err(join_err)??;
                        continue;
                    }
                    next = rx.recv() => match next {
                        Some(next) => next,
                        None => return Ok(()),
                    },
                };
                next_slot(&mut uploads).await?;
                let client = self.client.clone();
                let http = self.http.clone();
                let image = image.clone();
                let auth = self.auth.clone();
                let scheme = self.scheme;
                let registry = self.registry.clone();
                let repository = self.repository.clone();
                let counters = counters.clone();
                uploads.spawn(async move {
                    upload_chunk(
                        &client,
                        &http,
                        &image,
                        transparent,
                        scheme,
                        &registry,
                        &repository,
                        &auth,
                        &refc,
                        frame,
                        &counters,
                    )
                    .await
                });
            }
        }
        .await;
        if let Err(e) = queued {
            // An upload failed: abandon the others, and tell the builder, which
            // sees the closed channel before its next chunk. It is waited for,
            // so no thread is still reading the files once the push has
            // returned; its own error is only that it was stopped.
            drop(uploads);
            drop(rx);
            let _stopped = join_builder(build_handle.await);
            return Err(e);
        }

        // build finished (tx dropped, channel closed): surface its result first,
        // then any upload error.
        let idx = join_builder(build_handle.await)?;
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
        let (client, manifest) = (&self.client, &OciManifest::Image(manifest));
        with_retry(|| async move { client.push_manifest(image, manifest).await.map_err(oci_err) })
            .await?;
        Ok(())
    }

    async fn push_if_absent(
        &self,
        image: &Reference,
        desc: &OciDescriptor,
        data: Vec<u8>,
    ) -> Result<()> {
        if blob_exists(&self.client, image, &desc.digest).await? {
            return Ok(());
        }
        push_blob(&self.client, image, Bytes::from(data), &desc.digest).await
    }

    /// Resolve `tag`, fetch the missing chunks (through the local CAS when
    /// configured) and assemble the file set under `dir`. Returns the index and
    /// the manifest annotations. [`Store::pull_index`] and
    /// [`Store::assemble_pulled`] split it, so a caller can vet the file list
    /// and annotations before anything is fetched or written: this applies no
    /// policy on which paths the entry may write (see [`assemble`]).
    pub async fn pull(&self, tag: &str, dir: &Path) -> Result<(Index, BTreeMap<String, String>)> {
        let pulled = self.pull_index(tag).await?;
        self.assemble_pulled(&pulled, dir, None)
            .await?
            .commit_or_undo()?;
        Ok((pulled.index, pulled.annotations))
    }

    /// Resolve `tag` and fetch its validated index and manifest annotations,
    /// without fetching a chunk or writing a file.
    pub async fn pull_index(&self, tag: &str) -> Result<Pulled> {
        let image = self.reference(tag)?;
        let (client, image, auth) = (&self.client, &image, &self.auth.registry_auth());
        let (manifest, _digest) =
            with_retry(|| async move { client.pull_manifest(image, auth).await.map_err(oci_err) })
                .await?;
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
        let raw_idx = pull_blob_bytes(&self.client, image, index_desc, MAX_INDEX_BLOB).await?;
        let index = unmarshal_index(&raw_idx)?;
        Ok(Pulled {
            image: image.clone(),
            index,
            annotations: m.annotations.unwrap_or_default(),
        })
    }

    /// Fetch the chunks of a pulled index and assemble its file set under
    /// `dir` (see [`assemble`]). `pulled` must come from this store: its
    /// chunks are fetched from the store's repository, so one pulled from
    /// another registry or repository is an error.
    pub async fn assemble_pulled(
        &self,
        pulled: &Pulled,
        dir: &Path,
        journal_dir: Option<&Path>,
    ) -> Result<Written> {
        let source = self.chunk_source(&pulled.image, &pulled.index).await?;
        let owned_idx = pulled.index.clone();
        let dir = dir.to_path_buf();
        let journal = journal_dir.map(Path::to_path_buf);
        tokio::task::spawn_blocking(move || assemble(&owned_idx, &dir, journal.as_deref(), source))
            .await
            .map_err(join_err)?
    }

    /// Prefetch the chunks absent from the local CAS (or all chunks into memory
    /// when no cache dir) and return a synchronous source feeding [`assemble`].
    async fn chunk_source(
        &self,
        image: &Reference,
        idx: &Index,
    ) -> Result<Box<dyn FnMut(&str) -> Result<Vec<u8>> + Send>> {
        // A transparent fetch addresses the store's repository and a classic
        // one the image's: they must be the same.
        if image.resolve_registry() != self.registry || image.repository() != self.repository {
            return Err(Error::format(format!(
                "{}/{} was not pulled from this store ({}/{})",
                image.resolve_registry(),
                image.repository(),
                self.registry,
                self.repository
            )));
        }
        let mut want: BTreeMap<String, ChunkRef> = BTreeMap::new();
        for c in &idx.chunks {
            want.insert(c.digest.clone(), c.clone());
        }

        let fetcher = Arc::new(ChunkFetcher {
            client: self.client.clone(),
            http: self.http.clone(),
            auth: self.auth.clone(),
            image: image.clone(),
            blobs: format!(
                "{}://{}/v2/{}/blobs",
                self.scheme, self.registry, self.repository
            ),
            transparent: self.transparent,
        });
        if let Some(dir) = self.cache_dir.clone() {
            let mut set: JoinSet<Result<()>> = JoinSet::new();
            for c in want.values() {
                if cas_contains(&dir, &c.digest) {
                    continue;
                }
                next_slot(&mut set).await?;
                let fetcher = fetcher.clone();
                let dir = dir.clone();
                let c = c.clone();
                set.spawn(async move {
                    let fetched = fetcher.fetch(&c).await?;
                    match &fetched.frame {
                        Some(frame) => cas_write(&dir, Codec::Zstd, &c.digest, frame),
                        None => cas_write(&dir, Codec::Raw, &c.digest, &fetched.data),
                    }
                });
            }
            while let Some(res) = set.join_next().await {
                res.map_err(join_err)??;
            }
            // One buffer for every frame read back, rather than one each.
            let mut frame = Vec::new();
            Ok(Box::new(move |d: &str| read_cas(&dir, d, &mut frame)))
        } else {
            let mut mem: BTreeMap<String, Vec<u8>> = BTreeMap::new();
            for c in want.values() {
                mem.insert(c.digest.clone(), fetcher.fetch(c).await?.data);
            }
            Ok(Box::new(move |d: &str| {
                mem.get(d)
                    .cloned()
                    .ok_or_else(|| Error::format(format!("chunk {d} not fetched")))
            }))
        }
    }
}

/// The settings of the `reqwest` client behind the raw transfers. A response
/// body is taken as sent: a blob pull reads the `Content-Encoding` itself,
/// whatever decoders `reqwest` is built with.
fn raw_client() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
}

/// What a chunk fetch needs of its [`Store`], owned so a transfer task can
/// hold it.
struct ChunkFetcher {
    client: Client,
    http: reqwest::Client,
    auth: Auth,
    image: Reference,
    /// The repository's blob endpoint, `<scheme>://<registry>/v2/<repo>/blobs`.
    blobs: String,
    transparent: bool,
}

impl ChunkFetcher {
    /// Fetch the blob `c` names, verified against its digest. A
    /// transparent-zstd registry is asked for the frame it stores; any other
    /// serves the blob as is, through `oci-client`.
    async fn fetch(&self, c: &ChunkRef) -> Result<Fetched> {
        if self.transparent {
            let url = format!("{}/{}", self.blobs, c.digest);
            let (http, auth, url) = (&self.http, &self.auth, url.as_str());
            with_retry(|| pull_blob_zstd(http, auth, url, c)).await
        } else {
            let data = pull_blob_bytes(
                &self.client,
                &self.image,
                &descriptor(MEDIA_TYPE_CHUNK, c.digest.clone(), c.size),
                MAX_CHUNK_BLOB,
            )
            .await?;
            Ok(Fetched { data, frame: None })
        }
    }
}

/// A blob pulled and verified.
#[derive(Debug)]
struct Fetched {
    /// The bytes its digest names.
    data: Vec<u8>,
    /// The zstd frame of them the registry sent, when it sent one.
    frame: Option<Vec<u8>>,
}

/// The most bytes the blob `c` names can hold: the size its descriptor states,
/// which bounds the decoding of a frame of it.
fn blob_size(c: &ChunkRef) -> usize {
    usize::try_from(c.size).unwrap_or(0).min(MAX_CHUNK_BLOB)
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

/// Pull a blob fully into memory; `oci-client` verifies it against the descriptor's
/// digest while streaming.
async fn pull_blob_bytes(
    client: &Client,
    image: &Reference,
    desc: &OciDescriptor,
    max: usize,
) -> Result<Vec<u8>> {
    with_retry(|| async move {
        // Grown as bytes arrive, never sized from the descriptor: its size is
        // registry metadata, and an index's chunk sizes are not yet trusted.
        let mut buf = Vec::new();
        client
            .pull_blob(image, desc, CappedBuf { buf: &mut buf, max })
            .await
            .map_err(oci_err)?;
        Ok(buf)
    })
    .await
}

/// An in-memory blob sink that fails once more than `max` bytes arrive, so a
/// registry cannot make a pull hold more than that.
struct CappedBuf<'a> {
    buf: &'a mut Vec<u8>,
    max: usize,
}

impl tokio::io::AsyncWrite for CappedBuf<'_> {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if this.buf.len().saturating_add(data.len()) > this.max {
            return std::task::Poll::Ready(Err(std::io::Error::other(format!(
                "blob is larger than {} bytes",
                this.max
            ))));
        }
        this.buf.extend_from_slice(data);
        std::task::Poll::Ready(Ok(data.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

async fn blob_exists(client: &Client, image: &Reference, digest: &str) -> Result<bool> {
    with_retry(|| async move { client.blob_exists(image, digest).await.map_err(oci_err) }).await
}

/// Upload `data` under `digest` through `oci-client`. `Bytes`, so every attempt
/// hands the client the same buffer without copying it.
async fn push_blob(client: &Client, image: &Reference, data: Bytes, digest: &str) -> Result<()> {
    let data = &data;
    with_retry(|| async move {
        client
            .push_blob(image, data.clone(), digest)
            .await
            .map_err(oci_err)
    })
    .await?;
    Ok(())
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
    auth: &Auth,
    refc: &ChunkRef,
    frame: Vec<u8>,
    counters: &Counters,
) -> Result<()> {
    if blob_exists(client, image, &refc.digest).await? {
        counters.skipped.fetch_add(1, Ordering::Relaxed);
        return Ok(());
    }
    let n = frame.len() as i64;
    let frame = Bytes::from(frame);
    if transparent {
        // Retried as a whole: an upload session whose PUT got no answer is left
        // behind and a new one begun, rather than the PUT repeated into a session
        // the registry may have discarded.
        with_retry(|| {
            push_blob_zstd(
                http,
                scheme,
                registry,
                repository,
                auth,
                &refc.digest,
                frame.clone(),
            )
        })
        .await?;
    } else {
        push_blob(client, image, frame, &refc.digest).await?;
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
///
/// With `require` there is no fallback, so only a successful response carrying
/// the header is `Ok`. A request error or an error status is reported as such:
/// neither says whether the registry has the capability.
///
/// Authenticate the probe because a credential-gated vk-registry omits the
/// capability header from its 401 response.
async fn detect_transparent(
    http: &reqwest::Client,
    scheme: &str,
    registry: &str,
    auth: &Auth,
    require: bool,
) -> Result<bool> {
    let url = format!("{scheme}://{registry}/v2/");
    with_retry(|| async {
        let resp = match auth.apply(http.get(&url)).send().await {
            Ok(resp) => resp,
            Err(e) if require || is_transport(&e) => return Err(req_err(e)),
            Err(_) => return Ok(false),
        };
        let advertised = resp.headers().contains_key(TRANSPARENT_ZSTD_HEADER);
        if !require {
            return Ok(advertised);
        }
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::format(format!(
                "registry {registry} answered HTTP {status} to the transparent-zstd \
                 probe ({url})"
            )));
        }
        if !advertised {
            return Err(Error::format(format!(
                "registry {registry} does not advertise the transparent-zstd scheme \
                 (no {TRANSPARENT_ZSTD_HEADER} header on {url}): not a vk-registry?"
            )));
        }
        Ok(true)
    })
    .await
}

/// Upload an already-zstd-compressed frame keyed by the digest of its
/// uncompressed form, tagging the body `Content-Encoding: zstd`. A monolithic
/// OCI upload issued directly (oci-client cannot set the per-request encoding);
/// vk-registry stores the frame, and serves it back or decoded
/// ([`pull_blob_zstd`]).
async fn push_blob_zstd(
    http: &reqwest::Client,
    scheme: &str,
    registry: &str,
    repository: &str,
    auth: &Auth,
    digest: &str,
    frame: Bytes,
) -> Result<()> {
    let uploads = format!("{scheme}://{registry}/v2/{repository}/blobs/uploads/");
    let resp = auth
        .apply(
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

    let resp = auth
        .apply(
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

/// How a local CAS entry holds the bytes its digest names. Each form has its
/// own directory, so an entry's place says how to read it and a store written
/// before frames were kept stays valid as it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codec {
    /// The bytes themselves, under `sha256/`: a compressed-digest chunk (whose
    /// digest is its frame's), or a chunk that arrived decoded.
    Raw,
    /// A zstd frame of them, under `zstd/`: a transparent-zstd chunk as the
    /// registry sent it.
    Zstd,
}

impl Codec {
    /// The order a read tries them in: an entry that needs no decoding first.
    const ALL: [Codec; 2] = [Codec::Raw, Codec::Zstd];

    fn dir(self) -> &'static str {
        match self {
            Codec::Raw => "sha256",
            Codec::Zstd => "zstd",
        }
    }
}

/// The local CAS's directory for `codec`, held open: every chunk operation is
/// relative to it, never through a symlink, so a link planted in the store
/// cannot redirect one. `dir` itself is the user's configured location.
fn cas_dir(dir: &Path, codec: Codec, create: bool) -> Result<std::fs::File> {
    if create {
        std::fs::create_dir_all(dir)?;
    }
    let root = cap_primitives::fs::open_ambient_dir(dir, cap_primitives::ambient_authority())?;
    restore::open_parent(&root, &[codec.dir().to_string()], create, &mut Vec::new())
}

fn cas_hex(digest: &str) -> Result<&str> {
    digest_hex(digest).ok_or_else(|| Error::format(format!("bad digest {digest:?}")))
}

/// Whether the local CAS holds an entry for `digest`, in either form. Its
/// content is checked when it is read ([`read_cas`]), not here.
fn cas_contains(dir: &Path, digest: &str) -> bool {
    let Ok(hex) = cas_hex(digest) else {
        return false;
    };
    Codec::ALL.into_iter().any(|codec| {
        cas_dir(dir, codec, false).is_ok_and(|sub| {
            restore::leaf_type(&sub, Path::new(hex)).is_ok_and(|t| t.is_some_and(|t| t.is_file()))
        })
    })
}

/// Create an unpredictable temporary file exclusively, then rename it into
/// place. Concurrent pulls see only complete chunks, and a planted file cannot
/// redirect the write. `data` is the entry as `codec` holds it.
fn cas_write(dir: &Path, codec: Codec, digest: &str, data: &[u8]) -> Result<()> {
    use std::io::Write as _;
    static TMPS: AtomicI64 = AtomicI64::new(0);
    let hex = cas_hex(digest)?;
    let sub = cas_dir(dir, codec, true)?;
    let random = std::hash::BuildHasher::hash_one(
        &std::collections::hash_map::RandomState::new(),
        (
            std::process::id(),
            TMPS.fetch_add(1, Ordering::Relaxed),
            std::time::SystemTime::now(),
        ),
    );
    let tmp = PathBuf::from(format!(".{hex}.tmp.{}.{random:016x}", std::process::id()));
    let mut file = cap_primitives::fs::open(
        &sub,
        &tmp,
        cap_primitives::fs::OpenOptions::new()
            .write(true)
            .create_new(true),
    )?;
    let written = file.write_all(data).and_then(|()| {
        drop(file);
        cap_primitives::fs::rename(&sub, &tmp, &sub, Path::new(hex))
    });
    if written.is_err() {
        let _ = cap_primitives::fs::remove_file(&sub, &tmp);
    }
    Ok(written?)
}

/// Read a chunk from the local CAS, verifying its digest; a corrupted entry is
/// removed and reported so the restore fails rather than trusting bad bytes.
/// The entry is opened without following a link and must be a regular file.
///
/// A frame is decoded first and the digest taken over the result. Neither an
/// entry nor what a frame decodes to may exceed [`MAX_CHUNK_BLOB`], whatever
/// size an index gives the chunk: that size is for [`assemble`] to hold the
/// bytes to, and a wrong one says nothing against the entry. `frame` is
/// scratch space for a frame, kept by the caller from one read to the next.
///
/// A chunk held in both forms is read from the first that verifies, and a
/// form that cannot be read (its directory is a link, say) does not hide the
/// other. Only an entry found corrupt is removed: malformed, over the limit or
/// of another digest. One that could not be read for want of memory or for an
/// I/O error stays. The removal is best-effort: it can race a concurrent pull
/// replacing the entry and unlink the new file, which is then fetched again,
/// and its failing leaves the error of the read as it is.
fn read_cas(dir: &Path, digest: &str, frame: &mut Vec<u8>) -> Result<Vec<u8>> {
    let hex = cas_hex(digest)?;
    let mut failed = None;
    for codec in Codec::ALL {
        let sub = match cas_dir(dir, codec, false) {
            Ok(sub) => sub,
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                failed = Some(e);
                continue;
            }
        };
        match read_cas_entry(&sub, codec, hex, frame) {
            Ok(Some(data)) if sha256_hex(&data) == digest => return Ok(data),
            Ok(None) => {}
            Ok(Some(_)) | Err(Error::Format(_)) => {
                let _ = cap_primitives::fs::remove_file(&sub, Path::new(hex));
                failed = Some(Error::format(format!("corrupt CAS entry {digest}")));
            }
            Err(e) => failed = Some(e),
        }
    }
    Err(failed.unwrap_or_else(|| Error::format(format!("no CAS entry {digest}"))))
}

/// The bytes the entry `hex` of the `codec` directory `sub` holds, not yet
/// verified, or `None` if there is no such entry. An entry that is corrupt
/// whatever its digest is an [`Error::Format`]: one over [`MAX_CHUNK_BLOB`],
/// or a frame that is malformed or decodes to more.
fn read_cas_entry(
    sub: &std::fs::File,
    codec: Codec,
    hex: &str,
    frame: &mut Vec<u8>,
) -> Result<Option<Vec<u8>>> {
    use std::io::Read as _;
    let file = match restore::open_nofollow(sub, Path::new(hex), false) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if !file.metadata()?.is_file() {
        return Err(Error::Io(std::io::Error::other(format!(
            "CAS entry {}/{hex} is not a file",
            codec.dir()
        ))));
    }
    // No entry is longer: one byte more gives a longer file away without
    // reading it all.
    let mut entry = file.take((MAX_CHUNK_BLOB as u64).saturating_add(1));
    let mut data = Vec::new();
    match codec {
        Codec::Raw => {
            entry.read_to_end(&mut data)?;
            if data.len() > MAX_CHUNK_BLOB {
                return Err(Error::format("entry is larger than a chunk blob"));
            }
        }
        Codec::Zstd => {
            frame.clear();
            entry.read_to_end(frame)?;
            if frame.len() > MAX_CHUNK_BLOB {
                return Err(Error::format("frame is larger than a chunk blob"));
            }
            match zstd_decode_chunk_into(frame, MAX_CHUNK_BLOB, &mut data) {
                Ok(()) => {}
                // Not the entry's fault: it may decode the next time.
                Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::OutOfMemory => {
                    return Err(Error::Io(e));
                }
                Err(e) => return Err(Error::format(restore::message(&e))),
            }
        }
    }
    Ok(Some(data))
}

/// The `Content-Encoding` of a response, as far as a blob pull reads one.
enum Encoding {
    Identity,
    Zstd,
}

impl Encoding {
    /// Accept a single `identity` or `zstd` coding, ignoring case. A missing
    /// or empty field means identity. Reject lists and repeated fields.
    fn of(headers: &reqwest::header::HeaderMap) -> Result<Self> {
        let mut fields = headers.get_all(reqwest::header::CONTENT_ENCODING).iter();
        let Some(field) = fields.next() else {
            return Ok(Encoding::Identity);
        };
        let more = fields.count();
        if more > 0 {
            return Err(Error::format(format!(
                "{} Content-Encoding fields",
                more.saturating_add(1)
            )));
        }
        let coding = field.to_str().unwrap_or("?").trim();
        if coding.is_empty() || coding.eq_ignore_ascii_case("identity") {
            Ok(Encoding::Identity)
        } else if coding.eq_ignore_ascii_case("zstd") {
            Ok(Encoding::Zstd)
        } else {
            Err(Error::format(format!(
                "unsupported Content-Encoding {field:?}"
            )))
        }
    }
}

/// The digest a registry states for the blob it serves, in a
/// `Docker-Content-Digest` header.
struct StatedDigest {
    /// As sent: `<algorithm>:<hex>`.
    stated: String,
    algorithm: DigestAlgorithm,
}

enum DigestAlgorithm {
    Sha256,
    Sha384,
    Sha512,
}

impl StatedDigest {
    /// As `oci-client` reads the header on the pulls it makes: absent or
    /// empty states nothing, and one that is not text, has no `:` or names
    /// another algorithm than these is an error. Unlike it, more than one
    /// field is an error too, whatever each holds: which one the registry
    /// means is not known.
    fn of(headers: &reqwest::header::HeaderMap) -> Result<Option<Self>> {
        let mut fields = headers.get_all(DOCKER_CONTENT_DIGEST).iter();
        let Some(value) = fields.next() else {
            return Ok(None);
        };
        let more = fields.count();
        if more > 0 {
            return Err(Error::format(format!(
                "{} {DOCKER_CONTENT_DIGEST} fields",
                more.saturating_add(1)
            )));
        }
        let stated = value
            .to_str()
            .map_err(|_| Error::format(format!("{DOCKER_CONTENT_DIGEST} {value:?} is not text")))?;
        if stated.is_empty() {
            return Ok(None);
        }
        let algorithm = match stated.split_once(':') {
            Some(("sha256", _)) => DigestAlgorithm::Sha256,
            Some(("sha384", _)) => DigestAlgorithm::Sha384,
            Some(("sha512", _)) => DigestAlgorithm::Sha512,
            Some(_) => {
                return Err(Error::format(format!(
                    "{DOCKER_CONTENT_DIGEST} {stated:?} uses an unsupported algorithm"
                )));
            }
            None => {
                return Err(Error::format(format!(
                    "{DOCKER_CONTENT_DIGEST} {stated:?} names no algorithm"
                )));
            }
        };
        Ok(Some(StatedDigest {
            stated: stated.to_string(),
            algorithm,
        }))
    }

    /// Whether `data`, whose `sha256:<hex>` digest is `sha256`, has the stated
    /// digest, compared as text like `oci-client` does: lowercase hex only.
    fn verify(&self, data: &[u8], sha256: &str) -> Result<()> {
        use sha2::Digest;
        use std::fmt::Write as _;
        let hex = |name: &str, hash: &[u8]| {
            let mut digest = String::from(name);
            for b in hash {
                // Writing to a `String` does not fail.
                let _ = write!(digest, "{b:02x}");
            }
            digest
        };
        let got = match self.algorithm {
            DigestAlgorithm::Sha256 => sha256.to_string(),
            DigestAlgorithm::Sha384 => hex("sha384:", &sha2::Sha384::digest(data)),
            DigestAlgorithm::Sha512 => hex("sha512:", &sha2::Sha512::digest(data)),
        };
        if got != self.stated {
            return Err(Error::format(format!(
                "{DOCKER_CONTENT_DIGEST} mismatch: the registry states {}, got {got}",
                self.stated
            )));
        }
        Ok(())
    }
}

/// Fetch the blob `c` names from a transparent-zstd registry and return its
/// bytes, verified against the size and the digest of `c` and, as `oci-client`
/// does, against a `Docker-Content-Digest` the response states, with the frame
/// they arrived as. The request accepts zstd, so the registry may send the
/// frame it stores rather than decompress it; a response without the encoding
/// (a blob stored as is, or a proxy decoding on the way) is taken as the blob
/// itself. Issued directly: oci-client can neither ask for an encoding nor undo
/// one before it verifies the digest.
///
/// One attempt, for [`with_retry`] to repeat: only the transport failing is
/// [`Error::Network`].
async fn pull_blob_zstd(
    http: &reqwest::Client,
    auth: &Auth,
    url: &str,
    c: &ChunkRef,
) -> Result<Fetched> {
    let mut resp = auth
        .apply(
            http.get(url)
                .header(reqwest::header::ACCEPT_ENCODING, "zstd"),
        )
        .send()
        .await
        .map_err(req_err)?;
    let digest = &c.digest;
    if resp.status() != reqwest::StatusCode::OK {
        return Err(Error::format(format!(
            "blob {digest} GET: HTTP {}",
            resp.status()
        )));
    }
    let encoding = Encoding::of(resp.headers()).map_err(|e| in_blob(digest, &e))?;
    let stated = StatedDigest::of(resp.headers()).map_err(|e| in_blob(digest, &e))?;
    // Cap the buffered identity body at the indexed size. A zstd frame can
    // exceed the size of a small chunk, so use the chunk blob limit on the wire.
    let size = blob_size(c);
    let cap = match encoding {
        Encoding::Identity => size,
        Encoding::Zstd => MAX_CHUNK_BLOB,
    };
    // Grown as bytes arrive and capped, whatever length the response claims.
    let mut body = Vec::new();
    while let Some(part) = resp.chunk().await.map_err(req_err)? {
        if body.len().saturating_add(part.len()) > cap {
            return Err(Error::format(format!(
                "blob {digest} is larger than {cap} bytes"
            )));
        }
        body.extend_from_slice(&part);
    }
    let fetched = match encoding {
        Encoding::Identity => Fetched {
            data: body,
            frame: None,
        },
        Encoding::Zstd => {
            // A frame inflating past the blob's stated size is refused, a
            // byte beyond it.
            let mut data = Vec::new();
            zstd_decode_chunk_into(&body, size, &mut data).map_err(|e| in_blob(digest, &e))?;
            Fetched {
                data,
                frame: Some(body),
            }
        }
    };
    if fetched.data.len() as i64 != c.size {
        return Err(Error::format(format!(
            "blob {digest}: {} bytes, index says {}",
            fetched.data.len(),
            c.size
        )));
    }
    let got = sha256_hex(&fetched.data);
    if got != *digest {
        return Err(Error::format(format!(
            "blob digest mismatch: want {digest}, got {got}"
        )));
    }
    if let Some(stated) = stated {
        stated
            .verify(&fetched.data, &got)
            .map_err(|e| in_blob(digest, &e))?;
    }
    Ok(fetched)
}

/// `e` as a final failure of the blob `digest`: what the registry sent is
/// wrong, which asking again does not change.
fn in_blob(digest: &str, e: &Error) -> Error {
    Error::format(format!("blob {digest}: {}", restore::message(e)))
}

/// Whether a manifest fetch failed because the tag is not there — a cache miss,
/// not an error. `oci-client` only reports `ImageManifestNotFoundError` from
/// its image-index resolution; a registry's own 404 arrives as the OCI error
/// envelope (`MANIFEST_UNKNOWN`, or `NAME_UNKNOWN` for a repository nothing has
/// been pushed to yet).
fn is_missing(e: &OciDistributionError) -> bool {
    match e {
        OciDistributionError::ImageManifestNotFoundError(_) => true,
        OciDistributionError::RegistryError { envelope, .. } => envelope.errors.iter().any(|err| {
            matches!(
                err.code,
                OciErrorCode::ManifestUnknown | OciErrorCode::NameUnknown
            )
        }),
        _ => false,
    }
}

/// Map an `oci-client` error, a transport failure to [`Error::Network`] — which is
/// what [`with_retry`] repeats — and everything the registry answered to
/// [`Error::Format`].
fn oci_err(e: OciDistributionError) -> Error {
    // `RequestError` is transparent over the `reqwest` error, so its chain
    // carries the same reason a direct request's does.
    let msg = with_causes("oci", &e);
    if let OciDistributionError::RequestError(re) = &e
        && is_transport(re)
    {
        return Error::network(msg);
    }
    Error::format(msg)
}

fn req_err(e: reqwest::Error) -> Error {
    let msg = with_causes("http", &e);
    if is_transport(&e) {
        return Error::network(msg);
    }
    Error::format(msg)
}

fn join_err(e: JoinError) -> Error {
    Error::format(format!("task join: {e}"))
}

/// The outcome of the push's builder task, a panic in it carried on here.
fn join_builder(joined: std::result::Result<Result<Index>, JoinError>) -> Result<Index> {
    match joined {
        Ok(built) => built,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        Err(e) => Err(join_err(e)),
    }
}

/// Waits until `set` has room for one more transfer — unless a transfer
/// already running has failed, which is returned instead.
///
/// An entry is thousands of chunks, [`FETCH_CONCURRENCY`] in flight at a time,
/// and every chunk request retries on its own before it fails
/// ([`crate::retry::with_retry`]). Taking slots as they free up and only
/// looking at the outcomes once everything was queued would walk every
/// remaining chunk through those same retries after a registry stopped
/// answering, which is tens of minutes of silence for a failure already
/// decided by the first round. So a slot is free only once the outcome of the
/// transfer that held it has been seen, and the first failure ends the whole
/// transfer: the caller's `?` drops `set`, aborting the transfers still in
/// flight.
async fn next_slot(set: &mut JoinSet<Result<()>>) -> Result<()> {
    while set.len() >= FETCH_CONCURRENCY {
        if let Some(res) = set.join_next().await {
            res.map_err(join_err)??;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{
        FakeServer, Registry, blob_response, blob_response_with, install_crypto,
    };
    use std::sync::atomic::AtomicBool;

    /// A directory of [`cas_tmp`], removed with what it holds when dropped.
    #[derive(Debug)]
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            TempDir(cas_tmp(tag))
        }
    }

    impl std::ops::Deref for TempDir {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl AsRef<Path> for TempDir {
        fn as_ref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // Best-effort: a leftover only costs space in the temp directory.
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cas_tmp(tag: &str) -> PathBuf {
        static N: AtomicI64 = AtomicI64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ocicas-cas-{}-{tag}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Read a CAS entry of any size a chunk can have.
    fn read(dir: &Path, digest: &str) -> Result<Vec<u8>> {
        read_cas(dir, digest, &mut Vec::new())
    }

    #[test]
    fn cas_round_trips_and_drops_corrupt_entries() {
        let dir = cas_tmp("rt");
        let digest = sha256_hex(b"chunk");
        assert!(!cas_contains(&dir, &digest));
        assert!(read(&dir, &digest).is_err());
        cas_write(&dir, Codec::Raw, &digest, b"chunk").unwrap();
        assert!(cas_contains(&dir, &digest));
        assert_eq!(read(&dir, &digest).unwrap(), b"chunk");
        let hex = digest_hex(&digest).unwrap();
        std::fs::write(dir.join("sha256").join(hex), b"other").unwrap();
        assert!(read(&dir, &digest).is_err());
        assert!(!cas_contains(&dir, &digest));
    }

    /// A frame is kept as it is, apart from the entries that hold the bytes
    /// themselves, and read back as the bytes it encodes.
    #[test]
    fn cas_keeps_a_frame_and_reads_its_content() {
        let dir = TempDir::new("frame");
        let raw = crate::zctx::tests::sample(300_000, 1);
        let frame = zstd::bulk::compress(&raw, 3).unwrap();
        let digest = sha256_hex(&raw);
        let hex = digest_hex(&digest).unwrap();
        cas_write(&dir, Codec::Zstd, &digest, &frame).unwrap();
        assert_eq!(std::fs::read(dir.join("zstd").join(hex)).unwrap(), frame);
        assert!(!dir.join("sha256").exists());
        assert!(cas_contains(&dir, &digest));
        let mut scratch = Vec::new();
        assert_eq!(read_cas(&dir, &digest, &mut scratch).unwrap(), raw);
        // The entry is still there, and the scratch buffer serves the next read.
        assert_eq!(read_cas(&dir, &digest, &mut scratch).unwrap(), raw);
    }

    /// An entry that does not hold, within the size of a chunk blob, the bytes
    /// its name digests is removed and nothing of it returned.
    #[test]
    fn cas_drops_a_corrupt_entry() {
        let raw = crate::zctx::tests::sample(300_000, 1);
        let frame = zstd::bulk::compress(&raw, 3).unwrap();
        let other = zstd::bulk::compress(&crate::zctx::tests::sample(300_000, 2), 3).unwrap();
        let mut longer = raw.clone();
        longer.push(0);
        let over = vec![0; MAX_CHUNK_BLOB + 1];
        let cases: [(&str, Codec, &[u8], Vec<u8>); 7] = [
            (
                "not a frame",
                Codec::Zstd,
                &raw,
                b"not a zstd frame".to_vec(),
            ),
            (
                "cut short",
                Codec::Zstd,
                &raw,
                frame[..frame.len() - 8].to_vec(),
            ),
            ("a frame of other bytes", Codec::Zstd, &raw, other),
            (
                "a frame of more bytes",
                Codec::Zstd,
                &raw,
                zstd::bulk::compress(&longer, 3).unwrap(),
            ),
            (
                "a frame of more than a chunk blob holds",
                Codec::Zstd,
                &over,
                zstd::bulk::compress(&over, 3).unwrap(),
            ),
            ("longer than any frame", Codec::Zstd, &raw, over.clone()),
            (
                "longer than any chunk blob",
                Codec::Raw,
                &over,
                over.clone(),
            ),
        ];
        for (what, codec, named, entry) in cases {
            let digest = sha256_hex(named);
            let hex = digest_hex(&digest).unwrap();
            let dir = TempDir::new("badentry");
            cas_write(&dir, codec, &digest, &entry).unwrap();
            assert!(cas_contains(&dir, &digest), "{what}");
            let error = read_cas(&dir, &digest, &mut Vec::new()).unwrap_err();
            assert!(
                error.to_string().contains("corrupt CAS entry"),
                "{what}: {error}"
            );
            assert!(!dir.join(codec.dir()).join(hex).exists(), "{what}");
            assert!(!cas_contains(&dir, &digest), "{what}");
        }
    }

    /// An entry of the largest size a chunk blob has is read, in either form.
    #[test]
    fn cas_reads_an_entry_of_the_largest_size() {
        let raw = vec![0; MAX_CHUNK_BLOB];
        let digest = sha256_hex(&raw);
        for (codec, entry) in [
            (Codec::Raw, raw.clone()),
            (Codec::Zstd, zstd::bulk::compress(&raw, 3).unwrap()),
        ] {
            let dir = TempDir::new("largest");
            cas_write(&dir, codec, &digest, &entry).unwrap();
            assert!(read(&dir, &digest).unwrap() == raw, "{codec:?}");
        }
    }

    /// A frame that could not be decoded for want of memory stays: it is read
    /// once there is some.
    #[test]
    fn cas_keeps_a_frame_it_had_no_memory_to_decode() {
        let dir = TempDir::new("nomemory");
        let raw = crate::zctx::tests::sample(300_000, 1);
        let digest = sha256_hex(&raw);
        let hex = digest_hex(&digest).unwrap();
        cas_write(
            &dir,
            Codec::Zstd,
            &digest,
            &zstd::bulk::compress(&raw, 3).unwrap(),
        )
        .unwrap();

        crate::zctx::tests::OUT_OF_MEMORY.set(true);
        let error = read(&dir, &digest).unwrap_err();
        crate::zctx::tests::OUT_OF_MEMORY.set(false);
        let Error::Io(error) = error else {
            panic!("{error}");
        };
        assert_eq!(error.kind(), std::io::ErrorKind::OutOfMemory);
        assert!(dir.join("zstd").join(hex).exists());
        assert_eq!(read(&dir, &digest).unwrap(), raw);
    }

    /// Pulls that fill a new store at once each find or make the directory of
    /// a form: the writer that loses the race to create it uses the winner's.
    #[test]
    fn concurrent_writers_share_a_new_cas_directory() {
        const WRITERS: usize = 8;
        let frame_of = |i: usize| zstd::bulk::compress(format!("chunk{i:02}").as_bytes(), 3);
        for _ in 0..50 {
            let dir = TempDir::new("race");
            let start = std::sync::Barrier::new(WRITERS);
            std::thread::scope(|scope| {
                for i in 0..WRITERS {
                    let (dir, start) = (&dir, &start);
                    scope.spawn(move || {
                        let digest = sha256_hex(format!("chunk{i:02}").as_bytes());
                        start.wait();
                        cas_write(dir, Codec::Zstd, &digest, &frame_of(i).unwrap()).unwrap();
                    });
                }
            });
            for i in 0..WRITERS {
                let chunk = format!("chunk{i:02}");
                let digest = sha256_hex(chunk.as_bytes());
                assert_eq!(read(&dir, &digest).unwrap(), chunk.as_bytes());
            }
        }
    }

    /// A chunk held in both forms survives one of them being corrupt.
    #[test]
    fn cas_reads_the_form_that_verifies() {
        let raw = crate::zctx::tests::sample(300_000, 1);
        let frame = zstd::bulk::compress(&raw, 3).unwrap();
        let digest = sha256_hex(&raw);
        let hex = digest_hex(&digest).unwrap();

        let dir = TempDir::new("bothraw");
        cas_write(&dir, Codec::Raw, &digest, b"other").unwrap();
        cas_write(&dir, Codec::Zstd, &digest, &frame).unwrap();
        assert_eq!(read(&dir, &digest).unwrap(), raw);
        assert!(!dir.join("sha256").join(hex).exists());

        let dir = TempDir::new("bothframe");
        cas_write(&dir, Codec::Raw, &digest, &raw).unwrap();
        cas_write(&dir, Codec::Zstd, &digest, b"other").unwrap();
        assert_eq!(read(&dir, &digest).unwrap(), raw);
    }

    #[cfg(unix)]
    #[test]
    fn cas_does_not_follow_planted_symlinks() {
        let outside = cas_tmp("outside");
        let digest = sha256_hex(b"chunk");
        let hex = digest_hex(&digest).unwrap().to_string();

        // A symlinked `sha256` directory.
        let dir = cas_tmp("dirlink");
        std::os::unix::fs::symlink(&outside, dir.join("sha256")).unwrap();
        assert!(cas_write(&dir, Codec::Raw, &digest, b"chunk").is_err());
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());

        // An entry that is a link to a file holding the right bytes elsewhere.
        let dir = cas_tmp("leaflink");
        std::fs::create_dir(dir.join("sha256")).unwrap();
        std::fs::write(outside.join("secret"), b"chunk").unwrap();
        std::os::unix::fs::symlink(outside.join("secret"), dir.join("sha256").join(&hex)).unwrap();
        assert!(!cas_contains(&dir, &digest));
        assert!(read(&dir, &digest).is_err());
        // Writing replaces the link rather than writing through it.
        cas_write(&dir, Codec::Raw, &digest, b"chunk").unwrap();
        assert!(
            std::fs::symlink_metadata(dir.join("sha256").join(&hex))
                .unwrap()
                .is_file()
        );
        assert_eq!(std::fs::read(outside.join("secret")).unwrap(), b"chunk");

        // A link to a file of the same directory is not followed either.
        let dir = TempDir::new("siblinglink");
        std::fs::create_dir(dir.join("sha256")).unwrap();
        std::fs::write(dir.join("sha256").join("target"), b"chunk").unwrap();
        std::os::unix::fs::symlink("target", dir.join("sha256").join(&hex)).unwrap();
        assert!(!cas_contains(&dir, &digest));
        assert!(read(&dir, &digest).is_err());
        assert_eq!(
            std::fs::read(dir.join("sha256").join("target")).unwrap(),
            b"chunk"
        );

        // The same for the directory of frames and an entry in it.
        let frame = zstd::bulk::compress(b"chunk", 3).unwrap();
        let dir = TempDir::new("framedirlink");
        let frames = TempDir::new("frames");
        std::os::unix::fs::symlink(&frames, dir.join("zstd")).unwrap();
        assert!(cas_write(&dir, Codec::Zstd, &digest, &frame).is_err());
        assert!(std::fs::read_dir(&frames).unwrap().next().is_none());
        std::fs::write(frames.join(&hex), &frame).unwrap();
        assert!(!cas_contains(&dir, &digest));
        assert!(read(&dir, &digest).is_err());

        // Nor does an entry that is no file hide one in the other directory,
        // or hold the read: a FIFO has no writer to wait for here.
        let dir = TempDir::new("fifoframe");
        std::fs::create_dir(dir.join("sha256")).unwrap();
        let made = std::process::Command::new("mkfifo")
            .arg(dir.join("sha256").join(&hex))
            .status()
            .unwrap();
        assert!(made.success());
        cas_write(&dir, Codec::Zstd, &digest, &frame).unwrap();
        // On a thread of its own: a read the FIFO holds fails the test rather
        // than hang it.
        let (done, read_back) = std::sync::mpsc::channel();
        let (store, wanted) = (dir.to_path_buf(), digest.clone());
        std::thread::spawn(move || done.send(read(&store, &wanted)));
        let read_back = read_back
            .recv_timeout(Duration::from_secs(10))
            .expect("the FIFO held the read");
        assert_eq!(read_back.unwrap(), b"chunk");

        // A linked directory does not hide the entry the other one holds.
        let dir = TempDir::new("dirlinkframe");
        let linked = TempDir::new("linked");
        std::os::unix::fs::symlink(&linked, dir.join("sha256")).unwrap();
        cas_write(&dir, Codec::Zstd, &digest, &frame).unwrap();
        assert!(cas_contains(&dir, &digest));
        assert_eq!(read(&dir, &digest).unwrap(), b"chunk");
        assert!(std::fs::read_dir(&linked).unwrap().next().is_none());

        let dir = TempDir::new("frameleaflink");
        std::fs::create_dir(dir.join("zstd")).unwrap();
        std::fs::write(outside.join("frame"), &frame).unwrap();
        std::os::unix::fs::symlink(outside.join("frame"), dir.join("zstd").join(&hex)).unwrap();
        assert!(!cas_contains(&dir, &digest));
        assert!(read(&dir, &digest).is_err());
        assert_eq!(std::fs::read(outside.join("frame")).unwrap(), frame);
    }

    /// A credential-gated registry omits the capability header from an
    /// anonymous probe's 401 response, so the probe sends credentials.
    #[tokio::test]
    async fn the_capability_probe_carries_the_credential() {
        install_crypto();
        let server = FakeServer::start(vec![(200, "{}")]);
        let auth = Auth::Basic {
            user: "ci".to_string(),
            pass: "s3cret".to_string(),
        };
        let transparent = detect_transparent(
            &reqwest::Client::new(),
            "http",
            &server.authority(),
            &auth,
            false,
        )
        .await
        .expect("probe");
        // A 200 without the capability header reports no support.
        assert!(!transparent);
        let seen = server.seen();
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].path, "/v2/");
        // base64("ci:s3cret")
        assert_eq!(seen[0].authorization.as_deref(), Some("Basic Y2k6czNjcmV0"));
    }

    #[tokio::test]
    async fn the_capability_probe_recovers_after_a_disconnect() {
        install_crypto();
        let server = FakeServer::start_raw(vec![
            String::new(),
            "HTTP/1.1 200 OK\r\nx-virtkit-transparent-zstd: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ]);
        let store = Store::open(
            &format!("{}/repo", server.authority()),
            RemoteOptions {
                plain_http: true,
                require_transparent: true,
                ..Default::default()
            },
        )
        .await
        .expect("probe retries");
        assert!(store.transparent());
        assert_eq!(server.seen().len(), 2);
    }

    /// The error of opening a store that requires the transparent-zstd scheme
    /// on a registry answering its probe with `response`, at once: it
    /// answered, so there is nothing to retry.
    async fn required_transparent_refusal(response: &str) -> String {
        install_crypto();
        let server = FakeServer::start_raw(vec![response.to_string()]);
        let Err(error) = Store::open(
            &format!("{}/repo", server.authority()),
            RemoteOptions {
                plain_http: true,
                require_transparent: true,
                ..Default::default()
            },
        )
        .await
        else {
            panic!("the registry was accepted");
        };
        assert!(matches!(error, Error::Format(_)), "{error}");
        assert_eq!(server.seen().len(), 1);
        error.to_string()
    }

    #[tokio::test]
    async fn a_required_transparent_scheme_refuses_a_plain_registry() {
        let error = required_transparent_refusal(
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
        )
        .await;
        assert!(
            error.contains("does not advertise the transparent-zstd scheme"),
            "{error}"
        );
        assert!(error.contains("not a vk-registry?"), "{error}");
    }

    /// A credential-gated vk-registry omits the header from its 401: the
    /// error names the status, not a missing capability.
    #[tokio::test]
    async fn a_required_transparent_scheme_reports_a_refused_probe() {
        let error = required_transparent_refusal(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(error.contains("HTTP 401"), "{error}");
        assert!(!error.contains("not a vk-registry?"), "{error}");
    }

    /// The header on an error response does not establish the capability.
    #[tokio::test]
    async fn a_required_transparent_scheme_refuses_a_failed_probe_with_the_header() {
        let error = required_transparent_refusal(
            "HTTP/1.1 503 Service Unavailable\r\nx-virtkit-transparent-zstd: 1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        assert!(error.contains("HTTP 503"), "{error}");
        assert!(!error.contains("not a vk-registry?"), "{error}");
    }

    /// An answer that is not HTTP is final in both modes: a failed request
    /// when the scheme is required, no capability otherwise.
    #[tokio::test]
    async fn the_capability_probe_reads_a_malformed_response_by_mode() {
        install_crypto();
        for require in [true, false] {
            let server = FakeServer::start_raw(vec!["NOT HTTP\r\n\r\n".into()]);
            let probed = detect_transparent(
                &reqwest::Client::new(),
                "http",
                &server.authority(),
                &Auth::None,
                require,
            )
            .await;
            match probed {
                Err(Error::Format(error)) if require => {
                    assert!(error.starts_with("http: "), "{error}");
                }
                Ok(false) if !require => {}
                other => panic!("require {require}: {other:?}"),
            }
            assert_eq!(server.seen().len(), 1);
        }
    }

    fn http_client() -> Client {
        Client::new(ClientConfig {
            protocol: ClientProtocol::Http,
            ..Default::default()
        })
    }

    #[tokio::test]
    async fn a_blob_retry_discards_the_partial_body() {
        install_crypto();
        let server = FakeServer::start_raw(vec![
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\npart".into(),
            "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\ncomplete".into(),
        ]);
        let image = format!("{}/repo:tag", server.authority()).parse().unwrap();
        let desc = descriptor(MEDIA_TYPE_CHUNK, sha256_hex(b"complete"), 8);
        let body = pull_blob_bytes(&http_client(), &image, &desc, MAX_CHUNK_BLOB)
            .await
            .unwrap();
        assert_eq!(body, b"complete");
        let seen = server.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].path, seen[1].path);
    }

    #[tokio::test]
    async fn a_blob_larger_than_its_cap_is_refused() {
        install_crypto();
        // The descriptor understates the size; the cap holds regardless.
        let server = FakeServer::start_raw(
            (0..4)
                .map(|_| {
                    "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\ncomplete"
                        .into()
                })
                .collect(),
        );
        let image = format!("{}/repo:tag", server.authority()).parse().unwrap();
        let desc = descriptor(MEDIA_TYPE_CHUNK, sha256_hex(b"complete"), 1);
        let error = pull_blob_bytes(&http_client(), &image, &desc, 4)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("larger than"), "{error}");
    }

    #[tokio::test]
    async fn a_blob_digest_mismatch_is_final() {
        install_crypto();
        let server = FakeServer::start(vec![(200, "corrupt")]);
        let image = format!("{}/repo:tag", server.authority()).parse().unwrap();
        let desc = descriptor(MEDIA_TYPE_CHUNK, sha256_hex(b"correct"), 7);
        let error = pull_blob_bytes(&http_client(), &image, &desc, MAX_CHUNK_BLOB)
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Format(_)), "{error}");
        assert!(error.to_string().contains("digest"), "{error}");
        assert_eq!(server.seen().len(), 1);
    }

    #[tokio::test]
    async fn a_transparent_upload_retry_starts_a_new_session() {
        install_crypto();
        let server = FakeServer::start_raw(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            "HTTP/1.1 202 Accepted\r\nLocation: /upload/first\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            String::new(),
            "HTTP/1.1 202 Accepted\r\nLocation: /upload/second\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
            "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
        ]);
        let image = format!("{}/repo:tag", server.authority()).parse().unwrap();
        let frame = zstd::bulk::compress(b"chunk", 3).unwrap();
        let chunk = ChunkRef {
            digest: sha256_hex(b"chunk"),
            size: 5,
            raw_size: 5,
        };
        let counters = Counters::default();
        upload_chunk(
            &http_client(),
            &reqwest::Client::new(),
            &image,
            true,
            "http",
            &server.authority(),
            "repo",
            &Auth::None,
            &chunk,
            frame.clone(),
            &counters,
        )
        .await
        .unwrap();
        let seen = server.seen();
        assert_eq!(
            seen.iter().map(|r| r.method.as_str()).collect::<Vec<_>>(),
            ["HEAD", "POST", "PUT", "POST", "PUT"]
        );
        assert_eq!(seen[2].path, "/upload/first");
        assert_eq!(seen[4].path, "/upload/second");
        assert_eq!(seen[2].body, frame);
        assert_eq!(seen[4].body, frame);
        assert_eq!(seen[2].query, seen[4].query);
        assert_eq!(counters.pushed.load(Ordering::Relaxed), 1);
        assert_eq!(counters.bytes.load(Ordering::Relaxed), frame.len() as i64);
    }

    /// The reference of the chunk `raw` under the transparent scheme.
    fn raw_ref(raw: &[u8]) -> ChunkRef {
        ChunkRef {
            digest: sha256_hex(raw),
            size: raw.len() as i64,
            raw_size: raw.len() as i64,
        }
    }

    async fn pull_zstd(server: &FakeServer, auth: &Auth, c: &ChunkRef) -> Result<Fetched> {
        let url = format!("{}/v2/repo/blobs/{}", server.base(), c.digest);
        pull_blob_zstd(&raw_client().build().unwrap(), auth, &url, c).await
    }

    /// A transparent pull accepts zstd, so the registry sends the frame it
    /// stores; the digest is that of the decoded bytes.
    #[tokio::test]
    async fn a_transparent_pull_asks_for_the_frame() {
        install_crypto();
        let raw = crate::zctx::tests::sample(300_000, 1);
        let frame = zstd::bulk::compress(&raw, 3).unwrap();
        assert!(frame.len() < raw.len());
        let server = FakeServer::start_bytes(vec![blob_response(Some("zstd"), &frame)]);
        let auth = Auth::Basic {
            user: "ci".to_string(),
            pass: "s3cret".to_string(),
        };
        let chunk = raw_ref(&raw);
        let fetched = pull_zstd(&server, &auth, &chunk).await.unwrap();
        assert_eq!(fetched.data, raw);
        assert_eq!(fetched.frame, Some(frame));
        let seen = server.seen();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].method, "GET");
        assert_eq!(seen[0].path, format!("/v2/repo/blobs/{}", chunk.digest));
        assert_eq!(seen[0].accept_encoding.as_deref(), Some("zstd"));
        assert_eq!(seen[0].authorization.as_deref(), Some("Basic Y2k6czNjcmV0"));
    }

    /// A proxy on the way may decode the frame: a response without the
    /// encoding is the blob itself. So is one whose field is empty, which
    /// lists no coding.
    #[tokio::test]
    async fn a_transparent_pull_takes_a_decoded_response() {
        install_crypto();
        let raw = crate::zctx::tests::sample(300_000, 2);
        let chunk = raw_ref(&raw);
        for encoding in [None, Some("identity"), Some("")] {
            let server = FakeServer::start_bytes(vec![blob_response(encoding, &raw)]);
            let got = pull_zstd(&server, &Auth::None, &chunk).await.unwrap();
            assert_eq!(got.data, raw, "{encoding:?}");
            assert_eq!(got.frame, None, "{encoding:?}");
        }
    }

    /// What a registry answered is final, and nothing of a response that does
    /// not decode to the bytes the digest names is returned.
    #[tokio::test]
    async fn a_bad_transparent_response_is_refused() {
        install_crypto();
        let raw = crate::zctx::tests::sample(300_000, 3);
        let frame = zstd::bulk::compress(&raw, 3).unwrap();
        let other = crate::zctx::tests::sample(300_000, 4);
        let mut longer = raw.clone();
        longer.push(0);
        let frame_of = |raw: &[u8]| zstd::bulk::compress(raw, 3).unwrap();
        let not_found = b"HTTP/1.1 404 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let wire_cap = format!("is larger than {MAX_CHUNK_BLOB} bytes");
        let cases: Vec<(&str, Vec<u8>, &str)> = vec![
            (
                "not a frame",
                blob_response(Some("zstd"), b"not a zstd frame"),
                "blob sha256:",
            ),
            (
                "a frame cut short",
                blob_response(Some("zstd"), &frame[..frame.len() - 8]),
                "blob sha256:",
            ),
            (
                "a frame followed by other bytes",
                blob_response(Some("zstd"), &[frame.as_slice(), b"trailer"].concat()),
                "blob sha256:",
            ),
            (
                "a frame of other bytes",
                blob_response(Some("zstd"), &frame_of(&other)),
                "digest mismatch",
            ),
            (
                "other bytes",
                blob_response(None, &other),
                "digest mismatch",
            ),
            (
                "a frame one byte longer than the chunk",
                blob_response(Some("zstd"), &frame_of(&longer)),
                "exceeds 300000 bytes",
            ),
            (
                "a frame inflating far past the chunk",
                blob_response(Some("zstd"), &frame_of(&vec![0; 64 << 20])),
                "exceeds 300000 bytes",
            ),
            (
                "an encoding other than zstd",
                blob_response(Some("gzip"), &frame),
                "unsupported Content-Encoding",
            ),
            (
                "two encodings",
                blob_response(Some("zstd, gzip"), &frame),
                "unsupported Content-Encoding",
            ),
            (
                "an encoding in two fields",
                blob_response_with(
                    &[("Content-Encoding", "zstd"), ("Content-Encoding", "zstd")],
                    &frame,
                ),
                "2 Content-Encoding fields",
            ),
            (
                "a frame asking for a window past any chunk's",
                blob_response(
                    Some("zstd"),
                    &crate::zctx::tests::frame_with_window(&raw, 26),
                ),
                "byte window",
            ),
            (
                "more than a chunk blob holds",
                blob_response(None, &vec![0; MAX_CHUNK_BLOB + 1]),
                "is larger than",
            ),
            (
                "a frame of more than a chunk blob holds",
                blob_response(Some("zstd"), &vec![0; MAX_CHUNK_BLOB + 1]),
                wire_cap.as_str(),
            ),
            ("a missing blob", not_found.to_vec(), "HTTP 404"),
        ];
        let chunk = raw_ref(&raw);
        for (what, response, reason) in cases {
            let server = FakeServer::start_bytes(vec![response]);
            let error = pull_zstd(&server, &Auth::None, &chunk).await.unwrap_err();
            assert!(matches!(error, Error::Format(_)), "{what}: {error}");
            assert!(error.to_string().contains(reason), "{what}: {error}");
        }
    }

    /// The blob is the bytes the reference's size counts: a response holding
    /// more or fewer is refused whatever its digest, as a frame or decoded.
    #[tokio::test]
    async fn a_transparent_pull_holds_the_blob_to_its_size() {
        install_crypto();
        let raw = crate::zctx::tests::sample(300_000, 6);
        let frame = zstd::bulk::compress(&raw, 3).unwrap();
        for (encoding, body) in [(None, &raw), (Some("zstd"), &frame)] {
            for (size, reason) in [
                (raw.len() - 1, ["is larger than 299999", "exceeds 299999"]),
                (raw.len() + 1, ["index says 300001"; 2]),
            ] {
                let mut chunk = raw_ref(&raw);
                chunk.size = size as i64;
                let server = FakeServer::start_bytes(vec![blob_response(encoding, body)]);
                let error = pull_zstd(&server, &Auth::None, &chunk).await.unwrap_err();
                assert!(matches!(error, Error::Format(_)), "{encoding:?}: {error}");
                let error = error.to_string();
                assert!(
                    reason.iter().any(|r| error.contains(r)),
                    "{encoding:?}, size {size}: {error}"
                );
            }
        }
    }

    /// A frame is not held to the size of the chunk it encodes: that of a
    /// small chunk is the longer of the two.
    #[tokio::test]
    async fn a_transparent_pull_takes_a_frame_longer_than_its_chunk() {
        install_crypto();
        let raw = b"ED-209";
        let frame = zstd::bulk::compress(raw, 3).unwrap();
        assert!(frame.len() > raw.len());
        let server = FakeServer::start_bytes(vec![blob_response(Some("zstd"), &frame)]);
        let got = pull_zstd(&server, &Auth::None, &raw_ref(raw))
            .await
            .unwrap();
        assert_eq!(got.data, raw);
    }

    /// A `Docker-Content-Digest` is checked against the blob, decoded when it
    /// came as a frame, as `oci-client` checks one on its own pulls.
    #[tokio::test]
    async fn a_transparent_pull_checks_the_digest_the_registry_states() {
        use sha2::Digest;
        install_crypto();
        let raw = crate::zctx::tests::sample(300_000, 7);
        let frame = zstd::bulk::compress(&raw, 3).unwrap();
        let chunk = raw_ref(&raw);
        let hex = |hash: &[u8]| hash.iter().map(|b| format!("{b:02x}")).collect::<String>();
        let sha512 = format!("sha512:{}", hex(&sha2::Sha512::digest(&raw)));
        let other = sha256_hex(b"Dick Jones");

        let accepted = [
            None,
            Some(""),
            Some(chunk.digest.as_str()),
            Some(sha512.as_str()),
        ];
        let mismatch = "Docker-Content-Digest mismatch";
        let two = "2 Docker-Content-Digest fields";
        let md5 = "md5:d41d8cd98f00b204e9800998ecf8427e";
        let refused: [(&[&str], &str); 5] = [
            (&[&other], mismatch),
            (&["sha256"], "names no algorithm"),
            (&[md5], "unsupported algorithm"),
            (&[&chunk.digest, &other], two),
            (&["", &chunk.digest], two),
        ];
        for (encoding, body) in [(None, &raw), (Some("zstd"), &frame)] {
            let response = |stated: &[&str]| {
                let mut headers = Vec::new();
                headers.extend(encoding.map(|e| ("Content-Encoding", e)));
                headers.extend(stated.iter().map(|s| ("Docker-Content-Digest", *s)));
                blob_response_with(&headers, body)
            };
            for stated in accepted {
                let fields: Vec<&str> = stated.into_iter().collect();
                let server = FakeServer::start_bytes(vec![response(&fields)]);
                let got = pull_zstd(&server, &Auth::None, &chunk).await;
                assert_eq!(got.unwrap().data, raw, "{encoding:?}, {stated:?}");
            }
            for (stated, reason) in refused {
                let server = FakeServer::start_bytes(vec![response(stated)]);
                let error = fetcher(&server, true).fetch(&chunk).await.unwrap_err();
                assert!(matches!(error, Error::Format(_)), "{stated:?}: {error}");
                assert!(error.to_string().contains(reason), "{stated:?}: {error}");
                assert_eq!(server.seen().len(), 1, "{stated:?}: asked again");
            }
        }
    }

    fn fetcher(server: &FakeServer, transparent: bool) -> ChunkFetcher {
        ChunkFetcher {
            client: http_client(),
            http: raw_client().build().unwrap(),
            auth: Auth::None,
            image: format!("{}/repo:tag", server.authority()).parse().unwrap(),
            blobs: format!("{}/v2/repo/blobs", server.base()),
            transparent,
        }
    }

    #[tokio::test]
    async fn a_transparent_pull_retry_discards_the_partial_body() {
        install_crypto();
        let raw = crate::zctx::tests::sample(300_000, 5);
        let frame = zstd::bulk::compress(&raw, 3).unwrap();
        let complete = blob_response(Some("zstd"), &frame);
        let cut = complete[..complete.len() - 100].to_vec();
        let server = FakeServer::start_bytes(vec![cut, complete]);
        let chunk = raw_ref(&raw);
        let got = fetcher(&server, true).fetch(&chunk).await.unwrap();
        assert_eq!(got.data, raw);
        let seen = server.seen();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[0].path, seen[1].path);
    }

    /// A refused response is not asked for again.
    #[tokio::test]
    async fn a_transparent_digest_mismatch_is_final() {
        install_crypto();
        let server = FakeServer::start_bytes(vec![blob_response(None, b"corrupt")]);
        let error = fetcher(&server, true)
            .fetch(&raw_ref(b"correct"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("digest mismatch"), "{error}");
        assert_eq!(server.seen().len(), 1);
    }

    /// A few files, one of them several chunks long, under a new directory.
    fn sample_tree(tag: &str) -> (TempDir, Vec<String>) {
        let dir = TempDir::new(tag);
        std::fs::create_dir(dir.join("sub")).unwrap();
        let files = [("big.bin", 6 << 20), ("sub/small.bin", 1000), ("empty", 0)];
        for (i, (path, len)) in files.iter().enumerate() {
            std::fs::write(dir.join(path), crate::zctx::tests::sample(*len, i as u64)).unwrap();
        }
        (dir, files.iter().map(|(p, _)| p.to_string()).collect())
    }

    fn assert_same_tree(src: &Path, dst: &Path, paths: &[String]) {
        for path in paths {
            assert!(
                std::fs::read(src.join(path)).unwrap() == std::fs::read(dst.join(path)).unwrap(),
                "{path} differs"
            );
        }
    }

    async fn open(registry: &Registry, cas: Option<&Path>) -> Store {
        Store::open(
            &format!("{}/repo", registry.authority()),
            RemoteOptions {
                plain_http: true,
                cache_dir: cas.map(Path::to_path_buf),
                ..Default::default()
            },
        )
        .await
        .unwrap()
    }

    fn chunk_digests(idx: &Index) -> Vec<&str> {
        idx.chunks.iter().map(|c| c.digest.as_str()).collect()
    }

    /// An entry pushed to a transparent-zstd registry restores the same bytes
    /// whether its chunks come back as the stored frames or decoded on the way,
    /// through a local chunk store or without one; each is asked for as a frame.
    #[tokio::test]
    async fn a_transparent_entry_restores_from_frames_and_from_decoded_chunks() {
        install_crypto();
        let registry = Registry::start(true);
        let (src, paths) = sample_tree("src");
        let store = open(&registry, None).await;
        assert!(store.transparent());
        store
            .push("tag", &src, &paths, BTreeMap::new())
            .await
            .unwrap();

        let mut asked = 0;
        for decode in [false, true] {
            registry.behaviour().decode.store(decode, Ordering::Relaxed);
            for cas in [None, Some(TempDir::new("cas"))] {
                let dst = TempDir::new("dst");
                let store = open(&registry, cas.as_deref()).await;
                let (idx, _) = store.pull("tag", &dst).await.unwrap();
                assert!(idx.transparent);
                assert_same_tree(&src, &dst, &paths);

                let digests = chunk_digests(&idx);
                assert!(digests.len() > 2, "{} chunks", digests.len());
                for digest in &digests {
                    let (stored, zstd) = registry.blob(digest).unwrap();
                    assert!(zstd, "{digest} is not stored as a frame");
                    assert_ne!(sha256_hex(&stored), **digest);
                }
                let gets = registry.blob_gets(digests.iter().copied());
                asked += digests.len();
                assert_eq!(gets.len(), asked, "decode: {decode}, cas: {cas:?}");
                for get in gets {
                    assert_eq!(get.accept_encoding.as_deref(), Some("zstd"));
                }
            }
        }
    }

    /// A chunk whose bytes are not the length its reference gives fails the
    /// restore under either scheme, whether it is fetched or already in the
    /// local chunk store.
    #[tokio::test]
    async fn a_chunk_of_another_size_than_its_reference_fails_the_restore() {
        install_crypto();
        for transparent in [false, true] {
            let registry = Registry::start(transparent);
            let (src, paths) = sample_tree("src");
            let store = open(&registry, None).await;
            store
                .push("tag", &src, &paths, BTreeMap::new())
                .await
                .unwrap();

            for (delta, warmed) in [(-1, false), (-1, true), (1, false), (1, true)] {
                let cas = TempDir::new("cas");
                let store = open(&registry, Some(&cas)).await;
                if warmed {
                    store.pull("tag", &TempDir::new("dst")).await.unwrap();
                }
                let mut pulled = store.pull_index("tag").await.unwrap();
                assert_eq!(pulled.index.transparent, transparent);
                pulled.index.chunks[0].size += delta;
                let dst = TempDir::new("dst");
                let error = match store.assemble_pulled(&pulled, &dst, None).await {
                    Ok(_) => panic!("restored (transparent: {transparent}, warm: {warmed})"),
                    Err(error) => error.to_string(),
                };
                // A frame fetched for a smaller chunk is cut a byte past its
                // size; one already stored is held to it once decoded.
                let said = error.contains("index says") || error.contains("exceeds");
                assert!(said, "transparent: {transparent}, warm: {warmed}: {error}");
            }
        }
    }

    /// An index is assembled by the store that pulled it: its chunks are not
    /// looked for in another repository.
    #[tokio::test]
    async fn an_index_pulled_from_another_repository_is_refused() {
        install_crypto();
        let registry = Registry::start(true);
        let (src, paths) = sample_tree("src");
        let store = open(&registry, None).await;
        store
            .push("tag", &src, &paths, BTreeMap::new())
            .await
            .unwrap();
        let pulled = store.pull_index("tag").await.unwrap();

        let other = Store::open(
            &format!("{}/other", registry.authority()),
            RemoteOptions {
                plain_http: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let dst = TempDir::new("dst");
        let Err(error) = other.assemble_pulled(&pulled, &dst, None).await else {
            panic!("assembled from another repository");
        };
        assert!(
            error.to_string().contains("was not pulled from this store"),
            "{error}"
        );
        let digests = chunk_digests(&pulled.index);
        assert_eq!(registry.blob_gets(digests).len(), 0);
    }

    /// A transparent-zstd registry with the sample tree pushed as `tag`:
    /// the tree, its paths and its index.
    async fn pushed_transparent() -> (Registry, TempDir, Vec<String>, Index) {
        let registry = Registry::start(true);
        let (src, paths) = sample_tree("src");
        let store = open(&registry, None).await;
        store
            .push("tag", &src, &paths, BTreeMap::new())
            .await
            .unwrap();
        let idx = store.pull_index("tag").await.unwrap().index;
        assert!(idx.transparent);
        (registry, src, paths, idx)
    }

    /// The local store keeps a chunk as the frame the registry sent, a later
    /// restore reads it from there, and a chunk that arrived decoded is kept
    /// as its bytes.
    #[tokio::test]
    async fn the_local_store_keeps_frames_and_reuses_them() {
        install_crypto();
        let (registry, src, paths, idx) = pushed_transparent().await;
        let digests = chunk_digests(&idx);
        let cas = TempDir::new("cas");

        let dst = TempDir::new("dst");
        let store = open(&registry, Some(&cas)).await;
        store.pull("tag", &dst).await.unwrap();
        assert_same_tree(&src, &dst, &paths);
        assert!(!cas.join("sha256").exists());
        for digest in &digests {
            let frame = std::fs::read(cas.join("zstd").join(digest_hex(digest).unwrap())).unwrap();
            assert_eq!(frame, registry.blob(digest).unwrap().0);
        }
        let fetched = registry.blob_gets(digests.iter().copied()).len();
        assert_eq!(fetched, digests.len());

        let dst = TempDir::new("dst");
        open(&registry, Some(&cas))
            .await
            .pull("tag", &dst)
            .await
            .unwrap();
        assert_same_tree(&src, &dst, &paths);
        assert_eq!(registry.blob_gets(digests.iter().copied()).len(), fetched);

        // A proxy decoding on the way: the bytes themselves are what arrived.
        registry.behaviour().decode.store(true, Ordering::Relaxed);
        let (cas, dst) = (TempDir::new("cas"), TempDir::new("dst"));
        open(&registry, Some(&cas))
            .await
            .pull("tag", &dst)
            .await
            .unwrap();
        assert_same_tree(&src, &dst, &paths);
        assert!(!cas.join("zstd").exists());
        for digest in &digests {
            let raw = std::fs::read(cas.join("sha256").join(digest_hex(digest).unwrap())).unwrap();
            assert_eq!(sha256_hex(&raw), **digest);
        }
    }

    /// A local store filled before frames were kept holds the bytes of each
    /// chunk under `sha256`: they are restored from as they are.
    #[tokio::test]
    async fn the_local_store_reuses_entries_kept_decoded() {
        install_crypto();
        let (registry, src, paths, idx) = pushed_transparent().await;
        let digests = chunk_digests(&idx);
        let cas = TempDir::new("cas");
        std::fs::create_dir(cas.join("sha256")).unwrap();
        for digest in &digests {
            let raw = zstd::stream::decode_all(registry.blob(digest).unwrap().0.as_slice());
            std::fs::write(
                cas.join("sha256").join(digest_hex(digest).unwrap()),
                raw.unwrap(),
            )
            .unwrap();
        }

        let dst = TempDir::new("dst");
        open(&registry, Some(&cas))
            .await
            .pull("tag", &dst)
            .await
            .unwrap();
        assert_same_tree(&src, &dst, &paths);
        assert!(registry.blob_gets(digests.iter().copied()).is_empty());
        assert!(!cas.join("zstd").exists());
    }

    /// An index giving a chunk a smaller size than its stored frame decodes to
    /// fails the restore and leaves the frame: the entry is what its digest
    /// names, and the right index restores from it without fetching.
    #[tokio::test]
    async fn an_understated_size_fails_the_restore_and_keeps_the_stored_frame() {
        install_crypto();
        let (registry, src, paths, idx) = pushed_transparent().await;
        let digests = chunk_digests(&idx);
        let cas = TempDir::new("cas");
        let store = open(&registry, Some(&cas)).await;
        store.pull("tag", &TempDir::new("dst")).await.unwrap();
        let fetched = registry.blob_gets(digests.iter().copied()).len();

        let mut pulled = store.pull_index("tag").await.unwrap();
        pulled.index.chunks[0].size -= 1;
        let dst = TempDir::new("dst");
        let error = match store.assemble_pulled(&pulled, &dst, None).await {
            Ok(_) => panic!("restored from an index of another size"),
            Err(error) => error.to_string(),
        };
        let says = format!("index says {}", pulled.index.chunks[0].size);
        assert!(error.contains(&says), "{error}");
        assert!(std::fs::read_dir(&dst).unwrap().next().is_none());

        store.pull("tag", &dst).await.unwrap();
        assert_same_tree(&src, &dst, &paths);
        assert_eq!(registry.blob_gets(digests.iter().copied()).len(), fetched);
    }

    /// A frame gone bad in the local store fails the restore that reads it,
    /// which leaves nothing behind; the entry is dropped, so the next restore
    /// fetches that chunk again.
    #[tokio::test]
    async fn a_corrupt_frame_in_the_local_store_is_never_restored() {
        install_crypto();
        let (registry, src, paths, idx) = pushed_transparent().await;
        let digests = chunk_digests(&idx);
        let cas = TempDir::new("cas");
        let store = open(&registry, Some(&cas)).await;
        store.pull("tag", &TempDir::new("dst")).await.unwrap();
        let fetched = registry.blob_gets(digests.iter().copied()).len();

        // The frame of another chunk: it decodes, to the wrong bytes.
        let entry = |digest: &str| cas.join("zstd").join(digest_hex(digest).unwrap());
        let (last, first) = (digests[digests.len() - 1], digests[0]);
        assert_ne!(last, first);
        std::fs::copy(entry(first), entry(last)).unwrap();

        let dst = TempDir::new("dst");
        let Err(error) = store.pull("tag", &dst).await else {
            panic!("restored from a corrupt entry");
        };
        assert!(error.to_string().contains("corrupt CAS entry"), "{error}");
        assert!(std::fs::read_dir(&dst).unwrap().next().is_none());
        assert!(!entry(last).exists());
        assert_eq!(registry.blob_gets(digests.iter().copied()).len(), fetched);

        store.pull("tag", &dst).await.unwrap();
        assert_same_tree(&src, &dst, &paths);
        assert_eq!(registry.blob_gets([last]).len(), 2);
        assert_eq!(
            registry.blob_gets(digests.iter().copied()).len(),
            fetched + 1
        );
    }

    /// A registry that does not advertise the scheme keeps compressed-digest
    /// chunks, pulled as the blobs they are.
    #[tokio::test]
    async fn a_plain_registry_keeps_the_compressed_digest_scheme() {
        install_crypto();
        let registry = Registry::start(false);
        let (src, paths) = sample_tree("src");
        let store = open(&registry, None).await;
        assert!(!store.transparent());
        store
            .push("tag", &src, &paths, BTreeMap::new())
            .await
            .unwrap();

        let dst = TempDir::new("dst");
        let cas = TempDir::new("cas");
        let store = open(&registry, Some(&cas)).await;
        let (idx, _) = store.pull("tag", &dst).await.unwrap();
        assert!(!idx.transparent);
        assert_same_tree(&src, &dst, &paths);
        let digests = chunk_digests(&idx);
        for digest in &digests {
            let (stored, zstd) = registry.blob(digest).unwrap();
            assert!(!zstd);
            assert_eq!(sha256_hex(&stored), **digest);
        }
        let gets = registry.blob_gets(digests.iter().copied());
        assert_eq!(gets.len(), digests.len());
        for get in gets {
            assert_eq!(get.accept_encoding, None);
        }
    }

    /// A registry that answers the probe, serves the first chunk GET corrupt
    /// and holds every later connection open without answering, as a proxy
    /// whose upstream has gone does. Returns its authority, the count of
    /// chunk GETs it received and the flag that, once set, releases the silent
    /// connections.
    fn one_corrupt_then_silent(transparent: bool) -> (String, Arc<AtomicI64>, Arc<AtomicBool>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let gets = Arc::new(AtomicI64::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (gets_, stop_) = (gets.clone(), stop.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let (gets, stop) = (gets_.clone(), stop_.clone());
                std::thread::spawn(move || {
                    let mut line = String::new();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let _ = reader.read_line(&mut line);
                    loop {
                        let mut h = String::new();
                        if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                            break;
                        }
                    }
                    let response = if line.starts_with("GET /v2/ ") && transparent {
                        "HTTP/1.1 200 OK\r\nx-virtkit-transparent-zstd: 1\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                    } else if line.starts_with("GET /v2/ ") {
                        "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                    } else if gets.fetch_add(1, Ordering::Relaxed) == 0 {
                        "HTTP/1.1 200 OK\r\nContent-Length: 7\r\nConnection: close\r\n\r\ncorrupt"
                    } else {
                        while !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        return;
                    };
                    let _ = stream.write_all(response.as_bytes());
                });
            }
        });
        (authority, gets, stop)
    }

    /// Once a chunk has failed for good, the prefetch ends: the chunks still
    /// queued are never requested and the ones in flight are abandoned, rather
    /// than each waiting out its own read timeout and retries.
    #[tokio::test]
    async fn a_failed_chunk_ends_the_prefetch() {
        a_failed_chunk_ends_the_prefetch_of(false).await;
    }

    /// The same when the chunks are asked for compressed.
    #[tokio::test]
    async fn a_failed_chunk_ends_the_transparent_prefetch() {
        a_failed_chunk_ends_the_prefetch_of(true).await;
    }

    async fn a_failed_chunk_ends_the_prefetch_of(transparent: bool) {
        install_crypto();
        let (authority, gets, stop) = one_corrupt_then_silent(transparent);
        let cas = cas_tmp("ends");
        let store = Store::open(
            &format!("{authority}/repo"),
            RemoteOptions {
                plain_http: true,
                cache_dir: Some(cas.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(store.transparent(), transparent);
        let mut idx = Index::new();
        idx.transparent = transparent;
        idx.chunks = (0..3 * FETCH_CONCURRENCY)
            .map(|i| {
                // As long as the `corrupt` served: it is refused for its
                // digest, whichever chunk is asked for first.
                let data = format!("chunk{i:02}");
                ChunkRef {
                    digest: sha256_hex(data.as_bytes()),
                    size: data.len() as i64,
                    raw_size: data.len() as i64,
                }
            })
            .collect();
        let image = store.reference("tag").unwrap();

        // Well under one read timeout: a prefetch that waits on the silent
        // connections has not stopped at the failure.
        let outcome =
            tokio::time::timeout(Duration::from_secs(10), store.chunk_source(&image, &idx)).await;
        stop.store(true, Ordering::Relaxed);
        std::fs::remove_dir_all(&cas).unwrap();
        let error = outcome
            .expect("the prefetch ends at the first failed chunk")
            .err()
            .expect("the corrupt chunk fails the prefetch");
        assert!(error.to_string().contains("digest"), "{error}");
        let requested = gets.load(Ordering::Relaxed) as usize;
        assert!(
            (1..=FETCH_CONCURRENCY).contains(&requested),
            "{requested} chunks requested after the first failed"
        );
    }

    /// A registry that answers the probe and refuses everything else with a
    /// 500, which is final at once. Returns its authority and the count of
    /// refusals.
    fn refusing_registry() -> (String, Arc<AtomicI64>) {
        use std::io::{BufRead, BufReader, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let authority = listener.local_addr().unwrap().to_string();
        let refused = Arc::new(AtomicI64::new(0));
        let refused_ = refused.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                }
                let response = if line.starts_with("GET /v2/ ") {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}"
                } else {
                    refused_.fetch_add(1, Ordering::Relaxed);
                    "HTTP/1.1 500 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                };
                // Best-effort: the client may have gone.
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (authority, refused)
    }

    /// A sparse file is one chunk repeated: the builder emits it once and then
    /// chunks on without sending anything. The upload of that one chunk
    /// failing must end the push there, builder included, rather than once
    /// the rest of the file has been read.
    #[tokio::test]
    async fn a_failed_upload_ends_the_push_while_the_builder_sends_nothing() {
        install_crypto();
        let (authority, refused) = refusing_registry();
        let store = Store::open(
            &format!("{authority}/repo"),
            RemoteOptions {
                plain_http: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        // A minute or more of chunking, were the push to wait for the end of
        // the file.
        let dir = cas_tmp("sparse");
        std::fs::File::create(dir.join("sparse"))
            .unwrap()
            .set_len(16 << 30)
            .unwrap();

        // The push returns once its builder has stopped, so its returning at
        // all is the builder having been cancelled.
        let outcome = tokio::time::timeout(
            Duration::from_secs(10),
            store.push("tag", &dir, &["sparse".to_string()], BTreeMap::new()),
        )
        .await;
        std::fs::remove_dir_all(&dir).unwrap();
        let error = outcome
            .expect("the push ends at the failed upload")
            .expect_err("the refused chunk fails the push");
        assert!(error.to_string().contains("500"), "{error}");
        assert_eq!(
            refused.load(Ordering::Relaxed),
            1,
            "one distinct chunk, so one request: its existence check, and a 500 is not retried"
        );
    }

    fn registry_error(code: &str) -> OciDistributionError {
        let envelope: oci_client::errors::OciEnvelope = serde_json::from_str(&format!(
            r#"{{"errors":[{{"code":"{code}","message":"x"}}]}}"#
        ))
        .expect("envelope");
        OciDistributionError::RegistryError {
            envelope,
            url: "https://reg/v2/r/manifests/t".to_string(),
        }
    }

    /// A registry's 404 envelope is a miss; anything else it reports is an error.
    #[test]
    fn a_missing_manifest_is_a_miss_not_an_error() {
        assert!(is_missing(&registry_error("MANIFEST_UNKNOWN")));
        assert!(is_missing(&registry_error("NAME_UNKNOWN")));
        assert!(!is_missing(&registry_error("UNAUTHORIZED")));
        assert!(!is_missing(&registry_error("DENIED")));
        assert!(is_missing(
            &OciDistributionError::ImageManifestNotFoundError("x".to_string())
        ));
    }
}
