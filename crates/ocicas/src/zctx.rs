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

use zstd::stream::raw::{CParameter, DParameter, InBuffer, Operation, OutBuffer, WriteBuf};
use zstd::stream::zio;
use zstd::zstd_safe::{self, CCtx, DCtx, ResetDirective};

use crate::index::ZSTD_LEVEL;

/// Idle contexts kept per pool, 128 MiB at most. A full pool frees any additional
/// context returned to it.
const MAX_IDLE: usize = 16;

/// Per-context retention limit in bytes, above the workspace for a chunk of the
/// maximum size. A sender can choose a large decode window for a pulled frame,
/// up to the limit its decoder was taken with.
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
        #[cfg(test)]
        if tests::OUT_OF_MEMORY.get() {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "zstd could not allocate a context",
            ));
        }
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

/// The error of a zstd call that returned `code`. A failed allocation is
/// [`io::ErrorKind::OutOfMemory`], like a context that could not be created:
/// it says nothing against the data.
fn zstd_error(code: usize) -> io::Error {
    // An error code is the negated `ZSTD_ErrorCode`.
    let memory = zstd_safe::zstd_sys::ZSTD_ErrorCode::ZSTD_error_memory_allocation as usize;
    let kind = if code == 0usize.wrapping_sub(memory) {
        io::ErrorKind::OutOfMemory
    } else {
        io::ErrorKind::Other
    };
    io::Error::new(kind, zstd_safe::get_error_name(code))
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

/// Decode frames from an already buffered input, such as a byte slice. A frame
/// may ask for any window zstd's default limit admits (128 MiB).
pub(crate) fn decoder_buffered<R: BufRead>(reader: R) -> io::Result<Decoder<R>> {
    decoder_from(&DECODERS, reader, None)
}

/// [`decoder_buffered`] refusing a frame whose window is over
/// `1 << window_log_max` bytes, before allocating it: a sender's frame header
/// sets the window, whatever the frame holds.
///
/// Every frame's header is checked before any is decoded. The decoder's own
/// limit is not enough: handed a whole frame of known content size and room
/// for it, zstd decodes in one pass without looking at the window.
pub(crate) fn decoder_windowed(frames: &[u8], window_log_max: u32) -> io::Result<Decoder<&[u8]>> {
    check_windows(frames, window_log_max)?;
    decoder_from(&DECODERS, frames, Some(window_log_max))
}

/// Reject any frame declaring a window over `1 << window_log_max` bytes.
/// If a header or frame boundary is unreadable, let the decoder report it.
fn check_windows(mut frames: &[u8], window_log_max: u32) -> io::Result<()> {
    let max = 1u64.checked_shl(window_log_max).unwrap_or(u64::MAX);
    while !frames.is_empty() {
        let Some(window) = frame_window(frames) else {
            break;
        };
        if window > max {
            return Err(io::Error::other(format!(
                "frame asks for a {window} byte window, over the {max} allowed"
            )));
        }
        let rest = zstd_safe::find_frame_compressed_size(frames)
            .ok()
            .filter(|len| *len > 0)
            .and_then(|len| frames.get(len..));
        let Some(rest) = rest else {
            break;
        };
        frames = rest;
    }
    Ok(())
}

/// The window the header of the zstd frame `frame` starts with declares
/// (RFC 8878, 3.1.1.1): 0 for a skippable frame, which has none, and `None`
/// if this is no frame header.
fn frame_window(frame: &[u8]) -> Option<u64> {
    let magic = u32::from_le_bytes(frame.get(..4)?.try_into().ok()?);
    if magic & 0xFFFF_FFF0 == 0x184D_2A50 {
        return Some(0);
    }
    if magic != 0xFD2F_B528 {
        return None;
    }
    let descriptor = *frame.get(4)?;
    // Without the single-segment flag, a window descriptor follows.
    if descriptor & 0x20 == 0 {
        let window = *frame.get(5)?;
        let log = u32::from(window >> 3).saturating_add(10);
        let base = 1u64.checked_shl(log)?;
        let mantissa = u64::from(window & 7);
        return Some(base.saturating_add((base / 8).saturating_mul(mantissa)));
    }
    // With it, the window is the content, whose size follows the dictionary
    // ID.
    let dictionary_id = match descriptor & 3 {
        0 => 0usize,
        1 => 1,
        2 => 2,
        _ => 4,
    };
    let len = match descriptor >> 6 {
        0 => 1usize,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let start = dictionary_id.saturating_add(5);
    let field = frame.get(start..start.saturating_add(len))?;
    let mut size = [0u8; 8];
    size.get_mut(..len)?.copy_from_slice(field);
    let size = u64::from_le_bytes(size);
    // A two-byte size counts from 256.
    Some(if len == 2 {
        size.saturating_add(256)
    } else {
        size
    })
}

fn decoder_from<R: BufRead>(
    pool: &'static Pool<DCtx<'static>>,
    reader: R,
    window_log_max: Option<u32>,
) -> io::Result<Decoder<R>> {
    let mut pooled = pool.take()?;
    // The limit lasts for this checkout only: `Pool::take` resets a kept
    // context's parameters, and a frame change within a reader keeps them.
    if let Some(log) = window_log_max {
        pooled
            .ctx()?
            .set_parameter(DParameter::WindowLogMax(log))
            .map_err(zstd_error)?;
    }
    Ok(zio::Reader::new(reader, pooled))
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

    thread_local! {
        /// Makes every context checkout on this thread fail as zstd does when
        /// it cannot allocate one.
        pub(crate) static OUT_OF_MEMORY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Only a failed allocation is reported as a lack of memory.
    #[test]
    fn a_failed_allocation_is_out_of_memory() {
        let code = |error: u32| 0usize.wrapping_sub(error as usize);
        let memory = zstd_error(code(64));
        assert_eq!(memory.kind(), io::ErrorKind::OutOfMemory);
        assert!(memory.to_string().contains("memory"), "{memory}");
        // `corruption_detected`, and the window refusal of the limit tests.
        for error in [20, 16] {
            assert_eq!(zstd_error(code(error)).kind(), io::ErrorKind::Other);
        }
        assert_eq!(
            decode(&DECODERS, b"not a zstd frame").unwrap_err().kind(),
            io::ErrorKind::Other
        );
    }

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
        decode_within(pool, frames, None)
    }

    fn decode_within(
        pool: &'static Pool<DCtx<'static>>,
        frames: &[u8],
        window_log_max: Option<u32>,
    ) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        decoder_from(pool, frames, window_log_max)?.read_to_end(&mut out)?;
        Ok(out)
    }

    /// A frame of `raw` whose header asks the decoder for a `1 << window_log`
    /// byte window.
    pub(crate) fn frame_with_window(raw: &[u8], window_log: u32) -> Vec<u8> {
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        enc.window_log(window_log).unwrap();
        enc.write_all(raw).unwrap();
        enc.finish().unwrap()
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
        decoder_from(&POOL, &fb[..], None)
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
        decoder_from(
            &POOL,
            BufReader::with_capacity(DCtx::in_size(), &fb[..]),
            None,
        )
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
        let frame = frame_with_window(&raw, 26);
        assert_eq!(decode(&BIG, &frame).unwrap(), raw);
        assert_eq!(BIG.idle().len(), 0, "an outsized context is not kept");
        // A chunk of the maximum size in the store's frame format fits within the limit.
        let raw = sample(MAX_CHUNK_SIZE, 12);
        assert_eq!(decode(&BIG, &reference_frame(&raw)).unwrap(), raw);
        assert_eq!(BIG.idle().len(), 1);
    }

    /// A decoder taken with a window limit refuses a frame asking for more,
    /// without growing: its context is kept and decodes the next frame. The
    /// limit does not follow the context to a checkout taken without one.
    #[test]
    fn a_window_limit_holds_for_one_checkout() {
        static POOL: Pool<DCtx<'static>> = Pool::new();
        let limit = Some(crate::index::CHUNK_WINDOW_LOG_MAX);
        let small = sample(100_000, 13);
        let wide = frame_with_window(&small, 26);

        let refused = decode_within(&POOL, &wide, limit).unwrap_err();
        assert!(refused.to_string().contains("too much memory"), "{refused}");
        assert_eq!(POOL.idle().len(), 1, "the refusing context is kept");

        let chunk = sample(MAX_CHUNK_SIZE, 14);
        let decoded = decode_within(&POOL, &reference_frame(&chunk), limit).unwrap();
        assert_eq!(decoded, chunk);
        assert_eq!(POOL.idle().len(), 1, "the same context decoded it");

        assert_eq!(decode(&POOL, &wide).unwrap(), small);
    }

    /// A frame of known content size declaring a window over the limit is
    /// refused from its header, whatever room the reader is given: with room
    /// for the content, zstd's own limit would not stop it. So is one that
    /// follows a frame within the limit.
    #[test]
    fn a_declared_window_over_the_limit_is_refused_before_decoding() {
        let log = crate::index::CHUNK_WINDOW_LOG_MAX;
        let raw = vec![0u8; 9 << 20];
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 1).unwrap();
        enc.window_log(log + 1).unwrap();
        enc.set_pledged_src_size(Some(raw.len() as u64)).unwrap();
        enc.write_all(&raw).unwrap();
        let wide = enc.finish().unwrap();
        assert_eq!(
            zstd_safe::get_frame_content_size(&wide).ok().flatten(),
            Some(raw.len() as u64)
        );
        assert!(frame_window(&wide).unwrap() > 1 << log);

        for room in [1 << 10, 16 << 20] {
            let mut out = vec![0u8; room];
            let refused = decoder_windowed(&wide, log)
                .and_then(|mut decoder| decoder.read(&mut out))
                .unwrap_err();
            assert!(refused.to_string().contains("byte window"), "{refused}");
        }
        let chunk = sample(MAX_CHUNK_SIZE, 16);
        let both = [reference_frame(&chunk), wide].concat();
        assert!(decoder_windowed(&both, log).is_err());

        // What the store writes passes: a chunk of the maximum size, alone or
        // after another. A window of the limit itself is read as such.
        let frame = reference_frame(&chunk);
        for (frames, chunks) in [(frame.clone(), 1), ([frame.clone(), frame].concat(), 2)] {
            let mut out = Vec::new();
            decoder_windowed(&frames, log)
                .unwrap()
                .read_to_end(&mut out)
                .unwrap();
            assert_eq!(out.len(), chunks * MAX_CHUNK_SIZE);
        }
        assert_eq!(
            frame_window(&frame_with_window(&chunk, log)),
            Some(1 << log)
        );
    }

    /// The limit admits a window of exactly its size and none above, on the
    /// first frame of a stream or on a later one.
    #[test]
    fn a_window_limit_is_exact_and_holds_across_frames() {
        static POOL: Pool<DCtx<'static>> = Pool::new();
        let log = crate::index::CHUNK_WINDOW_LOG_MAX;
        assert_eq!(1usize << log, 8 << 20);
        let raw = sample(100_000, 15);
        let (at, over) = (
            frame_with_window(&raw, log),
            frame_with_window(&raw, log + 1),
        );

        assert_eq!(decode_within(&POOL, &at, Some(log)).unwrap(), raw);
        assert!(decode_within(&POOL, &over, Some(log)).is_err());

        let both = [at, over].concat();
        let mut stream = decoder_from(&POOL, &both[..], Some(log)).unwrap();
        let mut first = vec![0; raw.len()];
        stream.read_exact(&mut first).unwrap();
        assert_eq!(first, raw);
        let refused = stream.read_to_end(&mut Vec::new()).unwrap_err();
        assert!(refused.to_string().contains("too much memory"), "{refused}");
    }
}
