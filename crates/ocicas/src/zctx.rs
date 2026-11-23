//! zstd contexts kept between operations.
//!
//! For a chunk, zstd's tables, window and stream buffers take over 1 MiB to decode
//! or to compress. A fresh context per frame repeats workspace allocation and
//! compressor table setup. Touching pages the allocator released also incurs page
//! faults. Each reused encoder writes the same frame bytes as a fresh one.
//!
//! Vendored from virtkit's `vk-registry/src/zctx.rs`, with a whole-buffer
//! compression in place of its streaming encoder: this store's frames are those of
//! `zstd::bulk::compress`.

use std::io::{self, BufRead};
use std::sync::{Mutex, MutexGuard, PoisonError};

use zstd::stream::raw::{CParameter, InBuffer, Operation, OutBuffer, WriteBuf};
use zstd::stream::zio;
use zstd::zstd_safe::{self, CCtx, DCtx, ResetDirective};

use crate::index::ZSTD_LEVEL;

/// Idle contexts kept per pool, 128 MiB at most. A full pool frees any additional
/// context returned to it.
const MAX_IDLE: usize = 16;

/// Per-context retention limit in bytes, above the workspace for a chunk of the
/// maximum size. A sender can choose a large decode window for a pulled frame.
const MAX_KEPT: usize = 8 << 20;

pub(crate) trait Context: Sized {
    fn create() -> Option<Self>;
    /// Reset the session and its parameters, even after an abandoned frame or an error.
    /// Return `false` if the reset fails.
    fn renew(&mut self) -> bool;
    /// Bytes allocated to the context.
    fn size(&self) -> usize;
}

impl Context for DCtx<'static> {
    fn create() -> Option<Self> {
        DCtx::try_create()
    }
    fn renew(&mut self) -> bool {
        self.reset(ResetDirective::SessionAndParameters).is_ok()
    }
    fn size(&self) -> usize {
        self.sizeof()
    }
}

impl Context for CCtx<'static> {
    fn create() -> Option<Self> {
        CCtx::try_create()
    }
    fn renew(&mut self) -> bool {
        self.reset(ResetDirective::SessionAndParameters).is_ok()
    }
    fn size(&self) -> usize {
        self.sizeof()
    }
}

/// Idle contexts of one kind.
pub(crate) struct Pool<T>(Mutex<Vec<T>>);

static DECODERS: Pool<DCtx<'static>> = Pool::new();
static ENCODERS: Pool<CCtx<'static>> = Pool::new();

impl<T: Context> Pool<T> {
    const fn new() -> Self {
        Pool(Mutex::new(Vec::new()))
    }

    /// The idle list remains valid after a panic because each push or pop is complete
    /// or has made no change.
    fn idle(&self) -> MutexGuard<'_, Vec<T>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Renew an idle context, or create one if the pool is empty or renewal fails.
    fn take(&'static self) -> io::Result<Pooled<T>> {
        // Pop in a separate statement so the lock is released before the reset.
        let kept = self.idle().pop();
        let renewed = kept.and_then(|mut ctx| ctx.renew().then_some(ctx));
        let ctx = match renewed {
            Some(ctx) => ctx,
            None => T::create().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::OutOfMemory,
                    "zstd could not allocate a context",
                )
            })?,
        };
        Ok(Pooled {
            ctx: Some(ctx),
            pool: self,
        })
    }
}

/// A checked-out context. On drop, its [`Pool`] keeps it if both limits allow.
pub(crate) struct Pooled<T: Context + 'static> {
    /// `None` only after `drop` takes the context.
    ctx: Option<T>,
    pool: &'static Pool<T>,
}

impl<T: Context> Pooled<T> {
    fn ctx(&mut self) -> io::Result<&mut T> {
        self.ctx
            .as_mut()
            .ok_or_else(|| io::Error::other("missing zstd context"))
    }
}

impl<T: Context> Drop for Pooled<T> {
    fn drop(&mut self) {
        let Some(ctx) = self.ctx.take() else {
            return;
        };
        if ctx.size() > MAX_KEPT {
            return;
        }
        let mut idle = self.pool.idle();
        if idle.len() < MAX_IDLE {
            idle.push(ctx);
        }
    }
}

fn zstd_error(code: usize) -> io::Error {
    io::Error::other(zstd_safe::get_error_name(code))
}

/// Pooled equivalent of `zstd::stream::raw::Decoder` for the read paths, which never
/// call `flush`.
impl Operation for Pooled<DCtx<'static>> {
    fn run<C: WriteBuf + ?Sized>(
        &mut self,
        input: &mut InBuffer<'_>,
        output: &mut OutBuffer<'_, C>,
    ) -> io::Result<usize> {
        self.ctx()?
            .decompress_stream(output, input)
            .map_err(zstd_error)
    }

    fn reinit(&mut self) -> io::Result<()> {
        self.ctx()?
            .reset(ResetDirective::SessionOnly)
            .map_err(zstd_error)?;
        Ok(())
    }

    fn finish<C: WriteBuf + ?Sized>(
        &mut self,
        _output: &mut OutBuffer<'_, C>,
        finished_frame: bool,
    ) -> io::Result<usize> {
        if finished_frame {
            Ok(0)
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "incomplete frame",
            ))
        }
    }
}

/// Reads consecutive zstd frames from `R` as one decompressed stream.
pub(crate) type Decoder<R> = zio::Reader<R, Pooled<DCtx<'static>>>;

/// Decode frames from an already buffered input, such as a byte slice.
pub(crate) fn decoder_buffered<R: BufRead>(reader: R) -> io::Result<Decoder<R>> {
    decoder_from(&DECODERS, reader)
}

fn decoder_from<R: BufRead>(
    pool: &'static Pool<DCtx<'static>>,
    reader: R,
) -> io::Result<Decoder<R>> {
    Ok(zio::Reader::new(reader, pool.take()?))
}

/// Compress `raw` as one frame at [`ZSTD_LEVEL`] with its content size in the
/// header: the bytes of `zstd::bulk::compress`.
pub(crate) fn compress(raw: &[u8]) -> io::Result<Vec<u8>> {
    let mut frame = Vec::new();
    compress_into(raw, &mut frame)?;
    Ok(frame)
}

/// [`compress`] into a buffer the caller keeps from one frame to the next. The
/// frame overwrites `frame`.
pub(crate) fn compress_into(raw: &[u8], frame: &mut Vec<u8>) -> io::Result<()> {
    compress_from(&ENCODERS, raw, frame)
}

fn compress_from(
    pool: &'static Pool<CCtx<'static>>,
    raw: &[u8],
    frame: &mut Vec<u8>,
) -> io::Result<()> {
    let mut pooled = pool.take()?;
    let ctx = pooled.ctx()?;
    ctx.set_parameter(CParameter::CompressionLevel(ZSTD_LEVEL))
        .map_err(zstd_error)?;
    frame.clear();
    frame
        .try_reserve(zstd_safe::compress_bound(raw.len()))
        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
    ctx.compress2(frame, raw).map_err(zstd_error)?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::chunker::MAX_CHUNK_SIZE;
    use std::io::{BufReader, Read, Write};

    /// Partly compressible test data with a repeatable mix of random and patterned bytes.
    pub(crate) fn sample(len: usize, seed: u64) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|i| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                if i % 7 < 4 {
                    (state >> 56) as u8
                } else {
                    (i % 13) as u8
                }
            })
            .collect()
    }

    /// Encode `raw` with a fresh zstd context and the store's frame settings.
    fn reference_frame(raw: &[u8]) -> Vec<u8> {
        zstd::bulk::compress(raw, ZSTD_LEVEL).unwrap()
    }

    fn encode(pool: &'static Pool<CCtx<'static>>, raw: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        compress_from(pool, raw, &mut frame).unwrap();
        frame
    }

    fn decode(pool: &'static Pool<DCtx<'static>>, frames: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        decoder_from(pool, frames)?.read_to_end(&mut out)?;
        Ok(out)
    }

    /// A reused encoder produces the same frame bytes as a fresh encoder, whether the
    /// preceding input was larger or smaller, and records the content size. So does a
    /// reused frame buffer.
    #[test]
    fn a_reused_encoder_writes_the_frames_a_new_one_does() {
        static POOL: Pool<CCtx<'static>> = Pool::new();
        let mut frame = Vec::new();
        for (len, seed) in [
            (0, 1),
            (1, 2),
            (100_000, 3),
            (MAX_CHUNK_SIZE, 4),
            (300, 5),
            (MAX_CHUNK_SIZE, 6),
            (70_000, 7),
            (200_000, 8),
        ] {
            let raw = sample(len, seed);
            compress_from(&POOL, &raw, &mut frame).unwrap();
            assert_eq!(frame, reference_frame(&raw), "{len} bytes");
            assert_eq!(
                zstd_safe::get_frame_content_size(&frame).ok().flatten(),
                Some(len as u64),
                "{len} bytes"
            );
            assert_eq!(POOL.idle().len(), 1, "the context went back to the pool");
        }
    }

    /// Threads sharing a pool each get a context of their own and write the frames a
    /// fresh encoder does.
    #[test]
    fn concurrent_encoders_do_not_share_a_context() {
        static POOL: Pool<CCtx<'static>> = Pool::new();
        std::thread::scope(|scope| {
            for seed in 50..58 {
                scope.spawn(move || {
                    for round in 0..20 {
                        let raw = sample(50_000 + 7_000 * round, seed + round as u64);
                        assert_eq!(encode(&POOL, &raw), reference_frame(&raw));
                    }
                });
            }
        });
        let idle = POOL.idle().len();
        assert!((1..=8).contains(&idle), "{idle} idle contexts");
    }

    /// A reused decoder reads back-to-back frames and still decodes a valid frame
    /// after truncated or corrupt input.
    #[test]
    fn a_reused_decoder_reads_what_a_new_one_does() {
        static POOL: Pool<DCtx<'static>> = Pool::new();
        let (a, b) = (sample(300_000, 9), sample(MAX_CHUNK_SIZE, 10));
        let (fa, fb) = (reference_frame(&a), reference_frame(&b));

        assert_eq!(decode(&POOL, &fa).unwrap(), a);
        assert_eq!(decode(&POOL, &fb).unwrap(), b);
        assert_eq!(
            decode(&POOL, &[fa.clone(), fb.clone()].concat()).unwrap(),
            [a.clone(), b.clone()].concat()
        );
        assert_eq!(
            decode(&POOL, &[]).ok(),
            zstd::decode_all(&[][..]).ok(),
            "no input at all"
        );

        let cut = decode(&POOL, &fb[..fb.len() / 2]).unwrap_err();
        assert_eq!(cut.kind(), io::ErrorKind::UnexpectedEof, "{cut}");
        let mut head = [0u8; 1000];
        decoder_from(&POOL, &fb[..])
            .unwrap()
            .read_exact(&mut head)
            .unwrap();
        assert_eq!(head[..], b[..1000]);
        assert_eq!(decode(&POOL, &fa).unwrap(), a);

        // Corrupt compressed data can decode successfully to different bytes.
        assert!(decode(&POOL, b"not a zstd frame").is_err());
        let mut corrupt = fa.clone();
        let mid = corrupt.len() / 2;
        corrupt[mid..mid + 64].fill(0xff);
        assert_ne!(decode(&POOL, &corrupt).ok(), Some(a.clone()));
        assert_eq!(decode(&POOL, &fa).unwrap(), a);

        assert_eq!(POOL.idle().len(), 1, "one context served them all");
        let mut theirs = Vec::new();
        zstd::stream::read::Decoder::new(&fb[..])
            .unwrap()
            .read_to_end(&mut theirs)
            .unwrap();
        let mut ours = Vec::new();
        decoder_from(&POOL, BufReader::with_capacity(DCtx::in_size(), &fb[..]))
            .unwrap()
            .read_to_end(&mut ours)
            .unwrap();
        assert_eq!(ours, theirs);
    }

    /// The pool keeps at most [`MAX_IDLE`] contexts, however many were out at once, and
    /// none that grew past [`MAX_KEPT`].
    #[test]
    fn the_pool_is_bounded() {
        static POOL: Pool<DCtx<'static>> = Pool::new();
        let out: Vec<_> = (0..MAX_IDLE + 5).map(|_| POOL.take().unwrap()).collect();
        assert_eq!(POOL.idle().len(), 0);
        drop(out);
        assert_eq!(POOL.idle().len(), MAX_IDLE);

        // A 64 MiB window pushes the decoder over the retention limit.
        static BIG: Pool<DCtx<'static>> = Pool::new();
        let raw = sample(2 << 20, 11);
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        enc.window_log(26).unwrap();
        enc.write_all(&raw).unwrap();
        let frame = enc.finish().unwrap();
        assert_eq!(decode(&BIG, &frame).unwrap(), raw);
        assert_eq!(BIG.idle().len(), 0, "an outsized context is not kept");
        // A chunk of the maximum size in the store's frame format fits within the limit.
        let raw = sample(MAX_CHUNK_SIZE, 12);
        assert_eq!(decode(&BIG, &reference_frame(&raw)).unwrap(), raw);
        assert_eq!(BIG.idle().len(), 1);
    }
}
