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

use std::ops::RangeBounds;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use prometheus::HistogramTimer;
use tokio::io::{AsyncRead, AsyncReadExt};

pub mod mem;
pub use mem::*;

pub mod opendal_engine;
pub use opendal_engine::*;

pub mod s3;
use await_tree::InstrumentAwait;
use futures::stream::BoxStream;
pub use s3::*;

pub mod error;
pub mod object_metrics;

pub use error::*;
use object_metrics::ObjectStoreMetrics;

pub type ObjectStoreRef = Arc<ObjectStoreImpl>;
pub type ObjectStreamingUploader = MonitoredStreamingUploader;

type BoxedStreamingUploader = Box<dyn StreamingUploader>;

pub trait ObjectRangeBounds = RangeBounds<usize> + Clone + Send + Sync + std::fmt::Debug + 'static;

/// Partitions a set of given paths into two vectors. The first vector contains all local paths, and
/// the second contains all remote paths.
pub fn partition_object_store_paths(paths: &[String]) -> Vec<String> {
    // ToDo: Currently the result is a copy of the input. Would it be worth it to use an in-place
    //       partition instead?
    let mut vec_rem = vec![];

    for path in paths {
        vec_rem.push(path.to_string());
    }

    vec_rem
}

#[derive(Debug, Clone, PartialEq)]
pub struct ObjectMetadata {
    // Full path
    pub key: String,
    // Seconds since unix epoch.
    pub last_modified: f64,
    pub total_size: usize,
}

#[async_trait::async_trait]
pub trait StreamingUploader: Send {
    async fn write_bytes(&mut self, data: Bytes) -> ObjectResult<()>;

    async fn finish(self: Box<Self>) -> ObjectResult<()>;

    fn get_memory_usage(&self) -> u64;
}

/// The implementation must be thread-safe.
#[async_trait::async_trait]
pub trait ObjectStore: Send + Sync {
    /// Get the key prefix for object
    fn get_object_prefix(&self, obj_id: u64) -> String;

    /// Uploads the object to `ObjectStore`.
    async fn upload(&self, path: &str, obj: Bytes) -> ObjectResult<()>;

    async fn streaming_upload(&self, path: &str) -> ObjectResult<BoxedStreamingUploader>;

    /// If objects are PUT using a multipart upload, it's a good practice to GET them in the same
    /// part sizes (or at least aligned to part boundaries) for best performance.
    /// <https://d1.awsstatic.com/whitepapers/AmazonS3BestPractices.pdf?stod_obj2>
    async fn read(&self, path: &str, range: impl ObjectRangeBounds) -> ObjectResult<Bytes>;

    /// Returns a stream reading the object specified in `path`. If given, the stream starts at the
    /// byte with index `start_pos` (0-based). As far as possible, the stream only loads the amount
    /// of data into memory that is read from the stream.
    async fn streaming_read(
        &self,
        path: &str,
        start_pos: Option<usize>,
    ) -> ObjectResult<Box<dyn AsyncRead + Unpin + Send + Sync>>;

    /// Obtains the object metadata.
    async fn metadata(&self, path: &str) -> ObjectResult<ObjectMetadata>;

    /// Deletes blob permanently.
    async fn delete(&self, path: &str) -> ObjectResult<()>;

    /// Deletes the objects with the given paths permanently from the storage. If an object
    /// specified in the request is not found, it will be considered as successfully deleted.
    async fn delete_objects(&self, paths: &[String]) -> ObjectResult<()>;

    fn monitored(self, metrics: Arc<ObjectStoreMetrics>) -> MonitoredObjectStore<Self>
    where
        Self: Sized,
    {
        MonitoredObjectStore::new(self, metrics)
    }

    async fn list(&self, prefix: &str) -> ObjectResult<ObjectMetadataIter>;

    fn store_media_type(&self) -> &'static str;
}

pub enum ObjectStoreImpl {
    InMem(MonitoredObjectStore<InMemObjectStore>),
    Opendal(MonitoredObjectStore<OpendalObjectStore>),
    S3(MonitoredObjectStore<S3ObjectStore>),
}

macro_rules! dispatch_async {
    ($object_store:expr, $method_name:ident $(, $args:expr)*) => {
        $object_store.$method_name($($args, )*).await
    }
}

/// This macro routes the object store operation to the real implementation by the `ObjectStoreImpl`
/// enum type and the `path`.
///
/// Except for `InMem`,the operation should be performed on remote object store.
macro_rules! object_store_impl_method_body {
    ($object_store:expr, $method_name:ident, $dispatch_macro:ident, $path:expr $(, $args:expr)*) => {
        {
            let path = $path;
            match $object_store {
                ObjectStoreImpl::InMem(in_mem) => {
                    $dispatch_macro!(in_mem, $method_name, path $(, $args)*)
                },
                ObjectStoreImpl::Opendal(opendal) => {
                    $dispatch_macro!(opendal, $method_name, path $(, $args)*)
                },
                ObjectStoreImpl::S3(s3) => {
                    $dispatch_macro!(s3, $method_name, path $(, $args)*)
                },
            }
        }
    };
}

/// This macro routes the object store operation to the real implementation by the `ObjectStoreImpl`
/// enum type and the `paths`. It is a modification of the macro above to work with a slice of
/// strings instead of just a single one.
///
/// Except for `InMem`, the operation should be performed on remote object store.
macro_rules! object_store_impl_method_body_slice {
    ($object_store:expr, $method_name:ident, $dispatch_macro:ident, $paths:expr $(, $args:expr)*) => {
        {
            let paths_rem = partition_object_store_paths($paths);
            match $object_store {
                ObjectStoreImpl::InMem(in_mem) => {
                    $dispatch_macro!(in_mem, $method_name, &paths_rem $(, $args)*)
                },
                ObjectStoreImpl::Opendal(opendal) => {
                    $dispatch_macro!(opendal, $method_name, &paths_rem $(, $args)*)
                },
                ObjectStoreImpl::S3(s3) => {
                    $dispatch_macro!(s3, $method_name, &paths_rem $(, $args)*)
                },
            }
        }
    };
}

impl ObjectStoreImpl {
    pub async fn upload(&self, path: &str, obj: Bytes) -> ObjectResult<()> {
        object_store_impl_method_body!(self, upload, dispatch_async, path, obj)
    }

    pub async fn streaming_upload(&self, path: &str) -> ObjectResult<MonitoredStreamingUploader> {
        object_store_impl_method_body!(self, streaming_upload, dispatch_async, path)
    }

    pub async fn read(&self, path: &str, range: impl ObjectRangeBounds) -> ObjectResult<Bytes> {
        object_store_impl_method_body!(self, read, dispatch_async, path, range)
    }

    pub async fn metadata(&self, path: &str) -> ObjectResult<ObjectMetadata> {
        object_store_impl_method_body!(self, metadata, dispatch_async, path)
    }

    /// Returns a stream reading the object specified in `path`. If given, the stream starts at the
    /// byte with index `start_pos` (0-based). As far as possible, the stream only loads the amount
    /// of data into memory that is read from the stream.
    pub async fn streaming_read(
        &self,
        path: &str,
        start_loc: Option<usize>,
    ) -> ObjectResult<MonitoredStreamingReader> {
        object_store_impl_method_body!(self, streaming_read, dispatch_async, path, start_loc)
    }

    pub async fn delete(&self, path: &str) -> ObjectResult<()> {
        object_store_impl_method_body!(self, delete, dispatch_async, path)
    }

    /// Deletes the objects with the given paths permanently from the storage. If an object
    /// specified in the request is not found, it will be considered as successfully deleted.
    ///
    /// If a hybrid storage is used, the method will first attempt to delete objects in local
    /// storage. Only if that is successful, it will remove objects from remote storage.
    pub async fn delete_objects(&self, paths: &[String]) -> ObjectResult<()> {
        object_store_impl_method_body_slice!(self, delete_objects, dispatch_async, paths)
    }

    pub async fn list(&self, prefix: &str) -> ObjectResult<ObjectMetadataIter> {
        object_store_impl_method_body!(self, list, dispatch_async, prefix)
    }

    pub fn get_object_prefix(&self, obj_id: u64) -> String {
        // FIXME: ObjectStoreImpl lacks flexibility for adding new interface to ObjectStore
        // trait. Macro object_store_impl_method_body routes to local or remote only depending on
        // the path
        match self {
            ObjectStoreImpl::InMem(store) => store.inner.get_object_prefix(obj_id),
            ObjectStoreImpl::Opendal(store) => store.inner.get_object_prefix(obj_id),
            ObjectStoreImpl::S3(store) => store.inner.get_object_prefix(obj_id),
        }
    }

    pub fn support_streaming_upload(&self) -> bool {
        match self {
            ObjectStoreImpl::InMem(_) => true,
            ObjectStoreImpl::Opendal(store) => {
                store
                    .inner
                    .op
                    .info()
                    .capability()
                    .write_without_content_length
            }
            ObjectStoreImpl::S3(_) => true,
        }
    }

    pub fn set_opts(
        &mut self,
        streaming_read_timeout_ms: u64,
        streaming_upload_timeout_ms: u64,
        read_timeout_ms: u64,
        upload_timeout_ms: u64,
    ) {
        match self {
            ObjectStoreImpl::InMem(s) => {
                s.set_opts(
                    streaming_read_timeout_ms,
                    streaming_upload_timeout_ms,
                    read_timeout_ms,
                    upload_timeout_ms,
                );
            }
            ObjectStoreImpl::Opendal(s) => {
                s.set_opts(
                    streaming_read_timeout_ms,
                    streaming_upload_timeout_ms,
                    read_timeout_ms,
                    upload_timeout_ms,
                );
            }
            ObjectStoreImpl::S3(s) => {
                s.set_opts(
                    streaming_read_timeout_ms,
                    streaming_upload_timeout_ms,
                    read_timeout_ms,
                    upload_timeout_ms,
                );
            }
        }
    }
}

fn try_update_failure_metric<T>(
    metrics: &Arc<ObjectStoreMetrics>,
    result: &ObjectResult<T>,
    operation_type: &'static str,
) {
    if result.is_err() {
        metrics
            .failure_count
            .with_label_values(&[operation_type])
            .inc();
    }
}

/// `MonitoredStreamingUploader` will report the following metrics.
/// - `write_bytes`: The number of bytes uploaded from the uploader's creation to finish.
/// - `operation_size`:
///   - `streaming_upload_write_bytes`: The number of bytes written for each call to `write_bytes`.
///   - `streaming_upload`: Same as `write_bytes`.
/// - `operation_latency`:
///   - `streaming_upload_start`: The time spent creating the uploader.
///   - `streaming_upload_write_bytes`: The time spent on each call to `write_bytes`.
///   - `streaming_upload_finish`: The time spent calling `finish`.
/// - `failure_count`: `streaming_upload_start`, `streaming_upload_write_bytes`,
///   `streaming_upload_finish`
pub struct MonitoredStreamingUploader {
    inner: BoxedStreamingUploader,
    object_store_metrics: Arc<ObjectStoreMetrics>,
    /// Length of data uploaded with this uploader.
    operation_size: usize,
    media_type: &'static str,
    streaming_upload_timeout: Option<Duration>,
}

impl MonitoredStreamingUploader {
    pub fn new(
        media_type: &'static str,
        handle: BoxedStreamingUploader,
        object_store_metrics: Arc<ObjectStoreMetrics>,
        streaming_upload_timeout: Option<Duration>,
    ) -> Self {
        Self {
            inner: handle,
            object_store_metrics,
            operation_size: 0,
            media_type,
            streaming_upload_timeout,
        }
    }
}

impl MonitoredStreamingUploader {
    pub async fn write_bytes(&mut self, data: Bytes) -> ObjectResult<()> {
        let operation_type = "streaming_upload_write_bytes";
        let data_len = data.len();
        self.object_store_metrics
            .write_bytes
            .inc_by(data.len() as u64);
        self.object_store_metrics
            .operation_size
            .with_label_values(&[operation_type])
            .observe(data_len as f64);
        let _timer = self
            .object_store_metrics
            .operation_latency
            .with_label_values(&[self.media_type, operation_type])
            .start_timer();
        self.operation_size += data_len;

        let future = async {
            self.inner
                .write_bytes(data)
                .verbose_instrument_await("object_store_streaming_upload_write_bytes")
                .await
        };
        let res = match self.streaming_upload_timeout.as_ref() {
            None => future.await,
            Some(timeout) => tokio::time::timeout(*timeout, future)
                .await
                .unwrap_or_else(|_| {
                    Err(ObjectError::internal(
                        "streaming_upload write_bytes timeout",
                    ))
                }),
        };

        try_update_failure_metric(&self.object_store_metrics, &res, operation_type);
        res
    }

    pub async fn finish(self) -> ObjectResult<()> {
        let operation_type = "streaming_upload_finish";
        self.object_store_metrics
            .operation_size
            .with_label_values(&["streaming_upload"])
            .observe(self.operation_size as f64);
        let _timer = self
            .object_store_metrics
            .operation_latency
            .with_label_values(&[self.media_type, operation_type])
            .start_timer();

        let future = async {
            self.inner
                .finish()
                .verbose_instrument_await("object_store_streaming_upload_finish")
                .await
        };
        let res = match self.streaming_upload_timeout.as_ref() {
            None => future.await,
            Some(timeout) => tokio::time::timeout(*timeout, future)
                .await
                .unwrap_or_else(|_| Err(ObjectError::internal("streaming_upload finish timeout"))),
        };

        try_update_failure_metric(&self.object_store_metrics, &res, operation_type);
        res
    }

    pub fn get_memory_usage(&self) -> u64 {
        self.inner.get_memory_usage()
    }
}

type BoxedStreamingReader = Box<dyn AsyncRead + Unpin + Send + Sync>;
pub struct MonitoredStreamingReader {
    inner: BoxedStreamingReader,
    object_store_metrics: Arc<ObjectStoreMetrics>,
    operation_size: usize,
    media_type: &'static str,
    timer: Option<HistogramTimer>,
    streaming_read_timeout: Option<Duration>,
}

impl MonitoredStreamingReader {
    pub fn new(
        media_type: &'static str,
        handle: BoxedStreamingReader,
        object_store_metrics: Arc<ObjectStoreMetrics>,
        streaming_read_timeout: Option<Duration>,
    ) -> Self {
        let operation_type = "streaming_read";
        let timer = object_store_metrics
            .operation_latency
            .with_label_values(&[media_type, operation_type])
            .start_timer();
        Self {
            inner: handle,
            object_store_metrics,
            operation_size: 0,
            media_type,
            timer: Some(timer),
            streaming_read_timeout,
        }
    }

    // This is a clippy bug, see https://github.com/rust-lang/rust-clippy/issues/11380.
    // TODO: remove `allow` here after the issued is closed.
    #[expect(clippy::needless_pass_by_ref_mut)]
    pub async fn read_bytes(&mut self, buf: &mut [u8]) -> ObjectResult<usize> {
        let operation_type = "streaming_read_read_bytes";
        let data_len = buf.len();
        self.object_store_metrics.read_bytes.inc_by(data_len as u64);
        self.object_store_metrics
            .operation_size
            .with_label_values(&[operation_type])
            .observe(data_len as f64);
        let _timer = self
            .object_store_metrics
            .operation_latency
            .with_label_values(&[self.media_type, operation_type])
            .start_timer();
        self.operation_size += data_len;
        let future = async {
            self.inner
                .read_exact(buf)
                .verbose_instrument_await("object_store_streaming_read_read_bytes")
                .await
                .map_err(|err| {
                    ObjectError::internal(format!("read_bytes failed, error: {:?}", err))
                })
        };
        let res = match self.streaming_read_timeout.as_ref() {
            None => future.await,
            Some(timeout) => tokio::time::timeout(*timeout, future)
                .await
                .unwrap_or_else(|_| {
                    Err(ObjectError::internal("streaming_read read_bytes timeout"))
                }),
        };

        try_update_failure_metric(&self.object_store_metrics, &res, operation_type);
        res
    }
}

impl Drop for MonitoredStreamingReader {
    fn drop(&mut self) {
        let operation_type = "streaming_read";
        self.object_store_metrics
            .operation_size
            .with_label_values(&[operation_type])
            .observe(self.operation_size as f64);
        self.timer.take().unwrap().observe_duration();
    }
}

pub struct MonitoredObjectStore<OS: ObjectStore> {
    inner: OS,
    object_store_metrics: Arc<ObjectStoreMetrics>,
    streaming_read_timeout: Option<Duration>,
    streaming_upload_timeout: Option<Duration>,
    read_timeout: Option<Duration>,
    upload_timeout: Option<Duration>,
}

/// Manually dispatch trait methods.
///
/// The metrics are updated in the following order:
/// - Write operations
///   - `write_bytes`
///   - `operation_size`
///   - start `operation_latency` timer
///   - `failure_count`
/// - Read operations
///   - start `operation_latency` timer
///   - `failure-count`
///   - `read_bytes`
///   - `operation_size`
/// - Other
///   - start `operation_latency` timer
///   - `failure-count`
impl<OS: ObjectStore> MonitoredObjectStore<OS> {
    pub fn new(store: OS, object_store_metrics: Arc<ObjectStoreMetrics>) -> Self {
        Self {
            inner: store,
            object_store_metrics,
            streaming_read_timeout: None,
            streaming_upload_timeout: None,
            read_timeout: None,
            upload_timeout: None,
        }
    }

    fn media_type(&self) -> &'static str {
        self.inner.store_media_type()
    }

    pub fn inner(&self) -> &OS {
        &self.inner
    }

    pub async fn upload(&self, path: &str, obj: Bytes) -> ObjectResult<()> {
        let operation_type = "upload";
        self.object_store_metrics
            .write_bytes
            .inc_by(obj.len() as u64);
        self.object_store_metrics
            .operation_size
            .with_label_values(&[operation_type])
            .observe(obj.len() as f64);
        let _timer = self
            .object_store_metrics
            .operation_latency
            .with_label_values(&[self.media_type(), operation_type])
            .start_timer();
        let future = async {
            self.inner
                .upload(path, obj)
                .verbose_instrument_await("object_store_upload")
                .await
        };
        let res = match self.upload_timeout.as_ref() {
            None => future.await,
            Some(timeout) => tokio::time::timeout(*timeout, future)
                .await
                .unwrap_or_else(|_| Err(ObjectError::internal("upload timeout"))),
        };

        try_update_failure_metric(&self.object_store_metrics, &res, operation_type);
        res
    }

    pub async fn streaming_upload(&self, path: &str) -> ObjectResult<MonitoredStreamingUploader> {
        let operation_type = "streaming_upload_start";
        let media_type = self.media_type();
        let _timer = self
            .object_store_metrics
            .operation_latency
            .with_label_values(&[media_type, operation_type])
            .start_timer();
        let future = async {
            self.inner
                .streaming_upload(path)
                .verbose_instrument_await("object_store_streaming_upload")
                .await
        };
        let res = match self.streaming_upload_timeout.as_ref() {
            None => future.await,
            Some(timeout) => tokio::time::timeout(*timeout, future)
                .await
                .unwrap_or_else(|_| Err(ObjectError::internal("streaming_upload init timeout"))),
        };

        try_update_failure_metric(&self.object_store_metrics, &res, operation_type);
        Ok(MonitoredStreamingUploader::new(
            media_type,
            res?,
            self.object_store_metrics.clone(),
            self.streaming_upload_timeout,
        ))
    }

    pub async fn read(&self, path: &str, range: impl ObjectRangeBounds) -> ObjectResult<Bytes> {
        let operation_type = "read";
        let _timer = self
            .object_store_metrics
            .operation_latency
            .with_label_values(&[self.media_type(), operation_type])
            .start_timer();
        let future = async {
            self.inner
                .read(path, range)
                .verbose_instrument_await("object_store_read")
                .await
        };
        let res = match self.read_timeout.as_ref() {
            None => future.await,
            Some(read_timeout) => tokio::time::timeout(*read_timeout, future)
                .await
                .unwrap_or_else(|_| Err(ObjectError::internal("read timeout"))),
        };

        try_update_failure_metric(&self.object_store_metrics, &res, operation_type);

        let data = res?;
        self.object_store_metrics
            .read_bytes
            .inc_by(data.len() as u64);
        self.object_store_metrics
            .operation_size
            .with_label_values(&[operation_type])
            .observe(data.len() as f64);
        Ok(data)
    }

    /// Returns a stream reading the object specified in `path`. If given, the stream starts at the
    /// byte with index `start_pos` (0-based). As far as possible, the stream only loads the amount
    /// of data into memory that is read from the stream.
    async fn streaming_read(
        &self,
        path: &str,
        start_pos: Option<usize>,
    ) -> ObjectResult<MonitoredStreamingReader> {
        let operation_type = "streaming_read_start";
        let media_type = self.media_type();
        let _timer = self
            .object_store_metrics
            .operation_latency
            .with_label_values(&[media_type, operation_type])
            .start_timer();
        let future = async {
            self.inner
                .streaming_read(path, start_pos)
                .verbose_instrument_await("object_store_streaming_read")
                .await
        };
        let res = match self.streaming_read_timeout.as_ref() {
            None => future.await,
            Some(timeout) => tokio::time::timeout(*timeout, future)
                .await
                .unwrap_or_else(|_| Err(ObjectError::internal("streaming_read init timeout"))),
        };

        try_update_failure_metric(&self.object_store_metrics, &res, operation_type);
        Ok(MonitoredStreamingReader::new(
            media_type,
            res?,
            self.object_store_metrics.clone(),
            self.streaming_read_timeout,
        ))
    }

    pub async fn metadata(&self, path: &str) -> ObjectResult<ObjectMetadata> {
        let operation_type = "metadata";
        let _timer = self
            .object_store_metrics
            .operation_latency
            .with_label_values(&[self.media_type(), operation_type])
            .start_timer();

        let future = async {
            self.inner
                .metadata(path)
                .verbose_instrument_await("object_store_metadata")
                .await
        };
        let res = match self.read_timeout.as_ref() {
            None => future.await,
            Some(timeout) => tokio::time::timeout(*timeout, future)
                .await
                .unwrap_or_else(|_| Err(ObjectError::internal("metadata timeout"))),
        };

        try_update_failure_metric(&self.object_store_metrics, &res, operation_type);
        res
    }

    pub async fn delete(&self, path: &str) -> ObjectResult<()> {
        let operation_type = "delete";
        let _timer = self
            .object_store_metrics
            .operation_latency
            .with_label_values(&[self.media_type(), operation_type])
            .start_timer();

        let future = async {
            self.inner
                .delete(path)
                .verbose_instrument_await("object_store_delete")
                .await
        };
        let res = match self.read_timeout.as_ref() {
            None => future.await,
            Some(timeout) => tokio::time::timeout(*timeout, future)
                .await
                .unwrap_or_else(|_| Err(ObjectError::internal("delete timeout"))),
        };

        try_update_failure_metric(&self.object_store_metrics, &res, operation_type);
        res
    }

    async fn delete_objects(&self, paths: &[String]) -> ObjectResult<()> {
        let operation_type = "delete_objects";
        let _timer = self
            .object_store_metrics
            .operation_latency
            .with_label_values(&[self.media_type(), operation_type])
            .start_timer();

        let future = async {
            self.inner
                .delete_objects(paths)
                .verbose_instrument_await("object_store_delete_objects")
                .await
        };
        let res = match self.read_timeout.as_ref() {
            None => future.await,
            Some(timeout) => tokio::time::timeout(*timeout, future)
                .await
                .unwrap_or_else(|_| Err(ObjectError::internal("delete_objects timeout"))),
        };

        try_update_failure_metric(&self.object_store_metrics, &res, operation_type);
        res
    }

    pub async fn list(&self, prefix: &str) -> ObjectResult<ObjectMetadataIter> {
        let operation_type = "list";
        let _timer = self
            .object_store_metrics
            .operation_latency
            .with_label_values(&[self.media_type(), operation_type])
            .start_timer();

        let future = async {
            self.inner
                .list(prefix)
                .verbose_instrument_await("object_store_list")
                .await
        };
        let res = match self.read_timeout.as_ref() {
            None => future.await,
            Some(timeout) => tokio::time::timeout(*timeout, future)
                .await
                .unwrap_or_else(|_| Err(ObjectError::internal("list timeout"))),
        };

        try_update_failure_metric(&self.object_store_metrics, &res, operation_type);
        res
    }

    fn set_opts(
        &mut self,
        streaming_read_timeout_ms: u64,
        streaming_upload_timeout_ms: u64,
        read_timeout_ms: u64,
        upload_timeout_ms: u64,
    ) {
        self.streaming_read_timeout = Some(Duration::from_millis(streaming_read_timeout_ms));
        self.streaming_upload_timeout = Some(Duration::from_millis(streaming_upload_timeout_ms));
        self.read_timeout = Some(Duration::from_millis(read_timeout_ms));
        self.upload_timeout = Some(Duration::from_millis(upload_timeout_ms));
    }
}

pub async fn parse_remote_object_store(
    url: &str,
    metrics: Arc<ObjectStoreMetrics>,
    ident: &str,
) -> ObjectStoreImpl {
    match url {
        s3 if s3.starts_with("s3://") => ObjectStoreImpl::S3(
            S3ObjectStore::new(
                s3.strip_prefix("s3://").unwrap().to_string(),
                metrics.clone(),
            )
            .await
            .monitored(metrics),
        ),
        #[cfg(feature = "hdfs-backend")]
        hdfs if hdfs.starts_with("hdfs://") => {
            let hdfs = hdfs.strip_prefix("hdfs://").unwrap();
            let (namenode, root) = hdfs.split_once('@').unwrap_or((hdfs, ""));
            ObjectStoreImpl::Opendal(
                OpendalObjectStore::new_hdfs_engine(namenode.to_string(), root.to_string())
                    .unwrap()
                    .monitored(metrics),
            )
        }
        gcs if gcs.starts_with("gcs://") => {
            let gcs = gcs.strip_prefix("gcs://").unwrap();
            let (bucket, root) = gcs.split_once('@').unwrap_or((gcs, ""));
            ObjectStoreImpl::Opendal(
                OpendalObjectStore::new_gcs_engine(bucket.to_string(), root.to_string())
                    .unwrap()
                    .monitored(metrics),
            )
        }

        oss if oss.starts_with("oss://") => {
            let oss = oss.strip_prefix("oss://").unwrap();
            let (bucket, root) = oss.split_once('@').unwrap_or((oss, ""));
            ObjectStoreImpl::Opendal(
                OpendalObjectStore::new_oss_engine(bucket.to_string(), root.to_string())
                    .unwrap()
                    .monitored(metrics),
            )
        }
        webhdfs if webhdfs.starts_with("webhdfs://") => {
            let webhdfs = webhdfs.strip_prefix("webhdfs://").unwrap();
            let (namenode, root) = webhdfs.split_once('@').unwrap_or((webhdfs, ""));
            ObjectStoreImpl::Opendal(
                OpendalObjectStore::new_webhdfs_engine(namenode.to_string(), root.to_string())
                    .unwrap()
                    .monitored(metrics),
            )
        }
        azblob if azblob.starts_with("azblob://") => {
            let azblob = azblob.strip_prefix("azblob://").unwrap();
            let (container_name, root) = azblob.split_once('@').unwrap_or((azblob, ""));
            ObjectStoreImpl::Opendal(
                OpendalObjectStore::new_azblob_engine(container_name.to_string(), root.to_string())
                    .unwrap()
                    .monitored(metrics),
            )
        }
        fs if fs.starts_with("fs://") => ObjectStoreImpl::Opendal(
            // Now fs engine is only used in CI, so we can hardcode root.
            OpendalObjectStore::new_fs_engine("/tmp/rw_ci".to_string())
                .unwrap()
                .monitored(metrics),
        ),

        s3_compatible if s3_compatible.starts_with("s3-compatible://") => {
            tracing::error!("The s3 compatible mode has been unified with s3.");
            tracing::error!("If you want to use s3 compatible storage, please set your access_key, secret_key and region to the environment variable AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_REGION,
            set your endpoint to the environment variable RW_S3_ENDPOINT.");
            panic!("Passing s3-compatible is not supported, please modify the environment variable and pass in s3.");
        }
        minio if minio.starts_with("minio://") => ObjectStoreImpl::S3(
            S3ObjectStore::with_minio(minio, metrics.clone())
                .await
                .monitored(metrics),
        ),
        "memory" => {
            if ident == "Meta Backup" {
                tracing::warn!("You're using in-memory remote object store for {}. This is not recommended for production environment.", ident);
            } else {
                tracing::warn!("You're using in-memory remote object store for {}. This should never be used in benchmarks and production environment.", ident);
            }
            ObjectStoreImpl::InMem(InMemObjectStore::new().monitored(metrics))
        }
        "memory-shared" => {
            if ident == "Meta Backup" {
                tracing::warn!("You're using shared in-memory remote object store for {}. This should never be used in production environment.", ident);
            } else {
                tracing::warn!("You're using shared in-memory remote object store for {}. This should never be used in benchmarks and production environment.", ident);
            }
            ObjectStoreImpl::InMem(InMemObjectStore::shared().monitored(metrics))
        }
        other => {
            unimplemented!(
                "{} remote object store only supports s3, minio, gcs, oss, cos, azure blob, hdfs, disk, memory, and memory-shared.",
                other
            )
        }
    }
}

pub type ObjectMetadataIter = BoxStream<'static, ObjectResult<ObjectMetadata>>;
