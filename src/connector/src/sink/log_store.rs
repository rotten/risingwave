// Copyright 2023 RisingWave Labs
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::cmp::Ordering;
use std::fmt::Debug;
use std::future::Future;
use std::sync::Arc;

use anyhow::anyhow;
use risingwave_common::array::StreamChunk;
use risingwave_common::buffer::Bitmap;
use risingwave_common::util::epoch::EpochPair;

pub type LogStoreResult<T> = Result<T, anyhow::Error>;
pub type ChunkId = usize;

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum TruncateOffset {
    Chunk { epoch: u64, chunk_id: ChunkId },
    Barrier { epoch: u64 },
}

impl PartialOrd for TruncateOffset {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let extract = |offset: &TruncateOffset| match offset {
            TruncateOffset::Chunk { epoch, chunk_id } => (*epoch, *chunk_id),
            TruncateOffset::Barrier { epoch } => (*epoch, usize::MAX),
        };
        let this = extract(self);
        let other = extract(other);
        this.partial_cmp(&other)
    }
}

impl TruncateOffset {
    pub fn next_chunk_id(&self) -> ChunkId {
        match self {
            TruncateOffset::Chunk { chunk_id, .. } => chunk_id + 1,
            TruncateOffset::Barrier { .. } => 0,
        }
    }

    pub fn epoch(&self) -> u64 {
        match self {
            TruncateOffset::Chunk { epoch, .. } | TruncateOffset::Barrier { epoch } => *epoch,
        }
    }

    pub fn check_next_item_epoch(&self, epoch: u64) -> LogStoreResult<()> {
        match self {
            TruncateOffset::Chunk {
                epoch: offset_epoch,
                ..
            } => {
                if epoch != *offset_epoch {
                    return Err(anyhow!(
                        "new item epoch {} not match current chunk offset epoch {}",
                        epoch,
                        offset_epoch
                    ));
                }
            }
            TruncateOffset::Barrier {
                epoch: offset_epoch,
            } => {
                if epoch <= *offset_epoch {
                    return Err(anyhow!(
                        "new item epoch {} not exceed barrier offset epoch {}",
                        epoch,
                        offset_epoch
                    ));
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
pub enum LogStoreReadItem {
    StreamChunk {
        chunk: StreamChunk,
        chunk_id: ChunkId,
    },
    Barrier {
        is_checkpoint: bool,
    },
    UpdateVnodeBitmap(Arc<Bitmap>),
}

pub trait LogWriter {
    /// Initialize the log writer with an epoch
    fn init(&mut self, epoch: EpochPair) -> impl Future<Output = LogStoreResult<()>> + Send + '_;

    /// Write a stream chunk to the log writer
    fn write_chunk(
        &mut self,
        chunk: StreamChunk,
    ) -> impl Future<Output = LogStoreResult<()>> + Send + '_;

    /// Mark current epoch as finished and sealed, and flush the unconsumed log data.
    fn flush_current_epoch(
        &mut self,
        next_epoch: u64,
        is_checkpoint: bool,
    ) -> impl Future<Output = LogStoreResult<()>> + Send + '_;

    /// Update the vnode bitmap of the log writer
    fn update_vnode_bitmap(
        &mut self,
        new_vnodes: Arc<Bitmap>,
    ) -> impl Future<Output = LogStoreResult<()>> + Send + '_;
}

pub trait LogReader: Send + Sized + 'static {
    /// Initialize the log reader. Usually function as waiting for log writer to be initialized.
    fn init(&mut self) -> impl Future<Output = LogStoreResult<()>> + Send + '_;

    /// Emit the next item.
    fn next_item(
        &mut self,
    ) -> impl Future<Output = LogStoreResult<(u64, LogStoreReadItem)>> + Send + '_;

    /// Mark that all items emitted so far have been consumed and it is safe to truncate the log
    /// from the current offset.
    fn truncate(
        &mut self,
        offset: TruncateOffset,
    ) -> impl Future<Output = LogStoreResult<()>> + Send + '_;
}

pub trait LogStoreFactory: 'static {
    type Reader: LogReader + Send + 'static;
    type Writer: LogWriter + Send + 'static;

    fn build(self) -> impl Future<Output = (Self::Reader, Self::Writer)> + Send;
}

pub struct TransformChunkLogReader<F: Fn(StreamChunk) -> StreamChunk, R: LogReader> {
    f: F,
    inner: R,
}

impl<F: Fn(StreamChunk) -> StreamChunk + Send + 'static, R: LogReader> LogReader
    for TransformChunkLogReader<F, R>
{
    fn init(&mut self) -> impl Future<Output = LogStoreResult<()>> + Send + '_ {
        self.inner.init()
    }

    async fn next_item(&mut self) -> LogStoreResult<(u64, LogStoreReadItem)> {
        let (epoch, item) = self.inner.next_item().await?;
        let item = match item {
            LogStoreReadItem::StreamChunk { chunk, chunk_id } => LogStoreReadItem::StreamChunk {
                chunk: (self.f)(chunk),
                chunk_id,
            },
            other => other,
        };
        Ok((epoch, item))
    }

    fn truncate(
        &mut self,
        offset: TruncateOffset,
    ) -> impl Future<Output = LogStoreResult<()>> + Send + '_ {
        self.inner.truncate(offset)
    }
}

#[easy_ext::ext(LogStoreTransformChunkLogReader)]
impl<T> T
where
    T: LogReader,
{
    pub fn transform_chunk<F: Fn(StreamChunk) -> StreamChunk + Sized>(
        self,
        f: F,
    ) -> TransformChunkLogReader<F, Self> {
        TransformChunkLogReader { f, inner: self }
    }
}

#[cfg(test)]
mod tests {
    use crate::sink::log_store::TruncateOffset;

    #[test]
    fn test_truncate_offset_cmp() {
        assert!(
            TruncateOffset::Barrier { epoch: 232 }
                < TruncateOffset::Chunk {
                    epoch: 233,
                    chunk_id: 1
                }
        );
        assert_eq!(
            TruncateOffset::Chunk {
                epoch: 1,
                chunk_id: 1
            },
            TruncateOffset::Chunk {
                epoch: 1,
                chunk_id: 1
            }
        );
        assert!(
            TruncateOffset::Chunk {
                epoch: 1,
                chunk_id: 1
            } < TruncateOffset::Chunk {
                epoch: 1,
                chunk_id: 2
            }
        );
        assert!(
            TruncateOffset::Barrier { epoch: 1 }
                > TruncateOffset::Chunk {
                    epoch: 1,
                    chunk_id: 2
                }
        );
        assert!(
            TruncateOffset::Chunk {
                epoch: 1,
                chunk_id: 2
            } < TruncateOffset::Barrier { epoch: 1 }
        );
        assert!(
            TruncateOffset::Chunk {
                epoch: 2,
                chunk_id: 2
            } > TruncateOffset::Barrier { epoch: 1 }
        );
        assert!(TruncateOffset::Barrier { epoch: 2 } > TruncateOffset::Barrier { epoch: 1 });
    }
}
