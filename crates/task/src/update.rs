//! `task --update`: replace this binary with a published GitHub release build.
//!
//! A release ships one archive per platform (`task-<os>-<arch>.tar.gz`, `.zip`
//! on Windows) next to the `sha256sum` sidecar CI generates for it, and the
//! binary carries what it needs inside it — so an update is one download: fetch
//! the archive, check it against the published digest, take the `task` member
//! out of it, and rename that over the running binary. On Unix the rename is
//! atomic and leaves the live process untouched (it keeps the old inode; only
//! the directory entry moves), so an update never interrupts a run already in
//! flight.
//!
//! Nothing is written before the user confirms the version they are moving to,
//! and the replacement only happens once the download hashes to the digest
//! published beside it and the extracted binary reports its own version when
//! run. Both gates are integrity checks against one release rather than proof of
//! who published it: they catch a corrupted or truncated transfer and a build
//! that cannot run here, which is why the tag a release is looked up by has to
//! be trusted in its own right — see [`release_tag`]. [`check`] stops after
//! resolving the release, making it a read-only query for what is available.
//!
//! Ported from virtkit's `vk-selfupdate` crate, which does the same for `vk`;
//! the two are expected to stay recognisably the same code. What differs here is
//! that a task release publishes an archive rather than a bare binary, so this
//! also has to extract from one, and that `task` ships for Windows.

use std::cmp::Ordering;
use std::fs::{self, OpenOptions};
use std::io::{self, ErrorKind, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use sha2::{Digest, Sha256};
use taskcore::executor::Prompter;

use crate::prompter::CliPrompter;

/// The repository releases are published from.
const REPO: &str = "wallix/task-rs";
/// GitHub's REST API root. Threaded through as an argument rather than read from
/// this constant at each call site, so the tests can aim the same code at a
/// local server.
const API: &str = "https://api.github.com";
/// The binary this replaces: the file inside the release archive, and the name
/// its own output uses.
const BIN: &str = if cfg!(windows) { "task.exe" } else { "task" };
/// Mode to install with when the binary being replaced has none to copy.
#[cfg(unix)]
const INSTALL_MODE: u32 = 0o755;
/// A sidecar is one `sha256sum` line, and a release JSON a few kilobytes of it.
/// Reading an unbounded body into memory to find out it is neither is how a
/// hostile mirror exhausts the host's RAM.
const MAX_SIDECAR: usize = 4096;
const MAX_RELEASE_JSON: usize = 1024 * 1024;
/// The largest archive a release can plausibly ship — they are under ten
/// megabytes. The download's own length check is relative to what the release
/// announced, so without a ceiling on that number a release claiming half a
/// terabyte would be honoured; and the archive is held in memory to be
/// extracted from, so that ceiling is also what one update may allocate.
const MAX_ASSET: u64 = 64 * 1024 * 1024;
/// Connect and per-read timeouts. `--check` is meant for cron and login banners,
/// so a black-holed endpoint has to fail instead of hanging forever; a per-read
/// deadline bounds a stalled connection without capping how long a download may
/// take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

/// The subset of GitHub's release JSON we read.
#[derive(serde::Deserialize)]
struct ApiRelease {
    tag_name: String,
    assets: Vec<ApiAsset>,
}

#[derive(serde::Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    size: u64,
}

/// A release's archive for this platform, resolved and ready to fetch.
#[derive(Debug)]
struct Target {
    tag: String,
    /// the archive's asset name, which is also its line in the sidecar
    archive: String,
    url: String,
    /// download URL of the archive's `sha256sum` sidecar
    digest_url: String,
    size: u64,
}

/// How a release relates to the version this binary was built as.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// the release is the version already installed
    Same,
    /// strictly newer — the only case `--check` reports as an update waiting
    Newer,
    /// a different version that is not newer: an older release — installable,
    /// which is what naming one on the command line is for — or a tag carrying
    /// no version to order at all, which [`smoke_test`] then refuses because the
    /// build cannot report the tag's own name as its version.
    Other,
}

/// A tag name checked to be safe in the API URL's path. Naming a release goes
/// through [`release_tag`], the only constructor, so no later caller can route
/// an unchecked string into the URL.
#[derive(Debug, PartialEq, Eq)]
struct ReleaseTag(String);

impl std::fmt::Display for ReleaseTag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// What both entry points resolve before deciding anything: the binary they
/// would replace, the release on offer, and how the two relate.
struct Plan {
    exe: PathBuf,
    client: reqwest::Client,
    target: Target,
    step: Step,
}

/// The version this binary was built as, which a release's tag is compared
/// against to decide whether installing it moves forward.
fn current_version() -> &'static str {
    taskcore::version::get_version()
}

/// Move this binary to `tag`'s release build, or to the latest release when no
/// tag is given. Prompts before replacing it unless `assume_yes`.
pub async fn update(tag: Option<&str>, assume_yes: bool) -> Result<()> {
    let plan = plan(API, tag).await?;
    if plan.step == Step::Same {
        println!(
            "task is already at {} ({})",
            current_version(),
            plan.exe.display()
        );
        return Ok(());
    }
    // The download lands in the installed binary's own directory, so publishing
    // it is a rename on the same filesystem rather than a copy.
    let dir = plan
        .exe
        .parent()
        .with_context(|| format!("{} has no parent directory", plan.exe.display()))?;

    // The pre-confirmation summary is interactive framing, so it goes to stderr
    // with the prompt (which is unreadable without it) and leaves stdout to the
    // outcome.
    let aside = match plan.step {
        Step::Other => " — not a newer release",
        // An upgrade is the expected case and needs no aside; `Same` returned above.
        _ => "",
    };
    eprintln!(
        "task {} -> {} ({}){aside}",
        current_version(),
        plan.target.tag,
        human_bytes(plan.target.size)
    );
    eprintln!("  replacing {}", plan.exe.display());
    if !assume_yes && !confirm()? {
        eprintln!("update cancelled");
        return Ok(());
    }

    install(&plan.client, &plan.target, &plan.exe, dir).await?;
    println!("task updated to {}", plan.target.tag);
    Ok(())
}

/// Report how the release `tag` names — or the latest one when no tag is given —
/// compares to this binary, without downloading it. Returns true only for a
/// strictly newer release, which the caller turns into the exit code a script
/// can branch on.
pub async fn check(tag: Option<&str>) -> Result<bool> {
    let plan = plan(API, tag).await?;
    // An explicit tag is whatever the user named; only the default is "the latest".
    let label = match tag {
        Some(_) => "release",
        None => "latest release",
    };
    let found = &plan.target.tag;
    println!("task {} ({})", current_version(), plan.exe.display());
    match plan.step {
        Step::Same => {
            println!("  {label} {found} — up to date");
            Ok(false)
        }
        Step::Newer => {
            // Name the version in the hint, so the line works as-is for a
            // release the user asked about by name.
            let hint = match tag {
                Some(_) => format!("task --update={found}"),
                None => "task --update".to_string(),
            };
            println!(
                "  {label} {found} available ({}) — run `{hint}` to install it",
                human_bytes(plan.target.size)
            );
            Ok(true)
        }
        // Not an update: exit 0, so a `--check` in cron or a login banner stays
        // quiet about a release older than the build in place, and about a tag
        // whose version cannot be ordered against it.
        Step::Other => {
            println!("  {label} {found} is not newer than this build");
            Ok(false)
        }
    }
}

/// Resolve the release `tag` names — or the latest published one — and work out
/// what installing it would do to this binary.
async fn plan(api: &str, tag: Option<&str>) -> Result<Plan> {
    let running = std::env::current_exe().context("locating the running task binary")?;
    // Resolved, not taken as reported: `current_exe` is `/proc/self/exe` on Linux
    // (already resolved) but the load path on macOS and Windows, which may run
    // through a symlink. Renaming onto the link would replace it with a regular
    // file — leaving a Homebrew or `~/bin` install pointing at nothing it manages
    // and the real binary stale — and the platforms would disagree about which
    // file an update touched. Canonicalizing also makes `dir` the real file's
    // directory, which is what keeps publishing a same-filesystem rename.
    //
    // Falls back to the unresolved path rather than failing: once the file this
    // process was loaded from is unlinked, Linux reports it as `…/task (deleted)`
    // and canonicalizing that fails. `--check` writes nothing and still has a
    // useful answer in that state, and `install` has its own check that names
    // what happened — failing here would replace that message with a worse one.
    let exe = fs::canonicalize(&running).unwrap_or(running);
    let client = http_client(api)?;
    let target = resolve(&client, api, tag).await?;
    let step = step(current_version(), &target.tag);
    Ok(Plan {
        exe,
        client,
        target,
        step,
    })
}

/// An HTTP client identifying itself: GitHub's API rejects requests without a
/// `User-Agent`.
///
/// Bound to TLS whenever `api` is, and that has to hold for the whole redirect
/// chain, not just the first request: a release asset URL always redirects to
/// the CDN, and the default policy would follow an `https -> http` hop, which is
/// exactly the downgrade the per-asset scheme check in [`resolve`] is there to
/// refuse. `https_only` makes the client refuse it on every hop.
fn http_client(api: &str) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(format!("task/{}", current_version()))
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        // Conditional so the tests can drive the whole flow against a local
        // `http://` server; every real invocation goes through `API`.
        .https_only(api.starts_with("https://"))
        .build()
        .context("building the HTTP client")
}

/// This host's release archive and checksum names. Keep in sync with
/// `package.sh` and `install-task.sh`.
fn asset_names() -> Result<(String, String)> {
    let os = match std::env::consts::OS {
        os @ ("linux" | "macos" | "windows") => os,
        other => bail!("no task release is published for {other}"),
    };
    let arch = match std::env::consts::ARCH {
        arch @ ("x86_64" | "aarch64") => arch,
        other => bail!("no task release is published for {os} {other}"),
    };
    let stem = format!("task-{os}-{arch}");
    let ext = if os == "windows" { "zip" } else { "tar.gz" };
    Ok((format!("{stem}.{ext}"), format!("{stem}.sha256")))
}

/// Resolve a release — `tag`'s, or the latest published one — to this platform's
/// archive.
async fn resolve(client: &reqwest::Client, api: &str, tag: Option<&str>) -> Result<Target> {
    let (archive, sidecar) = asset_names()?;
    // The user's tag crosses the trust boundary once, here; the checked form is
    // what both the URL and the error message below are built from.
    let tag = tag.map(release_tag).transpose()?;
    let url = api_url(api, tag.as_ref());
    let resp = client
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .with_context(|| format!("querying {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        if rate_limited(&resp) {
            bail!(
                "GitHub's API rate limit is exhausted for this host (HTTP {status}) — retry later"
            );
        }
        // 404 on the tags endpoint is the common case: a version that was never
        // released, or spelled differently than the tag.
        match &tag {
            Some(t) => bail!("no release {t} in {REPO} (HTTP {status})"),
            None => bail!("no latest release in {REPO} (HTTP {status})"),
        }
    }
    let body = bounded_body(resp, MAX_RELEASE_JSON, &url).await?;
    let release: ApiRelease = serde_json::from_slice(&body)
        .with_context(|| format!("parsing the release JSON from {url}"))?;
    // The tag goes straight into the confirmation prompt the user answers, so it
    // may not carry the control bytes that would let it rewrite the line around
    // the question.
    if release.tag_name.chars().any(char::is_control) {
        bail!("the release's tag name is not printable");
    }
    let asset = pick(&release.assets, &archive)?;
    let digest = pick(&release.assets, &sidecar)?;
    if asset.size > MAX_ASSET {
        bail!(
            "the release's {archive} asset is {} — larger than a task release can be",
            human_bytes(asset.size),
        );
    }
    // The release told us where its assets live; require them on the scheme the
    // API was itself reached over, so a response cannot quietly move the
    // transfer to cleartext — the sidecar would move with it, leaving the digest
    // gate none the wiser. This checks the URL we were handed; the client is
    // built `https_only` for a real API, which is what holds the guarantee
    // across the redirect every asset URL goes through.
    let scheme = format!("{}://", api.split_once("://").map_or("https", |(s, _)| s));
    for a in [asset, digest] {
        if !a.browser_download_url.starts_with(&scheme) {
            bail!("the release's {} asset is not served over {scheme}", a.name);
        }
    }
    Ok(Target {
        tag: release.tag_name,
        archive,
        url: asset.browser_download_url.clone(),
        digest_url: digest.browser_download_url.clone(),
        size: asset.size,
    })
}

/// Make `target`'s release build the binary at `exe`. Every failure — a bad
/// download, a gate it does not pass, a rename that cannot happen — leaves `exe`
/// as it was and nothing extra in `dir`.
async fn install(client: &reqwest::Client, target: &Target, exe: &Path, dir: &Path) -> Result<()> {
    // `current_exe` is the on-disk *pathname*, not the `/proc/self/exe` magic
    // link: once the file this process was loaded from is unlinked, the kernel
    // reports it as `…/task (deleted)`, and renaming onto that would install a
    // binary nobody runs while reporting success. Checked before the download,
    // so a pointless multi-megabyte transfer is not how the user finds out.
    // `metadata`, not `is_file`, so a directory that cannot be traversed is
    // reported as itself instead of as the binary having disappeared.
    match fs::metadata(exe) {
        Ok(m) if m.is_file() => {}
        Ok(_) => bail!("{} is not a regular file", exe.display()),
        Err(e) if e.kind() == ErrorKind::NotFound => bail!(
            "{} is gone — task was replaced while this ran; rerun task --update",
            exe.display(),
        ),
        Err(e) => {
            return Err(
                anyhow::Error::new(e).context(format!("checking the binary at {}", exe.display()))
            );
        }
    }
    // A dotfile beside the installed binary: same filesystem as `exe`, so
    // publishing it is a rename. The pid keeps two updates running at once off
    // each other's file.
    //
    // That path is re-resolved by the exec, the rename and the cleanup rather
    // than held as an fd, which the guidelines say to treat as a TOCTOU bug
    // until argued otherwise: winning the race needs write access to `dir`, and
    // whoever has that can replace the binary outright without going near this
    // code.
    let tmp = dir.join(tmp_name());
    // Created before the download, for the same reason `exe` is checked before
    // it: an unwritable install directory is the ordinary way this fails, and
    // waiting out a multi-megabyte transfer is not how the user should find out.
    // The file is empty until `extract` writes it, so nothing lands next to the
    // installed binary before the digest gate has passed. A path that already
    // exists is refused untouched, leaving the file for the user the error tells
    // to remove it.
    let file = create_tmp(&tmp, dir)?;
    let outcome = async {
        let archive = download(client, target).await?;
        unpack(&archive, target, file, &tmp, exe)?;
        publish(&tmp, exe, dir)
    }
    .await;
    if outcome.is_err() {
        // Best-effort: an unverified or unpublished binary must not be left
        // lying next to the installed one, but the original error is what the
        // user needs to see.
        let _ = fs::remove_file(&tmp);
    }
    outcome
}

/// The dotfile the release binary is extracted into, beside the binary it is
/// replacing.
fn tmp_name() -> String {
    format!(".task-update.{}", std::process::id())
}

/// Create the file the binary is extracted into, refusing to reuse anything
/// already at that path: 0600 while the contents are unverified — it gains an
/// execute bit for the smoke test (0700, still owner-only) and reaches the
/// install mode only once that has passed — and `create_new`, so a symlink or a
/// file planted here is refused rather than written through.
fn create_tmp(tmp: &Path, dir: &Path) -> Result<fs::File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(tmp).map_err(|e| match e.kind() {
        ErrorKind::AlreadyExists => anyhow!(
            "{} already exists — an interrupted task update left it behind, and nothing here \
             reuses it; it is safe to remove",
            tmp.display(),
        ),
        _ => anyhow::Error::new(e).context(format!(
            "creating {} (is {} writable?)",
            tmp.display(),
            dir.display()
        )),
    })
}

/// Download `target`'s archive and check it against the digest published beside
/// it. Held in memory rather than spooled to disk: it is bounded by
/// [`MAX_ASSET`], and nothing may be written next to the installed binary until
/// the digest gate has passed.
async fn download(client: &reqwest::Client, target: &Target) -> Result<Vec<u8>> {
    let want = digest(client, target).await?;
    let mut resp = client
        .get(&target.url)
        .send()
        .await
        .with_context(|| format!("downloading {}", target.url))?
        .error_for_status()
        .with_context(|| format!("downloading {}", target.url))?;

    let mut bar = Progress::new(&target.archive, target.size);
    let mut hasher = Sha256::new();
    let mut body: Vec<u8> = Vec::new();
    loop {
        let chunk = resp
            .chunk()
            .await
            .with_context(|| format!("downloading {}", target.url))?;
        let Some(chunk) = chunk else { break };
        // Stop at the length the release announced: the digest can only be
        // checked once the whole body is down, and until then an endless
        // response would fill the host's memory.
        if body.len().saturating_add(chunk.len()) as u64 > target.size {
            bail!(
                "{} is longer than the {} bytes the release announced",
                target.url,
                target.size
            );
        }
        hasher.update(&chunk);
        body.extend_from_slice(&chunk);
        bar.advance(body.len() as u64);
    }
    // Cleared whether or not the body arrived, so a failure's message is not
    // printed under a stalled progress line.
    bar.clear();

    let got = hasher.finalize();
    if got.as_slice() != want {
        let shown = |b: &[u8]| b.iter().map(|b| format!("{b:02x}")).collect::<String>();
        bail!(
            "{} does not match the published digest (got {}, want {})",
            target.url,
            shown(&got),
            shown(&want)
        );
    }
    Ok(body)
}

/// This platform's expected sha256, from the sidecar CI publishes beside its
/// archive.
async fn digest(client: &reqwest::Client, target: &Target) -> Result<[u8; 32]> {
    let resp = client
        .get(&target.digest_url)
        .send()
        .await
        .with_context(|| format!("downloading {}", target.digest_url))?
        .error_for_status()
        .with_context(|| format!("downloading {}", target.digest_url))?;
    let body = bounded_body(resp, MAX_SIDECAR, &target.digest_url).await?;
    let text =
        std::str::from_utf8(&body).with_context(|| format!("{} is not text", target.digest_url))?;
    parse_digest(text, &target.archive)
        .with_context(|| format!("parsing the digest from {}", target.digest_url))
}

/// Take the `task` binary out of the verified `archive` into `file`, and leave it
/// executable, durable and smoke-tested — ready to be renamed into place.
fn unpack(
    archive: &[u8],
    target: &Target,
    mut file: fs::File,
    tmp: &Path,
    exe: &Path,
) -> Result<()> {
    extract(archive, target, &mut file)?;
    // Owner-only execute for the smoke test, so the file stays unreachable to
    // anyone else while its contents are still unproven — `create_tmp`'s 0600
    // cannot be kept, because `execve` needs an execute bit. The install mode
    // goes on after the exec, so the window at the wider mode is the rename
    // rather than the whole exec.
    //
    // Through the open fd rather than the path: there is nothing to re-resolve.
    // Set here and not at creation because `open(2)` masks its mode argument
    // with the umask, so asking for 0700 there yields no bits at all under a
    // hostile umask.
    set_file_mode(&file, 0o700, tmp)?;
    // `sync_all`, not `flush`: `File`'s flush is a no-op, so without this a host
    // crash just after the rename could leave a truncated binary behind — and
    // for `task` that is the tool a build depends on.
    file.sync_all()
        .with_context(|| format!("flushing {} to disk", tmp.display()))?;
    // Identity of the file the smoke test is about to run, so the reopen below
    // can prove it got the same one back.
    let ran = file_id(&file);
    // Closed before the exec below, not merely at the end of scope: `execve`
    // refuses a file any process still holds open for writing (ETXTBSY).
    drop(file);

    smoke_test(tmp, version_of(&target.tag))?;
    // Reopened only to carry the mode over, and only now that the contents have
    // passed. This is a fourth by-path resolution of `tmp` — the exec, the
    // rename and the cleanup being the others — so it is checked rather than
    // assumed: a symlink swapped in here would otherwise take the install mode.
    let file = OpenOptions::new()
        .read(true)
        .open(tmp)
        .with_context(|| format!("reopening {} to set its mode", tmp.display()))?;
    if file_id(&file) != ran {
        bail!(
            "{} was replaced while it was being smoke-tested",
            tmp.display()
        );
    }
    set_install_mode(&file, exe, tmp)
}

/// The device and inode of an open file, so two handles can be compared for
/// being the same file rather than the same name. `None` where the platform has
/// no such pair to read, which makes the comparison vacuous rather than wrong —
/// the equality below then holds for any two handles.
#[cfg(unix)]
fn file_id(file: &fs::File) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    file.metadata().ok().map(|m| (m.dev(), m.ino()))
}

#[cfg(not(unix))]
fn file_id(_file: &fs::File) -> Option<(u64, u64)> {
    None
}

/// Write the archive's [`BIN`] member into `out`, by the format its asset name
/// says it is. Only that one member is ever extracted, and it is matched by an
/// exact name — so no path out of the archive is ever joined onto a destination,
/// and there is nothing for a crafted entry name to escape.
fn extract(archive: &[u8], target: &Target, out: &mut fs::File) -> Result<()> {
    if target.archive.ends_with(".zip") {
        return unzip(archive, out);
    }
    let gz = flate2::read::GzDecoder::new(archive);
    untar(gz, out)
}

/// [`BIN`] out of a zip archive.
fn unzip(archive: &[u8], out: &mut fs::File) -> Result<()> {
    let mut zip = zip::ZipArchive::new(io::Cursor::new(archive))
        .context("reading the release archive as a zip")?;
    let mut entry = zip
        .by_name(BIN)
        .with_context(|| format!("the release archive has no {BIN} member"))?;
    // Only a plain file can be the binary, the same rule the tar walk applies to
    // its typeflag. A zip symlink is an ordinary entry whose body is the target
    // path and whose mode carries `S_IFLNK`: extracting it would write that path
    // into the binary and fail the smoke test with a confusing error, having
    // learnt nothing about where the link pointed.
    if !entry.is_file() {
        bail!("the release archive's {BIN} member is not a regular file");
    }
    // The size is not pre-checked against the header's claim — `copy_member`
    // caps the bytes it actually writes, which is the number that matters.
    copy_member(&mut entry, out)
}

/// [`BIN`] out of a tar stream: walk the 512-byte headers and skip every member
/// that is not it. Enough of the format for the archives CI publishes — a member
/// whose name does not fit its header, or whose size is not plain octal, is
/// reported rather than guessed at.
fn untar<R: Read>(mut src: R, out: &mut fs::File) -> Result<()> {
    /// Tar rounds every member up to a whole number of these.
    const BLOCK: u64 = 512;

    // Everything walked past so far. The members we skip are read out of an
    // *inflating* stream, so without a ceiling over the whole walk an archive
    // whose headers declare enough bytes would spend the gzip ratio on the way
    // to a member that is never found — CPU, not memory, but unbounded.
    let mut walked: u64 = 0;
    loop {
        let mut hdr = [0u8; 512];
        match src.read_exact(&mut hdr) {
            Ok(()) => {}
            // A truncated tail is the end of what we can read; the member we
            // wanted was not in it, which the error below reports.
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("reading the release archive"),
        }
        // Two zero blocks mark the end of the archive; one is enough to stop at.
        if hdr.iter().all(|&b| b == 0) {
            break;
        }
        let size = tar_size(&hdr)?;
        // Only a plain file can be the binary: `0` (and the historical NUL) are
        // regular files, and anything else — a directory, a link, a pax or GNU
        // long-name header — is skipped along with its body.
        let regular = matches!(hdr.get(156), Some(b'0' | 0));
        if regular && tar_name(&hdr) == Some(BIN.as_bytes()) {
            // `take(size)` bounds this to what the header declared; `copy_member`
            // caps what is written, so a header claiming more than one update may
            // allocate is refused there rather than trusted here.
            return copy_member(&mut src.by_ref().take(size), out);
        }
        let padded = size.div_ceil(BLOCK).saturating_mul(BLOCK);
        walked = walked.saturating_add(padded);
        if walked > MAX_ASSET {
            bail!(
                "the release archive walks past {} without a {BIN} member",
                human_bytes(MAX_ASSET)
            );
        }
        io::copy(&mut src.by_ref().take(padded), &mut io::sink())
            .context("reading the release archive")?;
    }
    bail!("the release archive has no {BIN} member")
}

/// The name field of a tar header, without its padding — `None` when the header
/// carries a `prefix`, which would make the name a path into a subdirectory and
/// so not the top-level member we are looking for.
fn tar_name(hdr: &[u8; 512]) -> Option<&[u8]> {
    let prefix = hdr.get(345..500)?;
    if prefix.first() != Some(&0) {
        return None;
    }
    let name = hdr.get(0..100)?;
    Some(match name.iter().position(|&b| b == 0) {
        Some(end) => name.get(..end)?,
        None => name,
    })
}

/// The member size from a tar header: NUL/space-padded octal in bytes 124..136.
fn tar_size(hdr: &[u8; 512]) -> Result<u64> {
    let field = hdr.get(124..136).context("truncated tar header")?;
    // GNU's base-256 encoding, for sizes octal cannot hold. Nothing a task
    // release ships comes near 8 GiB, so this is a malformed archive, not a
    // format to support.
    if field.first().is_some_and(|b| b & 0x80 != 0) {
        bail!("the release archive uses a base-256 tar size field");
    }
    let text = std::str::from_utf8(field).context("a tar size field is not text")?;
    let digits = text.trim_matches(|c| c == '\0' || c == ' ');
    if digits.is_empty() {
        return Ok(0);
    }
    u64::from_str_radix(digits, 8).with_context(|| format!("{digits:?} is not a tar size"))
}

/// Copy an archive member into the file it is being extracted to, refusing one
/// that turns out to be bigger than [`MAX_ASSET`].
///
/// The cap is on the bytes actually written, not on the size the archive claims:
/// a zip's declared uncompressed size is a header field the archive controls,
/// and the reader bounds only the *compressed* stream, so a member that inflates
/// to far more than it admits to would otherwise be written in full — filling
/// the install directory's filesystem — before its CRC failed at the end.
fn copy_member<R: Read>(member: &mut R, out: &mut fs::File) -> Result<()> {
    // One byte past the ceiling, so a member sitting exactly on it still copies
    // while anything longer is caught rather than silently truncated.
    let ceiling = MAX_ASSET.saturating_add(1);
    let written =
        io::copy(&mut member.take(ceiling), out).context("extracting the release binary")?;
    if written > MAX_ASSET {
        bail!(
            "the archive's {BIN} member is larger than {}",
            human_bytes(MAX_ASSET)
        );
    }
    Ok(())
}

/// Confirm the extracted binary runs on this host and is the version we asked
/// for: the digest proves the transfer was faithful, not that the release is
/// usable here (a foreign architecture hashes fine and cannot exec). Runs before
/// the rename, so a binary that fails this never becomes the installed one.
fn smoke_test(path: &Path, version: &str) -> Result<()> {
    let out = run_version(path).map_err(|e| {
        let hint = match e.kind() {
            // `run_version` has already waited this one out.
            ErrorKind::ExecutableFileBusy => {
                " (something is still holding the download open for writing)"
            }
            // A release built for another platform hashes fine and cannot exec,
            // which is the common cause; the errno itself is in the chain below.
            _ => " (is the release built for this platform?)",
        };
        anyhow::Error::new(e).context(format!("running {} --version{hint}", path.display()))
    })?;
    // Non-UTF-8 output is not a version string: fall through to the error below
    // with it empty rather than mangling the bytes to report them.
    let reported = std::str::from_utf8(&out.stdout).unwrap_or_default();
    // A whole token, not a substring, so a binary reporting `4.10.0` cannot
    // satisfy a request for `4.1.0`. `--version` appends `+<commit>` build
    // metadata when it was built with any (`taskcore::version`), which is not
    // part of the release's identity.
    let named = reported
        .split_whitespace()
        .any(|t| t.split('+').next() == Some(version));
    if !out.status.success() || !named {
        // Both streams: a release that cannot run here — a missing shared
        // library, the wrong libc — says so on stderr and leaves stdout empty,
        // which reported only the version it did not find.
        //
        // Printable-only, the same rule the tag name gets in `resolve`: this is
        // output from a binary that just arrived over the network and is about to
        // be quoted into the user's terminal.
        bail!(
            "the downloaded task did not report version {version} ({}, stdout: {}, stderr: {})",
            out.status,
            printable(&out.stdout),
            printable(&out.stderr),
        );
    }
    Ok(())
}

/// A command's output, safe to quote into a terminal: the first line with
/// anything on it, trimmed, with the control bytes that could rewrite the message
/// around it dropped, and clipped so a binary that writes a megabyte of prose
/// cannot bury the error.
///
/// `from_utf8_lossy` rather than a strict decode, which the guidelines otherwise
/// steer away from: this string is only ever displayed, and a release that writes
/// non-UTF-8 to stderr should still get its bytes reported rather than swallowed.
/// The version comparison itself is strict.
fn printable(bytes: &[u8]) -> String {
    /// Enough to carry a linker or loader message, not enough to bury the error.
    const LIMIT: usize = 200;

    let text = String::from_utf8_lossy(bytes);
    // Trimmed before clipping, so a line padded with whitespace cannot spend the
    // whole budget on it and report nothing.
    let line: String = text
        .lines()
        .map(|l| {
            l.chars()
                .filter(|c| !c.is_control())
                .collect::<String>()
                .trim()
                .to_string()
        })
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    match line.char_indices().nth(LIMIT) {
        // Marked, so a clipped message does not read as a complete one.
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line,
    }
}

/// Ask on stderr, read the answer on stdin. Anything but an explicit yes
/// declines, and without a terminal to ask at there is no implicit yes —
/// `--yes` is how a script opts in.
fn confirm() -> Result<bool> {
    if !io::stdin().is_terminal() {
        bail!("task --update needs a terminal to confirm at; pass --yes to update unattended");
    }
    match CliPrompter.confirm("proceed?") {
        Ok(yes) => Ok(yes),
        // Ctrl-D at the question is the user saying no, not the update failing:
        // it exits 0 like any other decline, which is what
        // `docs/reference/cli.md` promises. Ctrl-C does not come through here at
        // all — it is a signal, and the process dies of it.
        Err(taskcore::executor::PromptError::Cancelled) => Ok(false),
        Err(e) => Err(anyhow!("{e}")).context("asking whether to proceed"),
    }
}

/// The API endpoint for a release: the one `tag` names, or the latest published
/// one.
fn api_url(api: &str, tag: Option<&ReleaseTag>) -> String {
    match tag {
        Some(t) => format!("{api}/repos/{REPO}/releases/tags/{t}"),
        None => format!("{api}/repos/{REPO}/releases/latest"),
    }
}

/// A throttled API response, told apart from a plain refusal: unauthenticated
/// calls get 60 an hour per IP, which a per-minute `--check` or a whole NATed
/// office runs through, and reporting that as a missing release sends the user
/// hunting the wrong problem. GitHub answers 429, or 403 with the remaining
/// quota at zero.
fn rate_limited(resp: &reqwest::Response) -> bool {
    let headers = resp.headers();
    let exhausted = headers
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.trim() == "0");
    // Either signal on a 403: the hourly limit zeroes the quota header, while
    // the secondary (burst) limit leaves it alone and answers with `retry-after`.
    resp.status() == 429
        || (resp.status() == 403 && (exhausted || headers.contains_key("retry-after")))
}

/// The named asset of a release, or an error naming what the release does carry.
fn pick<'a>(assets: &'a [ApiAsset], name: &str) -> Result<&'a ApiAsset> {
    assets.iter().find(|a| a.name == name).with_context(|| {
        let names: Vec<&str> = assets.iter().map(|a| a.name.as_str()).collect();
        format!(
            "the release has no {name} asset (it has: {})",
            names.join(", ")
        )
    })
}

/// Swap the verified download in as `exe`, and make the swap durable: the
/// directory fsync is what keeps a host crash from leaving the entry pointing at
/// nothing.
#[cfg(unix)]
fn publish(tmp: &Path, exe: &Path, dir: &Path) -> Result<()> {
    fs::rename(tmp, exe)
        .with_context(|| format!("installing {} as {}", tmp.display(), exe.display()))?;
    // Best-effort: the rename itself already succeeded, and a host that cannot
    // fsync its bin directory is no reason to report a failed update.
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    Ok(())
}

/// Swap the verified download in as `exe`. Windows refuses to replace a file
/// that is being executed, but it does allow the running image to be *renamed
/// away* — so the old binary is moved aside and put back if the install cannot
/// be completed. That leftover cannot be deleted while this process runs; the
/// next update removes it.
#[cfg(windows)]
fn publish(tmp: &Path, exe: &Path, _dir: &Path) -> Result<()> {
    let old = exe.with_extension("old");
    // Whatever a previous update left, now that nothing is running from it.
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old)
        .with_context(|| format!("moving {} aside as {}", exe.display(), old.display()))?;
    if let Err(e) = fs::rename(tmp, exe) {
        // Put the running binary back before reporting: leaving the install
        // without one is worse than not updating it.
        let _ = fs::rename(&old, exe);
        return Err(anyhow::Error::new(e).context(format!(
            "installing {} as {}",
            tmp.display(),
            exe.display()
        )));
    }
    // Expected to fail while this process is running from it.
    let _ = fs::remove_file(&old);
    Ok(())
}

/// The mode to install with: whatever the binary being replaced carries, so an
/// install deliberately narrowed (a group-only 0750 `task`) keeps its
/// permissions instead of being widened by an update. [`INSTALL_MODE`] when
/// there is nothing to read.
#[cfg(unix)]
fn mode_of(exe: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(exe)
        // 0o777, not 0o7777: set-user-ID and set-group-ID are not carried onto
        // bytes that just arrived over the network, whatever the old binary was
        // marked with. Group and other *write* are dropped for the same reason —
        // a `task` someone left group-writable is not a reason for the update to
        // hand the next writer a binary the whole team runs. The owner-execute
        // bit is forced on so the result is always runnable — the smoke test
        // would otherwise fail on a mode it chose itself, blaming the release.
        .map(|m| (m.permissions().mode() & 0o777 & !0o022) | 0o100)
        .unwrap_or(INSTALL_MODE)
}

/// Set `mode` on the open file, by fd.
///
/// Not `set_mode`: the tests have a path-based helper of that name, and one
/// shadowing the other in the same file is a trap for the next reader.
#[cfg(unix)]
fn set_file_mode(file: &fs::File, mode: u32, tmp: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))
        .with_context(|| format!("setting the mode on {}", tmp.display()))
}

/// Windows has no mode to set; an `.exe` is executable by its extension.
#[cfg(not(unix))]
fn set_file_mode(_file: &fs::File, _mode: u32, _tmp: &Path) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn set_install_mode(file: &fs::File, exe: &Path, tmp: &Path) -> Result<()> {
    set_file_mode(file, mode_of(exe), tmp)
}

/// Windows has no mode to carry over; an `.exe` is executable by its extension.
#[cfg(not(unix))]
fn set_install_mode(_file: &fs::File, _exe: &Path, _tmp: &Path) -> Result<()> {
    Ok(())
}

/// Read a response body into memory, refusing one bigger than `max`: finding out
/// that a body is not what it claims to be must not cost the host its RAM.
async fn bounded_body(mut resp: reqwest::Response, max: usize, url: &str) -> Result<Vec<u8>> {
    let mut body: Vec<u8> = Vec::new();
    loop {
        let chunk = resp
            .chunk()
            .await
            .with_context(|| format!("reading {url}"))?;
        let Some(chunk) = chunk else { break };
        body.extend_from_slice(&chunk);
        if body.len() > max {
            bail!("{url} is larger than {max} bytes");
        }
    }
    Ok(body)
}

/// Run `<path> --version`, waiting out a busy file rather than reporting it.
/// Closing the download's write fd is not enough on its own: a `fork` anywhere
/// else in the process inherits that open file, and the kernel counts the file
/// open for writing — so `execve` answers ETXTBSY — until that child reaches its
/// own `exec`. Nothing here can stop the fork, and the window it leaves is
/// microseconds wide, so looking again beats failing a download that is fine. A
/// file held open for real still ends in ETXTBSY, once the looking is done.
fn run_version(path: &Path) -> io::Result<std::process::Output> {
    // Ten looks 20ms apart — 180ms of waiting before a busy file is reported as busy.
    const ATTEMPTS: u32 = 10;
    const WAIT: Duration = Duration::from_millis(20);
    let run = || std::process::Command::new(path).arg("--version").output();
    for _ in 1..ATTEMPTS {
        match run() {
            Err(e) if e.kind() == ErrorKind::ExecutableFileBusy => std::thread::sleep(WAIT),
            r => return r,
        }
    }
    // The last look is the verdict, whichever way it goes.
    run()
}

/// A one-line download indicator on stderr, or nothing when stderr is not a
/// terminal (so a log or a CI job does not collect thousands of redraws).
struct Progress {
    name: String,
    total: u64,
    /// when the line was last redrawn, to keep a fast link from flooding stderr
    drawn: Instant,
    on: bool,
}

impl Progress {
    fn new(name: &str, total: u64) -> Self {
        Self {
            name: name.to_string(),
            total,
            drawn: Instant::now(),
            on: io::stderr().is_terminal(),
        }
    }

    /// Redraw with `done` bytes in, at most ten times a second.
    fn advance(&mut self, done: u64) {
        if !self.on || self.drawn.elapsed() < Duration::from_millis(100) {
            return;
        }
        self.drawn = Instant::now();
        // A progress line that cannot be written is no reason to fail an update.
        let _ = write!(
            io::stderr(),
            "\r  downloading {} {}/{}",
            self.name,
            human_bytes(done),
            human_bytes(self.total)
        );
        let _ = io::stderr().flush();
    }

    /// Erase the line, so what is printed next starts on a clean one.
    fn clear(&self) {
        if self.on {
            // As above: the progress line is not worth an error.
            let _ = write!(io::stderr(), "\r\x1b[K");
            let _ = io::stderr().flush();
        }
    }
}

/// A byte count as MiB with one decimal — a release archive is a few megabytes,
/// so one unit keeps the numbers comparable between releases and between the
/// progress line and the summary.
fn human_bytes(n: u64) -> String {
    let tenths = n.saturating_mul(10) / (1024 * 1024);
    format!("{}.{} MiB", tenths / 10, tenths % 10)
}

/// The version a release tag carries (`v4.2.0` -> `4.2.0`), for comparison
/// against this build's own version.
fn version_of(tag: &str) -> &str {
    tag.strip_prefix('v').unwrap_or(tag)
}

/// What installing `tag` would do to a binary built as `current`.
fn step(current: &str, tag: &str) -> Step {
    let candidate = version_of(tag);
    match compare(candidate, current) {
        Some(Ordering::Equal) => Step::Same,
        Some(Ordering::Greater) => Step::Newer,
        Some(Ordering::Less) => Step::Other,
        // Nothing to order against: the same string is still the same release,
        // and anything else is not an update waiting.
        None if candidate == current => Step::Same,
        None => Step::Other,
    }
}

/// Order two `MAJOR.MINOR.PATCH` versions field by field. `None` when either
/// side is not all numeric fields — a prerelease suffix or a name like `nightly`
/// has no ordering against a release version, and inventing one is how a
/// downgrade ends up announced as an update.
fn compare(a: &str, b: &str) -> Option<Ordering> {
    let fields =
        |v: &str| -> Option<Vec<u64>> { v.split('.').map(|f| f.parse::<u64>().ok()).collect() };
    Some(fields(a)?.cmp(&fields(b)?))
}

/// The published tag for a user-given version: releases are tagged `v<version>`,
/// so accept `4.2.0` as well as `v4.2.0`; a tag of another shape passes through.
///
/// Restricted to tag-shaped tokens because this lands in the API URL's path,
/// where the URL parser resolves `..` segments: a `/` in it would silently
/// retarget the query at another repository's releases, and both of this
/// module's gates would then pass — that release's sidecar matches its own
/// archive, and its binary reports its own version. So the tag is the trust
/// boundary, and it is checked here.
fn release_tag(arg: &str) -> Result<ReleaseTag> {
    let shaped = !arg.is_empty()
        && !arg.starts_with('.')
        && arg
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+'));
    if !shaped {
        bail!("{arg:?} is not a version or a tag name");
    }
    Ok(ReleaseTag(
        if arg.starts_with(|c: char| c.is_ascii_digit()) {
            format!("v{arg}")
        } else {
            arg.to_string()
        },
    ))
}

/// The digest of `name` in `sha256sum` output (`<hex>  <name>` lines), as the raw
/// bytes to compare a download's own hash against.
fn parse_digest(text: &str, name: &str) -> Result<[u8; 32]> {
    for line in text.lines() {
        let Some((sum, file)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        // sha256sum marks a binary-mode read with a single `*` before the name.
        let file = file.trim();
        if file.strip_prefix('*').unwrap_or(file) != name {
            continue;
        }
        return parse_sha256(sum).with_context(|| format!("{sum:?} is not a sha256"));
    }
    bail!("no {name} line in the sidecar")
}

/// 64 hex digits as the 32 bytes they encode. The length and alphabet are
/// checked first, so the pairing below cannot run short.
fn parse_sha256(s: &str) -> Option<[u8; 32]> {
    let hex = s.as_bytes();
    if hex.len() != 64 || !hex.iter().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (byte, pair) in out.iter_mut().zip(hex.chunks_exact(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::OnceLock;

    use super::*;

    /// The version the fake release server publishes, and the one its stand-in
    /// binary reports. A major past anything this repo will ship, so releasing
    /// the version the tests once used cannot turn `resolve`'s `Step::Newer`
    /// into a `Step::Same`.
    const FAKE_TAG: &str = "v99.0.0";
    const FAKE_VERSION: &str = "99.0.0";

    // A user-given version reaches the API as the tag CI publishes (`v<version>`),
    // whichever of the two forms they typed; other tag shapes pass through.
    #[test]
    fn release_tag_normalizes_a_bare_version() {
        assert_eq!(release_tag("4.2.0").unwrap().to_string(), "v4.2.0");
        assert_eq!(release_tag("v4.2.0").unwrap().to_string(), "v4.2.0");
        assert_eq!(release_tag("nightly").unwrap().to_string(), "nightly");
        assert_eq!(version_of("v4.2.0"), "4.2.0");
        assert_eq!(version_of("4.2.0"), "4.2.0");
    }

    // The tag lands in the API URL's path, so a `/` in it must never reach the
    // URL: the parser resolves `..` segments, and a retargeted query would be
    // answered by a release whose own digest and own version self-check both pass.
    #[test]
    fn release_tag_refuses_anything_that_could_retarget_the_url() {
        for bad in [
            "v4.2.0/../../../../../evil-owner/evil-repo/releases/latest",
            "../../evil-owner/evil-repo/releases/latest",
            "..",
            ".",
            "v1%2f..%2fx",
            "v1?per_page=1",
            "v1#frag",
            "v1 2",
            "",
        ] {
            assert!(release_tag(bad).is_err(), "{bad:?} must be refused");
        }
        // and the shapes that are allowed still build the endpoint they should.
        // Only a `ReleaseTag` can be passed here, so this is the whole surface
        // reaching the URL.
        assert_eq!(
            api_url(API, Some(&release_tag("4.2.0").unwrap())),
            "https://api.github.com/repos/wallix/task-rs/releases/tags/v4.2.0"
        );
        assert_eq!(
            api_url(API, None),
            "https://api.github.com/repos/wallix/task-rs/releases/latest"
        );
    }

    // Only a strictly newer release is an update. A lower tag, and a tag with no
    // version to order at all, stay installable but are never reported as one
    // waiting — otherwise a build made after the last release nags about
    // downgrading forever.
    #[test]
    fn only_a_newer_release_counts_as_an_update() {
        assert_eq!(step("4.1.0", "v4.2.0"), Step::Newer);
        assert_eq!(step("4.1.0", "v4.1.1"), Step::Newer);
        assert_eq!(step("4.1.0", "v5.0.0"), Step::Newer);
        assert_eq!(step("4.1.0", "v4.1.0"), Step::Same);
        assert_eq!(step("4.1.0", "4.1.0"), Step::Same);
        // ordered, not string-compared, so a differently written same version agrees
        assert_eq!(step("4.1.0", "v4.1.00"), Step::Same);
        // numeric fields, not string order: 4.9.0 -> 4.10.0 is forward
        assert_eq!(step("4.9.0", "v4.10.0"), Step::Newer);
        assert_eq!(step("4.10.0", "v4.9.0"), Step::Other);
        // a version-bumped build looking at the last published tag: not an update
        assert_eq!(step("4.2.0", "v4.1.0"), Step::Other);
        // nothing to order against
        assert_eq!(step("4.1.0", "nightly"), Step::Other);
        assert_eq!(step("nightly", "nightly"), Step::Same);
        assert_eq!(step("4.1.0", "v4.2.0-rc1"), Step::Other);
    }

    // The sidecar is `sha256sum` output: pick the line for our archive, and
    // refuse anything that is not a sha256 rather than comparing against junk.
    #[test]
    fn digest_comes_from_the_archive_line() {
        let sum = "45c51f7d53eb22416c49c79a5dcccf94b9e0e110ba88b3ee7bbe22f98d0cd31d";
        let want = parse_sha256(sum).unwrap();
        let name = "task-linux-x86_64.tar.gz";
        assert_eq!(
            parse_digest(&format!("{sum}  {name}\n"), name).unwrap(),
            want
        );
        // binary-mode marker, and a multi-file sidecar: the right line still wins
        let many = format!("0{}  task-macos-x86_64.tar.gz\n{sum} *{name}\n", &sum[1..]);
        assert_eq!(parse_digest(&many, name).unwrap(), want);
        // another platform's archive must not satisfy a request for ours, and
        // neither must a name that only differs from it by more of the marker we
        // strip
        assert!(parse_digest(&format!("{sum}  task-macos-x86_64.tar.gz\n"), name).is_err());
        assert!(parse_digest(&format!("{sum} **{name}\n"), name).is_err());
        assert!(parse_digest(&format!("not-a-hash  {name}\n"), name).is_err());
        assert!(parse_digest("", name).is_err());
        // 64 hex digits and nothing else, decoded with leading zero bytes kept
        assert!(parse_sha256(&sum[..63]).is_none());
        assert!(parse_sha256(&format!("{sum}0")).is_none());
        assert_eq!(parse_sha256(&"00".repeat(32)).unwrap(), [0u8; 32]);
        assert_eq!(parse_sha256(&"0f".repeat(32)).unwrap(), [0x0f; 32]);
    }

    // Both assets must be present; the error names what the release does carry.
    #[test]
    fn asset_pick_reports_what_is_there() {
        let assets = vec![
            ApiAsset {
                name: "task-linux-x86_64.tar.gz".to_string(),
                browser_download_url: "https://example/a".to_string(),
                size: 42,
            },
            ApiAsset {
                name: "task-macos-x86_64.tar.gz".to_string(),
                browser_download_url: "https://example/b".to_string(),
                size: 7,
            },
        ];
        assert_eq!(pick(&assets, "task-linux-x86_64.tar.gz").unwrap().size, 42);
        let err = pick(&assets, "task-linux-aarch64.sha256")
            .err()
            .unwrap()
            .to_string();
        assert!(
            err.contains("no task-linux-aarch64.sha256 asset")
                && err.contains("task-macos-x86_64.tar.gz"),
            "{err}"
        );
    }

    // The names have to be the ones the release workflow publishes — a typo here
    // is a `--update` that reports every release as missing its asset.
    #[test]
    fn asset_names_follow_the_release_matrix() {
        let (archive, sidecar) = asset_names().unwrap();
        let stem = archive
            .strip_suffix(".tar.gz")
            .or_else(|| archive.strip_suffix(".zip"))
            .unwrap_or_else(|| panic!("unexpected archive name {archive}"));
        assert_eq!(sidecar, format!("{stem}.sha256"));
        assert!(
            [
                "task-linux-x86_64",
                "task-linux-aarch64",
                "task-macos-x86_64",
                "task-macos-aarch64",
                "task-windows-x86_64",
                "task-windows-aarch64",
            ]
            .contains(&stem),
            "{stem} is not a published archive"
        );
        assert_eq!(archive.ends_with(".zip"), cfg!(windows));
    }

    #[test]
    fn sizes_are_reported_in_megabytes() {
        assert_eq!(human_bytes(0), "0.0 MiB");
        assert_eq!(human_bytes(8_252_520), "7.8 MiB");
        // The scaling saturates rather than wrapping, so an absurd size reads as
        // an absurd number instead of a small plausible one.
        assert_eq!(human_bytes(u64::MAX), "1759218604441.5 MiB");
    }

    /// A stand-in for a release's binary: a script, so the smoke test can really
    /// run it.
    fn fake_binary(version: &str) -> Vec<u8> {
        format!("#!/bin/sh\necho \"{version}\"\n").into_bytes()
    }

    /// One tar member: a 512-byte header, the body, and padding out to the next
    /// block. `kind` is the type flag (`0` a regular file, `5` a directory).
    fn tar_entry(name: &str, body: &[u8], kind: u8) -> Vec<u8> {
        let mut hdr = [0u8; 512];
        hdr[..name.len()].copy_from_slice(name.as_bytes());
        hdr[100..108].copy_from_slice(b"0000755\0");
        hdr[124..136].copy_from_slice(format!("{:011o}\0", body.len()).as_bytes());
        hdr[136..148].copy_from_slice(b"00000000000\0");
        hdr[156] = kind;
        hdr[257..263].copy_from_slice(b"ustar\0");
        hdr[263..265].copy_from_slice(b"00");
        // The header checksum is computed with its own field read as spaces.
        hdr[148..156].copy_from_slice(b"        ");
        let sum: u32 = hdr.iter().map(|&b| u32::from(b)).sum();
        hdr[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());

        let mut out = hdr.to_vec();
        out.extend_from_slice(body);
        out.resize(out.len().next_multiple_of(512), 0);
        out
    }

    /// A tar stream of `entries`, closed by the two zero blocks tar ends with.
    fn tar(entries: &[Vec<u8>]) -> Vec<u8> {
        let mut out: Vec<u8> = entries.concat();
        out.extend_from_slice(&[0u8; 1024]);
        out
    }

    /// The release archive for this platform, holding a `task` that reports
    /// `version` — a `.zip` on Windows and a `.tar.gz` everywhere else, so the
    /// format the tests drive is the one this host would really be served.
    fn release_archive(version: &str) -> Vec<u8> {
        let bin = fake_binary(version);
        if cfg!(windows) {
            return zip_of(&[(BIN, &bin), ("README.md", b"docs")]);
        }
        targz(&tar(&[
            tar_entry("pax_global_header", b"52 comment=fake\n", b'x'),
            tar_entry(BIN, &bin, b'0'),
            tar_entry("completion/", b"", b'5'),
            tar_entry("README.md", b"docs", b'0'),
        ]))
    }

    fn targz(tar: &[u8]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(tar).unwrap();
        gz.finish().unwrap()
    }

    fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        for (name, body) in entries {
            w.start_file::<_, ()>(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(body).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    /// Extract into a scratch file and hand back what landed in it.
    fn extracted(archive: &[u8], name: &str) -> Result<Vec<u8>> {
        let s = Scratch::new(name);
        let path = s.dir.join("out");
        let mut file = fs::File::create(&path).unwrap();
        let target = Target {
            tag: FAKE_TAG.to_string(),
            archive: if archive.starts_with(b"PK") {
                "task-x.zip".to_string()
            } else {
                "task-x.tar.gz".to_string()
            },
            url: String::new(),
            digest_url: String::new(),
            size: archive.len() as u64,
        };
        extract(archive, &target, &mut file)?;
        drop(file);
        Ok(fs::read(&path).unwrap())
    }

    // The binary is picked out of the archive by name, past the pax header, the
    // directory and the other files a release archive ships.
    #[test]
    fn extraction_finds_the_binary_among_the_other_members() {
        let want = fake_binary(FAKE_VERSION);
        assert_eq!(
            extracted(&release_archive(FAKE_VERSION), "tar").unwrap(),
            want
        );
        // The zip path is the one Windows takes; exercised everywhere so it is
        // not first run on a platform CI never builds a test binary for.
        let zipped = zip_of(&[("README.md", b"docs"), (BIN, &want)]);
        assert_eq!(extracted(&zipped, "zip").unwrap(), want);
    }

    // An archive without it is an error, not an empty binary — and neither a
    // directory nor a path into one may stand in for the top-level member.
    #[test]
    fn extraction_refuses_an_archive_without_the_binary() {
        for (label, archive) in [
            ("empty", tar(&[])),
            (
                "decoy",
                tar(&[
                    tar_entry(BIN, b"", b'5'),
                    tar_entry("README.md", b"docs", b'0'),
                ]),
            ),
        ] {
            let err = format!(
                "{:#}",
                extracted(&targz(&archive), label).expect_err("must not extract")
            );
            assert!(err.contains(&format!("no {BIN} member")), "{label}: {err}");
        }
        let err = format!(
            "{:#}",
            extracted(&zip_of(&[("README.md", b"docs")]), "zip-none")
                .expect_err("must not extract")
        );
        assert!(err.contains(&format!("no {BIN} member")), "{err}");
    }

    /// A zip member named [`BIN`] that is a symlink rather than a file. The tar
    /// walk has always checked its typeflag (the `decoy` case above); the zip
    /// side gets the same rule, so the two formats cannot disagree about what
    /// counts as the binary.
    ///
    /// A symlink, not a directory: a directory entry's name ends in `/`, so it
    /// can never be what `by_name(BIN)` matched — a symlink is the one non-file
    /// that reaches the check.
    #[test]
    fn a_zip_member_that_is_not_a_file_is_refused() {
        let mut w = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        w.add_symlink::<_, _, ()>(BIN, "/etc/passwd", zip::write::SimpleFileOptions::default())
            .unwrap();
        let archive = w.finish().unwrap().into_inner();

        let err = format!(
            "{:#}",
            extracted(&archive, "zip-symlink").expect_err("must not extract")
        );
        assert!(err.contains("not a regular file"), "{err}");
    }

    /// A member that inflates past [`MAX_ASSET`] is refused while it is being
    /// written, not by believing the size the archive claims: a zip's declared
    /// uncompressed size is a header field, and the reader bounds only the
    /// compressed stream, so the ceiling has to be on the bytes that land.
    ///
    /// The body is zeros, which deflate and gzip both reduce to a few kilobytes —
    /// the archive under test is small, and only the extraction is large.
    #[test]
    fn an_archive_member_larger_than_the_cap_is_refused() {
        // A zip that *lies*: it declares a kilobyte and inflates to more than the
        // ceiling. This is the case a pre-check on the declared size cannot
        // catch — the whole point of capping the bytes that land instead — so
        // reverting `copy_member` to trusting the header makes this test fail
        // rather than pass on a different message.
        let archive = zip_lying_about_its_size(BIN, MAX_ASSET + 1, 1024);
        // The archive itself stays tiny; only the extraction is large.
        assert!(archive.len() < 512 * 1024, "{}", archive.len());

        let err = format!(
            "{:#}",
            extracted(&archive, "oversized-member").expect_err("must not extract")
        );
        assert!(
            err.contains(&format!("{BIN} member is larger than")),
            "{err}"
        );
    }

    /// A tar walk that would spend the gzip ratio skipping members before it
    /// found out there is no binary. The header alone is enough: the ceiling is
    /// checked before the member's body is read.
    #[test]
    fn a_tar_walk_that_never_reaches_the_binary_is_cut_short() {
        let mut hdr = [0u8; 512];
        hdr[..8].copy_from_slice(b"filler\0\0");
        hdr[100..108].copy_from_slice(b"0000644\0");
        // One byte past the ceiling, declared and never delivered.
        hdr[124..136].copy_from_slice(format!("{:011o}\0", MAX_ASSET + 1).as_bytes());
        hdr[136..148].copy_from_slice(b"00000000000\0");
        hdr[156] = b'0';
        hdr[257..263].copy_from_slice(b"ustar\0");
        hdr[263..265].copy_from_slice(b"00");
        hdr[148..156].copy_from_slice(b"        ");
        let sum: u32 = hdr.iter().map(|&b| u32::from(b)).sum();
        hdr[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());

        let err = format!(
            "{:#}",
            untar(
                io::Cursor::new(hdr.to_vec()),
                &mut fs::File::create(Scratch::new("walk").dir.join("out")).unwrap(),
            )
            .expect_err("must not walk past the ceiling")
        );
        assert!(err.contains("walks past"), "{err}");
    }

    /// A single-entry zip whose deflate stream inflates to `real` bytes of zeros
    /// while its headers claim `declared`.
    ///
    /// Hand-built rather than through `ZipWriter`, which always records the true
    /// size: a reader that believes the header is exactly what is under test. The
    /// CRC is left zero — nothing here reaches the point where it is checked.
    fn zip_lying_about_its_size(name: &str, real: u64, declared: u32) -> Vec<u8> {
        // Written in chunks so the uncompressed body is never held in memory.
        let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::fast());
        let chunk = [0u8; 64 * 1024];
        let mut left = real;
        while left > 0 {
            let n = left.min(chunk.len() as u64) as usize;
            enc.write_all(&chunk[..n]).unwrap();
            left -= n as u64;
        }
        let deflated = enc.finish().unwrap();

        let n = name.as_bytes();
        let csize = u32::try_from(deflated.len()).unwrap();
        let mut out = Vec::new();
        // Local file header: deflate, no CRC, the declared size.
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&8u16.to_le_bytes()); // deflate
        out.extend_from_slice(&0u32.to_le_bytes()); // mod time+date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc32
        out.extend_from_slice(&csize.to_le_bytes());
        out.extend_from_slice(&declared.to_le_bytes());
        out.extend_from_slice(&u16::try_from(n.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(n);
        out.extend_from_slice(&deflated);

        // Central directory, saying the same thing.
        let cd_start = out.len();
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version made by
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&8u16.to_le_bytes()); // deflate
        out.extend_from_slice(&0u32.to_le_bytes()); // mod time+date
        out.extend_from_slice(&0u32.to_le_bytes()); // crc32
        out.extend_from_slice(&csize.to_le_bytes());
        out.extend_from_slice(&declared.to_le_bytes());
        out.extend_from_slice(&u16::try_from(n.len()).unwrap().to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out.extend_from_slice(&0u16.to_le_bytes()); // disk number
        out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // local header offset
        out.extend_from_slice(n);
        let cd_len = u32::try_from(out.len() - cd_start).unwrap();

        // End of central directory.
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // this disk
        out.extend_from_slice(&0u16.to_le_bytes()); // disk with the CD
        out.extend_from_slice(&1u16.to_le_bytes()); // entries on this disk
        out.extend_from_slice(&1u16.to_le_bytes()); // entries total
        out.extend_from_slice(&cd_len.to_le_bytes());
        out.extend_from_slice(&u32::try_from(cd_start).unwrap().to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out
    }

    // A tar size is plain octal; a base-256 field (or junk) is reported rather
    // than read as some other number.
    #[test]
    fn a_tar_size_that_is_not_octal_is_reported() {
        let mut hdr = [0u8; 512];
        hdr[124] = 0x80;
        assert!(format!("{:#}", tar_size(&hdr).unwrap_err()).contains("base-256"));
        hdr[124..136].copy_from_slice(b"00000009zz9\0");
        assert!(format!("{:#}", tar_size(&hdr).unwrap_err()).contains("not a tar size"));
        hdr[124..136].copy_from_slice(b"00000000144\0");
        assert_eq!(tar_size(&hdr).unwrap(), 0o144);
    }

    /// How the fake release server should misbehave, so each gate is exercised
    /// against the real code path rather than a stub. Which one is in play is the
    /// first segment of the URL, so every case shares one server.
    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Fault {
        None,
        /// an archive body that is not what the sidecar promises
        WrongBody,
        /// more bytes than the release announced
        Oversized,
        /// an archive that matches its sidecar but holds another version
        WrongVersion,
        /// an archive that matches its sidecar but holds no `task` at all
        NoBinary,
        /// the archive request fails
        AssetError,
        /// the sidecar request fails
        SidecarError,
        /// the sidecar is far too big to be one `sha256sum` line
        HugeSidecar,
        /// the API is out of quota
        RateLimited,
        /// the release announces an archive too big to be one
        HugeAsset,
        /// the assets are published on another scheme than the API was reached
        /// over — the downgrade the scheme guard exists to refuse
        ForeignScheme,
        /// a tag name carrying the control bytes that would rewrite the prompt
        ControlTag,
    }

    impl Fault {
        fn segment(self) -> &'static str {
            match self {
                Fault::None => "ok",
                Fault::WrongBody => "wrong-body",
                Fault::Oversized => "oversized",
                Fault::WrongVersion => "wrong-version",
                Fault::NoBinary => "no-binary",
                Fault::AssetError => "asset-error",
                Fault::SidecarError => "sidecar-error",
                Fault::HugeSidecar => "huge-sidecar",
                Fault::RateLimited => "rate-limited",
                Fault::HugeAsset => "huge-asset",
                Fault::ForeignScheme => "foreign-scheme",
                Fault::ControlTag => "control-tag",
            }
        }

        fn from_segment(s: &str) -> Option<Fault> {
            Some(match s {
                "ok" => Fault::None,
                "wrong-body" => Fault::WrongBody,
                "oversized" => Fault::Oversized,
                "wrong-version" => Fault::WrongVersion,
                "no-binary" => Fault::NoBinary,
                "asset-error" => Fault::AssetError,
                "sidecar-error" => Fault::SidecarError,
                "huge-sidecar" => Fault::HugeSidecar,
                "rate-limited" => Fault::RateLimited,
                "huge-asset" => Fault::HugeAsset,
                "foreign-scheme" => Fault::ForeignScheme,
                "control-tag" => Fault::ControlTag,
                _ => return None,
            })
        }
    }

    /// The archive the server serves for a fault, and the length the release
    /// announces for it. The two agree except under `Oversized`, where the point
    /// is that they must not and the download has to stop at the announced
    /// length.
    fn body_of(fault: Fault) -> (Vec<u8>, u64) {
        let good = release_archive(FAKE_VERSION);
        match fault {
            Fault::WrongBody => (b"not the published bytes".to_vec(), good.len() as u64),
            Fault::Oversized => (good.repeat(2), good.len() as u64),
            Fault::WrongVersion => {
                let other = release_archive("4.0.0");
                (other.clone(), other.len() as u64)
            }
            Fault::NoBinary => {
                let none = if cfg!(windows) {
                    zip_of(&[("README.md", b"docs")])
                } else {
                    targz(&tar(&[tar_entry("README.md", b"docs", b'0')]))
                };
                (none.clone(), none.len() as u64)
            }
            _ => (good.clone(), good.len() as u64),
        }
    }

    /// The sidecar line for whatever body this fault serves, so every case but
    /// `WrongBody` and `Oversized` passes the digest gate and is judged by a
    /// later one.
    fn sidecar_of(fault: Fault, archive_name: &str) -> Vec<u8> {
        let (body, _) = body_of(fault);
        let body = match fault {
            // the sidecar promises the good archive; the body is not it
            Fault::WrongBody | Fault::Oversized => release_archive(FAKE_VERSION),
            _ => body,
        };
        let sum = Sha256::digest(&body);
        let hex: String = sum.iter().map(|b| format!("{b:02x}")).collect();
        let line = format!("{hex}  {archive_name}\n");
        if fault == Fault::HugeSidecar {
            return line.repeat(4096).into_bytes();
        }
        line.into_bytes()
    }

    /// One HTTP response: status, headers, body.
    struct Reply(u16, Vec<(&'static str, String)>, Vec<u8>);

    fn ok(body: Vec<u8>) -> Reply {
        Reply(200, Vec::new(), body)
    }

    /// Serve GitHub's release API and this platform's assets, for the one tag we
    /// publish, under `/<fault>/…` so a single server covers every case.
    fn serve(addr: SocketAddr, path: &str, user_agent: &str) -> Reply {
        let not_found = || Reply(404, Vec::new(), br#"{"message":"Not Found"}"#.to_vec());
        let Some((seg, rest)) = path.trim_start_matches('/').split_once('/') else {
            return not_found();
        };
        let Some(fault) = Fault::from_segment(seg) else {
            return not_found();
        };
        // Echoed back rather than recorded, so the assertion needs no state
        // shared with the tests running against this one server at the same time.
        if rest == "user-agent" {
            return ok(user_agent.as_bytes().to_vec());
        }
        let (archive, sidecar) = asset_names().unwrap();
        let (body, size) = body_of(fault);
        let latest = format!("repos/{REPO}/releases/latest");
        let tagged = format!("repos/{REPO}/releases/tags/{FAKE_TAG}");
        if rest == latest || rest == tagged {
            if fault == Fault::RateLimited {
                return Reply(
                    403,
                    vec![("x-ratelimit-remaining", "0".to_string())],
                    b"rate limit exceeded".to_vec(),
                );
            }
            // `ftp://` rather than a plain `http://`: the API here is itself
            // `http://`, so only a third scheme shows the guard rejecting a
            // mismatch rather than happening to agree with the test server.
            let scheme = match fault {
                Fault::ForeignScheme => "ftp",
                _ => "http",
            };
            let asset = |name: &str, size: usize| {
                format!(
                    r#"{{"name":"{name}","browser_download_url":"{scheme}://{addr}/{seg}/{name}","size":{size}}}"#
                )
            };
            let announced = match fault {
                // One byte past what a release may announce; the body served is
                // the ordinary one, because nothing should get as far as it.
                Fault::HugeAsset => MAX_ASSET as usize + 1,
                _ => size as usize,
            };
            // JSON-escaped, since a raw control byte would not be valid JSON —
            // it reaches `resolve` as the control character all the same.
            let tag = match fault {
                Fault::ControlTag => r"v99.0.0\u001b[2K",
                _ => FAKE_TAG,
            };
            let json = format!(
                r#"{{"tag_name":"{tag}","assets":[{},{}]}}"#,
                asset(&archive, announced),
                asset(&sidecar, sidecar_of(fault, &archive).len())
            );
            return ok(json.into_bytes());
        }
        if rest == sidecar {
            return match fault {
                Fault::SidecarError => Reply(500, Vec::new(), b"boom".to_vec()),
                _ => ok(sidecar_of(fault, &archive)),
            };
        }
        if rest == archive {
            return match fault {
                Fault::AssetError => Reply(500, Vec::new(), b"boom".to_vec()),
                _ => ok(body),
            };
        }
        not_found()
    }

    /// The API root to hand `resolve` for a given fault, starting the one fake
    /// release server on first use. A blocking listener on its own thread, as
    /// `ocicas`'s fake lock server does, so the accept loop never competes with a
    /// test's runtime threads.
    fn release_api(fault: Fault) -> String {
        static ADDR: OnceLock<SocketAddr> = OnceLock::new();
        let addr = ADDR.get_or_init(|| {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    std::thread::spawn(move || answer(addr, stream));
                }
            });
            addr
        });
        format!("http://{addr}/{}", fault.segment())
    }

    /// Read one HTTP/1.1 request (headers only — none of these carry a body) and
    /// write back what [`serve`] decides. `Connection: close`, so the client does
    /// not hold a connection this server would have to multiplex.
    fn answer(addr: SocketAddr, stream: TcpStream) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
        let mut user_agent = String::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = h.split_once(':')
                && name.eq_ignore_ascii_case("user-agent")
            {
                user_agent = value.trim().to_string();
            }
        }

        let Reply(status, headers, body) = serve(addr, &path, &user_agent);
        let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\n", body.len());
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("Connection: close\r\n\r\n");
        let mut stream = stream;
        let _ = stream.write_all(head.as_bytes());
        let _ = stream.write_all(&body);
        let _ = stream.flush();
    }

    /// The very client the entry points use, so its configuration is on the
    /// tested path.
    ///
    /// Built for an `http://` API, which is what these tests serve: that is the
    /// one thing it does not share with a real run, and
    /// [`the_client_refuses_a_plaintext_api`] pins the real setting instead.
    fn test_client() -> reqwest::Client {
        // reqwest (rustls-no-provider) needs a crypto provider before a client
        // builds; `main` installs it for real runs.
        let _ = rustls::crypto::ring::default_provider().install_default();
        http_client("http://127.0.0.1").unwrap()
    }

    /// A scratch directory holding a stand-in for the installed binary, removed
    /// however the test ends — a failed assertion must not leak it.
    ///
    /// Beside the test binary itself (so, under `target/`) rather than in `/tmp`:
    /// the smoke test execs the download from this directory, and a host that
    /// mounts `/tmp` `noexec` would fail every test here for a reason that has
    /// nothing to do with the code.
    struct Scratch {
        dir: PathBuf,
        exe: PathBuf,
    }

    impl Scratch {
        /// Named after the test and the pid, so two suites on the same host never
        /// share a path and pull each other's tree out mid-run. Removed first all
        /// the same, so a recycled pid starts clean instead of on an older run's
        /// leftovers.
        fn new(name: &str) -> Scratch {
            let here = std::env::current_exe().unwrap();
            let dir = here
                .parent()
                .unwrap()
                .join(format!("update-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let exe = dir.join(BIN);
            fs::write(&exe, b"the binary being replaced").unwrap();
            set_mode(&exe, 0o755);
            Scratch { dir, exe }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            // Restore traversal first: `a_rename_that_cannot_happen…` locks a
            // subdirectory down.
            for e in fs::read_dir(&self.dir).into_iter().flatten().flatten() {
                if e.path().is_dir() {
                    set_mode(&e.path(), 0o700);
                }
            }
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    #[cfg(unix)]
    fn set_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(mode));
    }

    #[cfg(not(unix))]
    fn set_mode(_path: &Path, _mode: u32) {}

    /// The temp downloads left behind in `dir`, which must always be none.
    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".task-update."))
            .collect()
    }

    async fn fake_target(client: &reqwest::Client, fault: Fault) -> Target {
        resolve(client, &release_api(fault), None).await.unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_finds_this_platforms_archive_and_its_sidecar() {
        let client = test_client();
        let target = fake_target(&client, Fault::None).await;
        let (archive, sidecar) = asset_names().unwrap();
        assert_eq!(target.tag, FAKE_TAG);
        assert_eq!(target.archive, archive);
        assert!(
            target.url.ends_with(&format!("/{archive}")),
            "{}",
            target.url
        );
        assert!(
            target.digest_url.ends_with(&format!("/{sidecar}")),
            "{}",
            target.digest_url
        );
        assert_eq!(target.size, release_archive(FAKE_VERSION).len() as u64);
        assert_eq!(step(current_version(), &target.tag), Step::Newer);
    }

    // GitHub answers nothing without a `User-Agent`, so the client sends one
    // naming the build it is updating.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_client_identifies_itself_to_the_api() {
        let client = test_client();
        let seen = client
            .get(format!("{}/user-agent", release_api(Fault::None)))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        assert_eq!(seen, format!("task/{}", current_version()));
    }

    /// The real client refuses cleartext outright, which is what carries the
    /// no-downgrade guarantee across the redirect every asset URL goes through —
    /// `resolve`'s scheme check only sees the URL it was handed. [`test_client`]
    /// is built for an `http://` API precisely so the rest of the suite can serve
    /// one, so this pins the setting a real run gets.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_client_refuses_a_plaintext_api() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let real = http_client("https://api.github.com").unwrap();
        let err = real
            .get(format!("{}/user-agent", release_api(Fault::None)))
            .send()
            .await
            .expect_err("http:// must be refused");
        // Refused before a connection is attempted, so the server this URL points
        // at never sees the request.
        assert!(err.is_builder(), "{err:#}");
    }

    // The whole point of the module: a release that passes both gates becomes the
    // binary, keeping the mode the one it replaced had.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_publishes_a_download_that_passes_both_gates() {
        use std::os::unix::fs::PermissionsExt;

        let client = test_client();
        let target = fake_target(&client, Fault::None).await;

        // 0750 rather than the 0755 default: an install deliberately narrowed
        // keeps its mode instead of being widened, and the umask does not get to
        // decide it either.
        let s = Scratch::new("ok");
        set_mode(&s.exe, 0o750);
        install(&client, &target, &s.exe, &s.dir).await.unwrap();
        assert_eq!(fs::read(&s.exe).unwrap(), fake_binary(FAKE_VERSION));
        assert_eq!(
            fs::metadata(&s.exe).unwrap().permissions().mode() & 0o7777,
            0o750
        );
        assert_eq!(leftovers(&s.dir), Vec::<String>::new());
    }

    // And its other half: a download that fails either gate never becomes the
    // installed binary, and nothing unverified is left next to it. One directory
    // for all of the cases, so a failure to clean up surfaces as the next one
    // refusing to create its temp file.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_download_that_fails_a_gate_never_becomes_the_binary() {
        let client = test_client();
        let s = Scratch::new("gates");
        let before = fs::read(&s.exe).unwrap();
        for (fault, want) in [
            (Fault::WrongBody, "does not match the published digest"),
            (Fault::Oversized, "longer than the"),
            (Fault::WrongVersion, "did not report version 99.0.0"),
            (Fault::NoBinary, "no task member"),
            (Fault::AssetError, "500"),
            (Fault::SidecarError, "500"),
            (Fault::HugeSidecar, "is larger than"),
        ] {
            let target = fake_target(&client, fault).await;
            let err = format!(
                "{:#}",
                install(&client, &target, &s.exe, &s.dir)
                    .await
                    .expect_err("a failed gate must not install")
            );
            assert!(err.contains(want), "expected {want:?}, got {err:?}");
            assert_eq!(
                fs::read(&s.exe).unwrap(),
                before,
                "{want}: exe was replaced"
            );
            assert_eq!(
                leftovers(&s.dir),
                Vec::<String>::new(),
                "{want}: left a temp"
            );
        }
    }

    // The binary being replaced has to still be there. `current_exe` hands back a
    // `…/task (deleted)` pathname once it is not, and renaming onto that would
    // report success while installing something nobody runs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn install_refuses_when_the_binary_it_would_replace_is_gone() {
        let client = test_client();
        let target = fake_target(&client, Fault::None).await;
        let s = Scratch::new("gone");
        fs::remove_file(&s.exe).unwrap();
        let err = format!(
            "{:#}",
            install(&client, &target, &s.exe, &s.dir)
                .await
                .expect_err("nothing to replace")
        );
        assert!(err.contains("is gone"), "{err}");
        // refused before anything was downloaded, so there is nothing to clean up
        assert_eq!(leftovers(&s.dir), Vec::<String>::new());
    }

    // A verified download whose rename cannot happen is removed too, rather than
    // left executable beside the binary. `exe` sits in a directory of its own,
    // made unwritable after it is populated, so the download lands in `dir` as
    // usual and only the rename is refused.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_rename_that_cannot_happen_leaves_no_temp_behind() {
        // Root ignores the directory mode, so there would be no failure to observe.
        if std::env::var_os("USER").is_some_and(|u| u == *"root") {
            return;
        }
        let client = test_client();
        let target = fake_target(&client, Fault::None).await;
        let s = Scratch::new("rename");
        let locked = s.dir.join("locked");
        fs::create_dir(&locked).unwrap();
        let exe = locked.join(BIN);
        fs::write(&exe, b"the binary being replaced").unwrap();
        set_mode(&exe, 0o755);
        set_mode(&locked, 0o500);

        let err = format!(
            "{:#}",
            install(&client, &target, &exe, &s.dir)
                .await
                .expect_err("the rename must fail")
        );
        assert!(err.contains("installing"), "{err}");
        assert_eq!(leftovers(&s.dir), Vec::<String>::new());
        assert_eq!(fs::read(&exe).unwrap(), b"the binary being replaced");
    }

    // A temp file already at the path is someone else's: refuse it, and — since
    // the error tells the user to remove it — leave it there to be removed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_leftover_temp_is_reported_and_not_touched() {
        let client = test_client();
        let target = fake_target(&client, Fault::None).await;
        let s = Scratch::new("leftover");
        let tmp = s.dir.join(tmp_name());
        fs::write(&tmp, b"a previous run's").unwrap();

        let err = format!(
            "{:#}",
            install(&client, &target, &s.exe, &s.dir)
                .await
                .expect_err("must not reuse it")
        );
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(fs::read(&tmp).unwrap(), b"a previous run's");
    }

    // The mode is copied from the binary being replaced, minus the bits that must
    // not follow bytes off the network.
    #[cfg(unix)]
    #[test]
    fn the_install_mode_comes_from_the_binary_being_replaced() {
        let s = Scratch::new("mode");
        set_mode(&s.exe, 0o4751);
        assert_eq!(
            mode_of(&s.exe),
            0o751,
            "set-user-ID must not be carried over"
        );
        // whatever it copies, the result has to be something the smoke test can run
        set_mode(&s.exe, 0o644);
        assert_eq!(mode_of(&s.exe), 0o744);
        // A `task` left group- or world-writable is not a reason to install one:
        // carrying those bits over would hand the next writer a binary everyone
        // else runs.
        set_mode(&s.exe, 0o777);
        assert_eq!(
            mode_of(&s.exe),
            0o755,
            "group and other write must not be carried over"
        );
        assert_eq!(mode_of(&s.dir.join("absent")), INSTALL_MODE);
    }

    // The smoke test execs a file this process just wrote, so it races every
    // `fork` the process makes: one landing while the download is still open
    // inherits the write fd and holds the file busy past the close, until that
    // child execs. A write fd of our own stands in for that inherited one —
    // released while the smoke test is already looking again, and the download is
    // good, so the verdict must be that it passes.
    #[cfg(unix)]
    #[test]
    fn the_smoke_test_waits_out_a_binary_something_still_holds_open() {
        let s = Scratch::new("busy");
        fs::write(&s.exe, fake_binary(FAKE_VERSION)).unwrap();
        set_mode(&s.exe, 0o755);

        let held = OpenOptions::new().write(true).open(&s.exe).unwrap();
        // Released a long way inside the 180ms budget, and late enough that the
        // first look is the one that finds the file busy.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(held);
        });
        let looking = Instant::now();
        smoke_test(&s.exe, FAKE_VERSION).unwrap();
        assert!(
            looking.elapsed() >= Duration::from_millis(20),
            "the first look succeeded: nothing waited"
        );

        // Held for good: the wait is a budget, not a spin, so a file that never
        // frees up still reports the errno it failed on.
        let _held = OpenOptions::new().write(true).open(&s.exe).unwrap();
        let err = smoke_test(&s.exe, FAKE_VERSION).unwrap_err();
        assert_eq!(
            err.downcast_ref::<io::Error>().map(io::Error::kind),
            Some(ErrorKind::ExecutableFileBusy),
            "{err:#}"
        );
    }

    // Build metadata is not part of a release's identity: a binary built from the
    // tag with a commit hash baked in still reports the version asked for.
    #[cfg(unix)]
    #[test]
    fn the_smoke_test_accepts_a_build_with_commit_metadata() {
        let s = Scratch::new("metadata");
        fs::write(&s.exe, fake_binary("99.0.0+abc1234.dirty")).unwrap();
        set_mode(&s.exe, 0o755);
        smoke_test(&s.exe, FAKE_VERSION).unwrap();
        let err = format!("{:#}", smoke_test(&s.exe, "99.0").unwrap_err());
        assert!(err.contains("did not report version 99.0 "), "{err}");
    }

    // A tag that was never released and an exhausted API quota are different
    // problems: reporting the second as the first sends the user hunting a typo.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_separates_a_missing_release_from_a_rate_limit() {
        let client = test_client();

        let err = format!(
            "{:#}",
            resolve(&client, &release_api(Fault::None), Some("4.0.0"))
                .await
                .expect_err("no such tag")
        );
        assert!(err.contains("no release v4.0.0"), "{err}");

        let err = format!(
            "{:#}",
            resolve(&client, &release_api(Fault::RateLimited), None)
                .await
                .expect_err("out of quota")
        );
        assert!(err.contains("rate limit is exhausted"), "{err}");
    }

    /// The three things `resolve` refuses about a release before a single byte of
    /// archive is fetched. Each is cheap to state and none of them is reachable
    /// once the download has started, so they are gates or they are nothing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resolve_refuses_a_release_it_cannot_trust() {
        let client = test_client();
        for (fault, want) in [
            // An announced size past the ceiling: the number is what the download
            // budgets against, so it is checked rather than trusted.
            (Fault::HugeAsset, "larger than a task release can be"),
            // Assets published on another scheme than the API was reached over —
            // the sidecar would move with the archive, leaving the digest gate
            // agreeing with whatever arrived.
            (Fault::ForeignScheme, "not served over http://"),
            // A tag name is printed into the confirmation prompt, so it may not
            // carry the control bytes that would rewrite the line around it.
            (Fault::ControlTag, "not printable"),
        ] {
            let e = format!(
                "{:#}",
                resolve(&client, &release_api(fault), None)
                    .await
                    .expect_err("refused")
            );
            assert!(e.contains(want), "{fault:?}: {e}");
        }
    }
}
