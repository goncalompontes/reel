use std::{
    collections::VecDeque,
    io::SeekFrom,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Poll, Waker},
    time::Instant,
};

use anyhow::Context;
use dashmap::DashMap;

use librqbit_core::lengths::{CurrentPiece, Lengths, ValidPieceIndex};
use tokio::{
    io::{AsyncRead, AsyncSeek},
    sync::OwnedSemaphorePermit,
};
use tracing::{debug, trace};

use crate::{ManagedTorrent, file_info::FileInfo, storage::TorrentStorage};

use super::{ManagedTorrentHandle, TorrentMetadata};

type StreamId = usize;

// How much data around the playhead a stream fetches when no explicit
// look-ahead is configured. The session's `stream_lookahead_bytes` overrides
// it; reel sets that from the user's "Stream buffer (MB)" setting.
pub(crate) const PER_STREAM_BUF_DEFAULT: u64 = 32 * 1024 * 1024;

struct StreamState {
    file_id: usize,
    file_len: u64,
    file_abs_offset: u64,
    position: u64,
    waker: Option<Waker>,
}

impl StreamState {
    fn current_piece(&self, lengths: &Lengths) -> Option<CurrentPiece> {
        lengths.compute_current_piece(self.position, self.file_abs_offset)
    }

    fn queue<'a>(
        &self,
        lengths: &'a Lengths,
        lookahead: u64,
    ) -> impl Iterator<Item = ValidPieceIndex> + use<'a> {
        let start = self.file_abs_offset + self.position;
        let end = start
            .saturating_add(lookahead)
            .min(self.file_abs_offset + self.file_len);
        let dpl = lengths.default_piece_length();
        let start_id = (start / dpl as u64).try_into().unwrap();
        let end_id = end.div_ceil(dpl as u64).try_into().unwrap();
        (start_id..end_id).filter_map(|i| lengths.validate_piece_index(i))
    }
}

#[derive(Default)]
pub(crate) struct TorrentStreams {
    next_stream_id: AtomicUsize,
    streams: DashMap<StreamId, StreamState>,
}

impl TorrentStreams {
    fn next_id(&self) -> usize {
        self.next_stream_id.fetch_add(1, Ordering::Relaxed)
    }

    fn register_waker(&self, stream_id: StreamId, waker: Waker) {
        if let Some(mut s) = self.streams.get_mut(&stream_id) {
            let vm = s.value_mut();
            vm.waker = Some(waker);
        }
    }

    // Interleave 1st, 2nd etc pieces from each active stream in turn until they get 1/10th of the file .
    pub(crate) fn iter_next_pieces<'a>(
        &'a self,
        lengths: &'a Lengths,
        lookahead: u64,
    ) -> impl Iterator<Item = ValidPieceIndex> + 'a {
        struct Interleave<I> {
            all: VecDeque<I>,
        }

        impl<I: Iterator<Item = ValidPieceIndex>> Iterator for Interleave<I> {
            type Item = ValidPieceIndex;

            fn next(&mut self) -> Option<Self::Item> {
                while let Some(mut it) = self.all.pop_front() {
                    if let Some(piece) = it.next() {
                        self.all.push_back(it);
                        return Some(piece);
                    }
                }
                None
            }
        }

        let mut all: Vec<_> = self
            .streams
            .iter()
            .map(|s| s.queue(lengths, lookahead))
            .collect();

        // Shuffle to decrease determinism and make queueing fairer.
        use rand::seq::SliceRandom;
        all.shuffle(&mut rand::rng());

        Interleave { all: all.into() }
    }

    pub(crate) fn wake_streams_on_piece_completed(
        &self,
        piece_id: ValidPieceIndex,
        lengths: &Lengths,
    ) {
        for mut w in self.streams.iter_mut() {
            if w.value().current_piece(lengths).map(|p| p.id) == Some(piece_id)
                && let Some(waker) = w.value_mut().waker.take()
            {
                debug!(
                    stream_id = *w.key(),
                    piece_id = piece_id.get(),
                    "waking stream"
                );
                waker.wake();
            }
        }
    }

    fn drop_stream(&self, stream_id: StreamId) -> Option<StreamState> {
        debug!(stream_id, "dropping stream");
        self.streams.remove(&stream_id).map(|s| s.1)
    }

    pub(crate) fn streamed_file_ids(&self) -> impl Iterator<Item = usize> + '_ {
        self.streams.iter().map(|s| s.value().file_id)
    }
}

pub struct FileStream {
    torrent: ManagedTorrentHandle,
    metadata: Arc<TorrentMetadata>,
    streams: Arc<TorrentStreams>,
    stream_id: usize,
    file_id: usize,
    position: u64,

    // file params
    file_len: u64,
    file_torrent_abs_offset: u64,

    _blocking_permit: OwnedSemaphorePermit,
}

macro_rules! map_io_err {
    ($e:expr) => {
        $e.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))
    };
}

macro_rules! poll_try_io {
    ($e:expr) => {{
        let e = map_io_err!($e);
        match e {
            Ok(r) => r,
            Err(e) => {
                debug!("stream error {e:#}");
                return Poll::Ready(Err(e));
            }
        }
    }};
}

impl AsyncRead for FileStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        tbuf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // if the file is over, return 0
        if self.position == self.file_len {
            debug!(
                stream_id = self.stream_id,
                file_id = self.file_id,
                "stream completed, EOF"
            );
            return Poll::Ready(Ok(()));
        }

        let current = poll_try_io!(
            self.metadata
                .lengths()
                .compute_current_piece(self.position, self.file_torrent_abs_offset)
                .context("invalid position")
        );

        // if the piece is not there, register to wake when it is
        // check if we have the piece for real
        let have = poll_try_io!(self.torrent.with_chunk_tracker(|ct| {
            let have = ct.get_have_pieces().as_slice()[current.id.get() as usize];
            if !have {
                self.streams
                    .register_waker(self.stream_id, cx.waker().clone());
            }
            have
        }));
        if !have {
            debug!(stream_id = self.stream_id, file_id = self.file_id, piece_id = %current.id, "poll pending, not have");
            return Poll::Pending;
        }

        // actually stream the piece
        let buf = tbuf.initialize_unfilled();
        let file_remaining = self.file_len - self.position;
        let bytes_to_read: usize = poll_try_io!(
            (buf.len() as u64)
                .min(current.piece_remaining as u64)
                .min(file_remaining)
                .try_into()
        );

        let buf = &mut buf[..bytes_to_read];

        let start = Instant::now();
        poll_try_io!(poll_try_io!(self.torrent.shared.spawner.block_in_place(
            || {
                self.torrent.with_storage_and_file(
                    self.file_id,
                    |files, _fi| {
                        files.pread_exact(self.file_id, self.position, buf)?;
                        Ok::<_, anyhow::Error>(())
                    },
                    &self.metadata,
                )
            }
        )));

        trace!(
            buflen = buf.len(),
            stream_id = self.stream_id,
            file_id = self.file_id,
            read_time = ?start.elapsed(),
            "will write bytes"
        );

        self.as_mut().advance(bytes_to_read as u64);
        tbuf.advance(bytes_to_read);

        Poll::Ready(Ok(()))
    }
}

impl AsyncSeek for FileStream {
    fn start_seek(
        mut self: std::pin::Pin<&mut Self>,
        position: std::io::SeekFrom,
    ) -> std::io::Result<()> {
        let end_i64 = map_io_err!(TryInto::<i64>::try_into(self.file_len))?;
        let new_pos: i64 = match position {
            SeekFrom::Start(s) => map_io_err!(s.try_into())?,
            SeekFrom::End(e) => map_io_err!(TryInto::<i64>::try_into(self.file_len))? + e,
            SeekFrom::Current(o) => map_io_err!(TryInto::<i64>::try_into(self.position))? + o,
        };

        if new_pos < 0 || new_pos > end_i64 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                anyhow::anyhow!("invalid seek"),
            ));
        }

        self.as_mut().set_position(map_io_err!(new_pos.try_into())?);
        debug!(stream_id = self.stream_id, position = self.position, "seek");
        Ok(())
    }

    fn poll_complete(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> Poll<std::io::Result<u64>> {
        Poll::Ready(Ok(self.position))
    }
}

impl Drop for FileStream {
    fn drop(&mut self) {
        self.streams.drop_stream(self.stream_id);
    }
}

impl ManagedTorrent {
    fn with_storage_and_file<F, R>(
        &self,
        file_id: usize,
        f: F,
        metadata: &TorrentMetadata,
    ) -> anyhow::Result<R>
    where
        F: FnOnce(&dyn TorrentStorage, &FileInfo) -> R,
    {
        self.with_state(|s| {
            let files = match s {
                crate::ManagedTorrentState::Paused(p) => &*p.files,
                crate::ManagedTorrentState::Live(l) => &*l.files,
                s => anyhow::bail!("with_storage_and_file: invalid state: {}", s.name()),
            };
            let fi = metadata.file_infos.get(file_id).context("invalid file")?;
            Ok(f(files, fi))
        })
    }

    fn streams(&self) -> anyhow::Result<Arc<TorrentStreams>> {
        self.with_state(|s| match s {
            crate::ManagedTorrentState::Paused(p) => Ok(p.streams.clone()),
            crate::ManagedTorrentState::Live(l) => Ok(l.streams.clone()),
            s => anyhow::bail!("streams: invalid state {}", s.name()),
        })
    }

    fn maybe_reconnect_needed_peers_for_file(&self, file_id: usize) -> bool {
        // If we have the full file, don't bother.
        if self.is_file_finished(file_id) {
            return false;
        }
        self.with_state(|state| {
            if let crate::ManagedTorrentState::Live(l) = &state {
                l.reconnect_all_not_needed_peers();
            }
        });
        true
    }

    fn is_file_finished(&self, file_id: usize) -> bool {
        let metadata = self.metadata.load();
        let metadata = match metadata.as_ref() {
            Some(r) => r,
            None => return false,
        };
        // TODO: would be nice to remove locking
        self.with_chunk_tracker(|ct| ct.is_file_finished(&metadata.file_infos[file_id]))
            .unwrap_or(false)
    }

    pub async fn stream(self: Arc<Self>, file_id: usize) -> anyhow::Result<FileStream> {
        let metadata = self
            .metadata
            .load_full()
            .context("torrent metadata is not resolved")?;
        let (fd_len, fd_offset) = self.with_storage_and_file(
            file_id,
            |_fd, fi| (fi.len, fi.offset_in_torrent),
            &metadata,
        )?;
        let streams = self.streams()?;
        let blocking_permit = self.shared().spawner.semaphore().acquire_owned().await?;
        let s = FileStream {
            stream_id: streams.next_id(),
            streams: streams.clone(),
            file_id,
            position: 0,

            file_len: fd_len,
            file_torrent_abs_offset: fd_offset,
            _blocking_permit: blocking_permit,
            torrent: self,
            metadata,
        };
        s.torrent.maybe_reconnect_needed_peers_for_file(file_id);
        streams.streams.insert(
            s.stream_id,
            StreamState {
                file_id,
                position: 0,
                waker: None,
                file_len: fd_len,
                file_abs_offset: fd_offset,
            },
        );

        debug!(stream_id = s.stream_id, file_id, "started stream");

        Ok(s)
    }
}

impl FileStream {
    pub fn position(&self) -> u64 {
        self.position
    }

    fn advance(&mut self, diff: u64) {
        self.set_position(self.position + diff)
    }

    fn set_position(&mut self, new_pos: u64) {
        self.position = new_pos;
        self.streams
            .streams
            .get_mut(&self.stream_id)
            .unwrap()
            .value_mut()
            .position = new_pos;
    }

    pub fn len(&self) -> u64 {
        self.file_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PIECE_LEN: u32 = 16384;

    fn make_lengths(num_pieces: u32) -> Lengths {
        Lengths::new(PIECE_LEN as u64 * num_pieces as u64, PIECE_LEN).unwrap()
    }

    fn make_stream(file_abs_offset: u64, file_len: u64, position: u64) -> StreamState {
        StreamState {
            file_id: 0,
            file_len,
            file_abs_offset,
            position,
            waker: None,
        }
    }

    fn window(state: &StreamState, lengths: &Lengths, lookahead: u64) -> Vec<u32> {
        state.queue(lengths, lookahead).map(|p| p.get()).collect()
    }

    #[test]
    fn test_stream_window_empty_with_zero_lookahead() {
        let lengths = make_lengths(10);
        let s = make_stream(0, lengths.total_length(), 0);
        assert!(window(&s, &lengths, 0).is_empty());
    }

    #[test]
    fn test_stream_window_rounds_up_to_piece_boundaries() {
        let lengths = make_lengths(10);
        let s = make_stream(0, lengths.total_length(), 0);

        // Any lookahead short of the next piece boundary still only needs the
        // piece under the playhead.
        assert_eq!(window(&s, &lengths, 1), vec![0]);
        assert_eq!(window(&s, &lengths, PIECE_LEN as u64 - 1), vec![0]);
        // Landing exactly on the boundary is exclusive (half-open window).
        assert_eq!(window(&s, &lengths, PIECE_LEN as u64), vec![0]);
        // One byte past pulls in the next piece.
        assert_eq!(window(&s, &lengths, PIECE_LEN as u64 + 1), vec![0, 1]);
    }

    #[test]
    fn test_stream_window_tracks_playhead() {
        let lengths = make_lengths(10);
        let piece_len = PIECE_LEN as u64;

        // Playhead at the start of piece 1 with a one-piece lookahead.
        let s = make_stream(0, lengths.total_length(), piece_len);
        assert_eq!(window(&s, &lengths, piece_len), vec![1]);

        // A wider lookahead spans the following pieces.
        let s = make_stream(0, lengths.total_length(), piece_len);
        assert_eq!(window(&s, &lengths, piece_len * 3), vec![1, 2, 3]);
    }

    #[test]
    fn test_stream_window_clamped_to_file_end() {
        let lengths = make_lengths(10);
        let file_len = lengths.total_length();

        // Huge lookahead near the end must not read past the file's last piece.
        let s = make_stream(0, file_len, file_len - 1);
        assert_eq!(window(&s, &lengths, u64::MAX), vec![9]);
    }

    #[test]
    fn test_stream_window_empty_at_eof() {
        let lengths = make_lengths(10);
        let file_len = lengths.total_length();
        let s = make_stream(0, file_len, file_len);
        assert!(window(&s, &lengths, PIECE_LEN as u64 * 4).is_empty());
    }

    #[test]
    fn test_stream_window_respects_file_offset() {
        let lengths = make_lengths(10);
        let piece_len = PIECE_LEN as u64;

        // A 3-piece file starting at piece 1 of a larger torrent.
        let file_abs_offset = piece_len;
        let file_len = piece_len * 3;
        let s = make_stream(file_abs_offset, file_len, 0);

        assert_eq!(window(&s, &lengths, piece_len), vec![1]);
        assert_eq!(window(&s, &lengths, piece_len * 2), vec![1, 2]);
        // Even an unbounded lookahead stops at this file's last piece; pieces
        // belonging to other files are never included.
        assert_eq!(window(&s, &lengths, u64::MAX), vec![1, 2, 3]);
    }

    #[test]
    fn test_stream_current_piece_tracks_playhead() {
        let lengths = make_lengths(10);
        let s = make_stream(0, lengths.total_length(), PIECE_LEN as u64 + 5);
        let cur = s.current_piece(&lengths).unwrap();
        assert_eq!(cur.id.get(), 1);
        assert_eq!(cur.piece_remaining, PIECE_LEN - 5);
    }
}
