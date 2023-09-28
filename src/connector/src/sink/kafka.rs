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

use std::collections::{HashMap, VecDeque};
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use futures::future::try_join_all;
use futures::{Future, FutureExt};
use rdkafka::error::{KafkaError, KafkaResult};
use rdkafka::message::ToBytes;
use rdkafka::producer::{DeliveryFuture, FutureProducer, FutureRecord};
use rdkafka::types::RDKafkaErrorCode;
use rdkafka::ClientConfig;
use risingwave_common::array::StreamChunk;
use risingwave_common::catalog::Schema;
use serde_derive::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};
use strum_macros::{Display, EnumString};

use super::{
    Sink, SinkError, SinkParam, SINK_TYPE_APPEND_ONLY, SINK_TYPE_DEBEZIUM, SINK_TYPE_OPTION,
    SINK_TYPE_UPSERT,
};
use crate::common::KafkaCommon;
use crate::sink::formatter::SinkFormatterImpl;
use crate::sink::writer::{
    FormattedSink, LogSinkerOf, SinkWriterExt, SinkWriterV1, SinkWriterV1Adapter,
};
use crate::sink::{DummySinkCommitCoordinator, Result, SinkWriterParam};
use crate::source::kafka::{KafkaProperties, KafkaSplitEnumerator, PrivateLinkProducerContext};
use crate::source::{SourceEnumeratorContext, SplitEnumerator};
use crate::{
    deserialize_bool_from_string, deserialize_duration_from_string, deserialize_u32_from_string,
    dispatch_sink_formatter_impl,
};

pub const KAFKA_SINK: &str = "kafka";

const fn _default_max_retries() -> u32 {
    3
}

const fn _default_retry_backoff() -> Duration {
    Duration::from_millis(100)
}

const fn _default_force_append_only() -> bool {
    false
}

const fn _default_message_timeout_ms() -> usize {
    5000
}

const fn _default_max_in_flight_requests_per_connection() -> usize {
    5
}

#[derive(Debug, Clone, PartialEq, Display, Serialize, Deserialize, EnumString)]
#[strum(serialize_all = "snake_case")]
enum CompressionCodec {
    None,
    Gzip,
    Snappy,
    Lz4,
    Zstd,
}

/// See <https://github.com/confluentinc/librdkafka/blob/master/CONFIGURATION.md>
/// for the detailed meaning of these librdkafka producer properties
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RdKafkaPropertiesProducer {
    /// Maximum number of messages allowed on the producer queue. This queue is shared by all
    /// topics and partitions. A value of 0 disables this limit.
    #[serde(rename = "properties.queue.buffering.max.messages")]
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub queue_buffering_max_messages: Option<usize>,

    /// Maximum total message size sum allowed on the producer queue. This queue is shared by all
    /// topics and partitions. This property has higher priority than queue.buffering.max.messages.
    #[serde(rename = "properties.queue.buffering.max.kbytes")]
    #[serde_as(as = "Option<DisplayFromStr>")]
    queue_buffering_max_kbytes: Option<usize>,

    /// Delay in milliseconds to wait for messages in the producer queue to accumulate before
    /// constructing message batches (MessageSets) to transmit to brokers. A higher value allows
    /// larger and more effective (less overhead, improved compression) batches of messages to
    /// accumulate at the expense of increased message delivery latency.
    #[serde(rename = "properties.queue.buffering.max.ms")]
    #[serde_as(as = "Option<DisplayFromStr>")]
    queue_buffering_max_ms: Option<f64>,

    /// When set to true, the producer will ensure that messages are successfully produced exactly
    /// once and in the original produce order. The following configuration properties are adjusted
    /// automatically (if not modified by the user) when idempotence is enabled:
    /// max.in.flight.requests.per.connection=5 (must be less than or equal to 5),
    /// retries=INT32_MAX (must be greater than 0), acks=all, queuing.strategy=fifo. Producer
    /// will fail if user-supplied configuration is incompatible.
    #[serde(rename = "properties.enable.idempotence")]
    #[serde_as(as = "Option<DisplayFromStr>")]
    enable_idempotence: Option<bool>,

    /// How many times to retry sending a failing Message.
    #[serde(rename = "properties.message.send.max.retries")]
    #[serde_as(as = "Option<DisplayFromStr>")]
    message_send_max_retries: Option<usize>,

    /// The backoff time in milliseconds before retrying a protocol request.
    #[serde(rename = "properties.retry.backoff.ms")]
    #[serde_as(as = "Option<DisplayFromStr>")]
    retry_backoff_ms: Option<usize>,

    /// Maximum number of messages batched in one MessageSet
    #[serde(rename = "properties.batch.num.messages")]
    #[serde_as(as = "Option<DisplayFromStr>")]
    batch_num_messages: Option<usize>,

    /// Maximum size (in bytes) of all messages batched in one MessageSet, including protocol
    /// framing overhead. This limit is applied after the first message has been added to the
    /// batch, regardless of the first message's size, this is to ensure that messages that exceed
    /// batch.size are produced.
    #[serde(rename = "properties.batch.size")]
    #[serde_as(as = "Option<DisplayFromStr>")]
    batch_size: Option<usize>,

    /// Compression codec to use for compressing message sets.
    #[serde(rename = "properties.compression.codec")]
    #[serde_as(as = "Option<DisplayFromStr>")]
    compression_codec: Option<CompressionCodec>,

    /// Produce message timeout.
    /// This value is used to limits the time a produced message waits for
    /// successful delivery (including retries).
    #[serde(
        rename = "properties.message.timeout.ms",
        default = "_default_message_timeout_ms"
    )]
    #[serde_as(as = "DisplayFromStr")]
    message_timeout_ms: usize,

    /// The maximum number of unacknowledged requests the client will send on a single connection before blocking.
    #[serde(
        rename = "properties.max.in.flight.requests.per.connection",
        default = "_default_max_in_flight_requests_per_connection"
    )]
    #[serde_as(as = "DisplayFromStr")]
    max_in_flight_requests_per_connection: usize,
}

impl RdKafkaPropertiesProducer {
    pub(crate) fn set_client(&self, c: &mut rdkafka::ClientConfig) {
        if let Some(v) = self.queue_buffering_max_messages {
            c.set("queue.buffering.max.messages", v.to_string());
        }
        if let Some(v) = self.queue_buffering_max_kbytes {
            c.set("queue.buffering.max.kbytes", v.to_string());
        }
        if let Some(v) = self.queue_buffering_max_ms {
            c.set("queue.buffering.max.ms", v.to_string());
        }
        if let Some(v) = self.enable_idempotence {
            c.set("enable.idempotence", v.to_string());
        }
        if let Some(v) = self.message_send_max_retries {
            c.set("message.send.max.retries", v.to_string());
        }
        if let Some(v) = self.retry_backoff_ms {
            c.set("retry.backoff.ms", v.to_string());
        }
        if let Some(v) = self.batch_num_messages {
            c.set("batch.num.messages", v.to_string());
        }
        if let Some(v) = self.batch_size {
            c.set("batch.size", v.to_string());
        }
        if let Some(v) = &self.compression_codec {
            c.set("compression.codec", v.to_string());
        }
        c.set("message.timeout.ms", self.message_timeout_ms.to_string());
        c.set(
            "max.in.flight.requests.per.connection",
            self.max_in_flight_requests_per_connection.to_string(),
        );
    }
}

#[serde_as]
#[derive(Debug, Clone, Deserialize)]
pub struct KafkaConfig {
    #[serde(skip_serializing)]
    pub connector: String, // Must be "kafka" here.

    // #[serde(rename = "connection.name")]
    // pub connection: String,
    #[serde(flatten)]
    pub common: KafkaCommon,

    pub r#type: String, // accept "append-only", "debezium", or "upsert"

    #[serde(
        default = "_default_force_append_only",
        deserialize_with = "deserialize_bool_from_string"
    )]
    pub force_append_only: bool,

    #[serde(
        rename = "properties.retry.max",
        default = "_default_max_retries",
        deserialize_with = "deserialize_u32_from_string"
    )]
    pub max_retry_num: u32,

    #[serde(
        rename = "properties.retry.interval",
        default = "_default_retry_backoff",
        deserialize_with = "deserialize_duration_from_string"
    )]
    pub retry_interval: Duration,

    /// We have parsed the primary key for an upsert kafka sink into a `usize` vector representing
    /// the indices of the pk columns in the frontend, so we simply store the primary key here
    /// as a string.
    pub primary_key: Option<String>,

    #[serde(flatten)]
    pub rdkafka_properties: RdKafkaPropertiesProducer,
}

impl KafkaConfig {
    pub fn from_hashmap(values: HashMap<String, String>) -> Result<Self> {
        let config = serde_json::from_value::<KafkaConfig>(serde_json::to_value(values).unwrap())
            .map_err(|e| SinkError::Config(anyhow!(e)))?;

        if config.r#type != SINK_TYPE_APPEND_ONLY
            && config.r#type != SINK_TYPE_DEBEZIUM
            && config.r#type != SINK_TYPE_UPSERT
        {
            return Err(SinkError::Config(anyhow!(
                "`{}` must be {}, {}, or {}",
                SINK_TYPE_OPTION,
                SINK_TYPE_APPEND_ONLY,
                SINK_TYPE_DEBEZIUM,
                SINK_TYPE_UPSERT
            )));
        }
        Ok(config)
    }

    pub(crate) fn set_client(&self, c: &mut rdkafka::ClientConfig) {
        self.common.set_client(c);
        self.rdkafka_properties.set_client(c);

        tracing::info!("kafka client starts with: {:?}", c);
    }
}

impl From<KafkaConfig> for KafkaProperties {
    fn from(val: KafkaConfig) -> Self {
        KafkaProperties {
            bytes_per_second: None,
            max_num_messages: None,
            scan_startup_mode: None,
            time_offset: None,
            consumer_group: None,
            upsert: None,
            common: val.common,
            rdkafka_properties: Default::default(),
        }
    }
}

#[derive(Debug)]
pub struct KafkaSink {
    pub config: KafkaConfig,
    schema: Schema,
    pk_indices: Vec<usize>,
    is_append_only: bool,
    db_name: String,
    sink_from_name: String,
}

impl TryFrom<SinkParam> for KafkaSink {
    type Error = SinkError;

    fn try_from(param: SinkParam) -> std::result::Result<Self, Self::Error> {
        let schema = param.schema();
        let config = KafkaConfig::from_hashmap(param.properties)?;
        Ok(Self {
            config,
            schema,
            pk_indices: param.downstream_pk,
            is_append_only: param.sink_type.is_append_only(),
            db_name: param.db_name,
            sink_from_name: param.sink_from_name,
        })
    }
}

impl Sink for KafkaSink {
    type Coordinator = DummySinkCommitCoordinator;
    type LogSinker = LogSinkerOf<SinkWriterV1Adapter<KafkaSinkWriter>>;

    const SINK_NAME: &'static str = KAFKA_SINK;

    async fn new_log_sinker(&self, writer_param: SinkWriterParam) -> Result<Self::LogSinker> {
        Ok(SinkWriterV1Adapter::new(
            KafkaSinkWriter::new(
                self.config.clone(),
                SinkFormatterImpl::new(
                    &self.config.r#type,
                    self.schema.clone(),
                    self.pk_indices.clone(),
                    self.is_append_only,
                    self.db_name.clone(),
                    self.sink_from_name.clone(),
                )?,
            )
            .await?,
        )
        .into_log_sinker(writer_param.sink_metrics))
    }

    async fn validate(&self) -> Result<()> {
        // For upsert Kafka sink, the primary key must be defined.
        if !self.is_append_only && self.pk_indices.is_empty() {
            return Err(SinkError::Config(anyhow!(
                "primary key not defined for {} kafka sink (please define in `primary_key` field)",
                self.config.r#type
            )));
        }

        // Try Kafka connection.
        // There is no such interface for kafka producer to validate a connection
        // use enumerator to validate broker reachability and existence of topic
        let check = KafkaSplitEnumerator::new(
            KafkaProperties::from(self.config.clone()),
            Arc::new(SourceEnumeratorContext::default()),
        )
        .await?;
        if !check.check_reachability().await {
            return Err(SinkError::Config(anyhow!(
                "cannot connect to kafka broker ({})",
                self.config.common.brokers
            )));
        }
        Ok(())
    }
}

/// When the `DeliveryFuture` the current `future_delivery_buffer`
/// is buffering is greater than `queue_buffering_max_messages` * `KAFKA_WRITER_MAX_QUEUE_SIZE_RATIO`,
/// then enforcing commit once
const KAFKA_WRITER_MAX_QUEUE_SIZE_RATIO: f32 = 1.2;
/// The default queue size used to enforce a commit in kafka producer if `queue.buffering.max.messages` is not specified.
/// This default value is determined based on the librdkafka default. See the following doc for more details:
/// <https://github.com/confluentinc/librdkafka/blob/1cb80090dfc75f5a36eae3f4f8844b14885c045e/CONFIGURATION.md>
const KAFKA_WRITER_MAX_QUEUE_SIZE: usize = 100000;

struct KafkaPayloadWriter {
    inner: FutureProducer<PrivateLinkProducerContext>,
    future_delivery_buffer: VecDeque<DeliveryFuture>,
    config: KafkaConfig,
}

pub struct KafkaSinkWriter {
    pub config: KafkaConfig,
    payload_writer: KafkaPayloadWriter,
    formatter: SinkFormatterImpl,
}

impl KafkaSinkWriter {
    pub async fn new(config: KafkaConfig, formatter: SinkFormatterImpl) -> Result<Self> {
        let inner: FutureProducer<PrivateLinkProducerContext> = {
            let mut c = ClientConfig::new();

            // KafkaConfig configuration
            config.common.set_security_properties(&mut c);
            config.set_client(&mut c);

            // ClientConfig configuration
            c.set("bootstrap.servers", &config.common.brokers);

            // Create the producer context, will be used to create the producer
            let producer_ctx = PrivateLinkProducerContext::new(
                config.common.broker_rewrite_map.clone(),
                // fixme: enable kafka native metrics for sink
                None,
                None,
            )?;

            // Generate the producer
            c.create_with_context(producer_ctx).await?
        };

        Ok(KafkaSinkWriter {
            config: config.clone(),
            payload_writer: KafkaPayloadWriter {
                inner,
                future_delivery_buffer: VecDeque::new(),
                config,
            },
            formatter,
        })
    }
}

impl KafkaPayloadWriter {
    /// The actual `send_result` function, will be called when the `KafkaSinkWriter` needs to sink
    /// messages
    async fn send_result<'a, K, P>(
        &'a mut self,
        mut record: FutureRecord<'a, K, P>,
    ) -> KafkaResult<()>
    where
        K: ToBytes + ?Sized,
        P: ToBytes + ?Sized,
    {
        let mut success_flag = false;

        let mut ret = Ok(());

        let max_delivery_buffer_size = (self
            .config
            .rdkafka_properties
            .queue_buffering_max_messages
            .as_ref()
            .cloned()
            .unwrap_or(KAFKA_WRITER_MAX_QUEUE_SIZE) as f32
            * KAFKA_WRITER_MAX_QUEUE_SIZE_RATIO) as usize;

        for i in 0..self.config.max_retry_num {
            match self.inner.send_result(record) {
                Ok(delivery_future) => {
                    // First check if the current length is
                    // greater than the preset limit
                    while self.future_delivery_buffer.len() >= max_delivery_buffer_size {
                        tracing::warn!(
                            "Number of records being delivered ({}) >= expected kafka producer queue size ({}).
                            This indicates the default value of queue.buffering.max.messages has changed.",
                            self.future_delivery_buffer.len(),
                            max_delivery_buffer_size
                        );
                        Self::map_future_result(
                            self.future_delivery_buffer
                                .pop_front()
                                .expect("Expect the future not to be None")
                                .await,
                        )?;
                    }

                    self.future_delivery_buffer.push_back(delivery_future);
                    success_flag = true;
                    break;
                }
                // The enqueue buffer is full, `send_result` will immediately return
                // We can retry for another round after sleeping for sometime
                Err((e, rec)) => {
                    tracing::warn!(
                        "producing message (key {:?}) to topic {} failed, err {:?}.",
                        rec.key.map(|k| k.to_bytes()),
                        rec.topic,
                        e
                    );
                    record = rec;
                    match e {
                        KafkaError::MessageProduction(RDKafkaErrorCode::QueueFull) => {
                            tracing::warn!(
                                "Producer queue full. Delivery future buffer size={}. Await and retry #{}",
                                self.future_delivery_buffer.len(),
                                i
                            );
                            Self::map_future_result(
                                self.future_delivery_buffer
                                    .pop_front()
                                    .expect("Expect the future not to be None")
                                    .await,
                            )?;
                            continue;
                        }
                        _ => return Err(e),
                    }
                }
            }
        }

        if !success_flag {
            // In this case, after trying `max_retry_num`
            // The enqueue buffer is still full
            ret = Err(KafkaError::MessageProduction(RDKafkaErrorCode::QueueFull));
        }

        ret
    }

    async fn write_inner(
        &mut self,
        event_key_object: Option<Vec<u8>>,
        event_object: Option<Vec<u8>>,
    ) -> Result<()> {
        let topic = self.config.common.topic.clone();
        // here we assume the key part always exists and value part is optional.
        // if value is None, we will skip the payload part.
        let key_str = event_key_object.unwrap();
        let mut record = FutureRecord::<[u8], [u8]>::to(topic.as_str()).key(&key_str);
        let payload;
        if let Some(value) = event_object {
            payload = value;
            record = record.payload(&payload);
        }
        // Send the data but not wait it to finish sinking
        // Will join all `DeliveryFuture` during commit
        self.send_result(record).await?;
        Ok(())
    }

    fn map_future_result(
        delivery_future_result: <DeliveryFuture as Future>::Output,
    ) -> KafkaResult<()> {
        match delivery_future_result {
            // Successfully sent the record
            // Will return the partition and offset of the message (i32, i64)
            // Note that `Vec<()>` won't cause memory allocation
            Ok(Ok(_)) => Ok(()),
            // If the message failed to be delivered. (i.e., flush)
            // The error & the copy of the original message will be returned
            // i.e., (KafkaError, OwnedMessage)
            // We will just stop the loop, and return the error
            // The sink executor will back to the latest checkpoint
            Ok(Err((k_err, _msg))) => Err(k_err),
            // This represents the producer is dropped
            // before the delivery status is received
            // Return `KafkaError::Canceled`
            Err(_) => Err(KafkaError::Canceled),
        }
    }

    async fn commit_inner(&mut self) -> Result<()> {
        let _v = try_join_all(
            self.future_delivery_buffer
                .drain(..)
                .map(|delivery_future| {
                    delivery_future.map(|delivery_future_result| {
                        Self::map_future_result(delivery_future_result).map_err(SinkError::Kafka)
                    })
                }),
        )
        .await?;

        // Sanity check
        debug_assert!(
            self.future_delivery_buffer.is_empty(),
            "The buffer after `commit_inner` must be empty"
        );

        Ok(())
    }
}

impl FormattedSink for KafkaPayloadWriter {
    type K = Vec<u8>;
    type V = Vec<u8>;

    async fn write_one(&mut self, k: Option<Self::K>, v: Option<Self::V>) -> Result<()> {
        self.write_inner(k, v).await
    }
}

#[async_trait::async_trait]
impl SinkWriterV1 for KafkaSinkWriter {
    async fn write_batch(&mut self, chunk: StreamChunk) -> Result<()> {
        dispatch_sink_formatter_impl!(&self.formatter, formatter, {
            self.payload_writer.write_chunk(chunk, formatter).await
        })
    }

    /// ---------------------------------------------------------------------------------------
    /// Note: The following functions are just to satisfy `SinkWriterV1` trait                |
    /// We do not need transaction-related functionality for sink executor, return Ok(())     |
    /// ---------------------------------------------------------------------------------------
    // Note that epoch 0 is reserved for initializing, so we should not use epoch 0 for
    // transaction.
    async fn begin_epoch(&mut self, _epoch: u64) -> Result<()> {
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        // Group delivery (await the `FutureRecord`) here
        self.payload_writer.commit_inner().await?;
        Ok(())
    }

    async fn abort(&mut self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use maplit::hashmap;
    use risingwave_common::catalog::Field;
    use risingwave_common::types::DataType;

    use super::*;
    use crate::sink::encoder::{JsonEncoder, TimestampHandlingMode};
    use crate::sink::formatter::AppendOnlyFormatter;

    #[test]
    fn parse_rdkafka_props() {
        let props: HashMap<String, String> = hashmap! {
            // basic
            "connector".to_string() => "kafka".to_string(),
            "properties.bootstrap.server".to_string() => "localhost:9092".to_string(),
            "topic".to_string() => "test".to_string(),
            "type".to_string() => "append-only".to_string(),
            // RdKafkaPropertiesCommon
            "properties.message.max.bytes".to_string() => "12345".to_string(),
            "properties.receive.message.max.bytes".to_string() => "54321".to_string(),
            // RdKafkaPropertiesProducer
            "properties.queue.buffering.max.messages".to_string() => "114514".to_string(),
            "properties.queue.buffering.max.kbytes".to_string() => "114514".to_string(),
            "properties.queue.buffering.max.ms".to_string() => "114.514".to_string(),
            "properties.enable.idempotence".to_string() => "false".to_string(),
            "properties.message.send.max.retries".to_string() => "114514".to_string(),
            "properties.retry.backoff.ms".to_string() => "114514".to_string(),
            "properties.batch.num.messages".to_string() => "114514".to_string(),
            "properties.batch.size".to_string() => "114514".to_string(),
            "properties.compression.codec".to_string() => "zstd".to_string(),
            "properties.message.timeout.ms".to_string() => "114514".to_string(),
            "properties.max.in.flight.requests.per.connection".to_string() => "114514".to_string(),
        };
        let c = KafkaConfig::from_hashmap(props).unwrap();
        assert_eq!(
            c.rdkafka_properties.queue_buffering_max_ms,
            Some(114.514f64)
        );
        assert_eq!(
            c.rdkafka_properties.compression_codec,
            Some(CompressionCodec::Zstd)
        );
        assert_eq!(c.rdkafka_properties.message_timeout_ms, 114514);
        assert_eq!(
            c.rdkafka_properties.max_in_flight_requests_per_connection,
            114514
        );

        let props: HashMap<String, String> = hashmap! {
            // basic
            "connector".to_string() => "kafka".to_string(),
            "properties.bootstrap.server".to_string() => "localhost:9092".to_string(),
            "topic".to_string() => "test".to_string(),
            "type".to_string() => "append-only".to_string(),

            "properties.enable.idempotence".to_string() => "True".to_string(), // can only be 'true' or 'false'
        };
        assert!(KafkaConfig::from_hashmap(props).is_err());

        let props: HashMap<String, String> = hashmap! {
            // basic
            "connector".to_string() => "kafka".to_string(),
            "properties.bootstrap.server".to_string() => "localhost:9092".to_string(),
            "topic".to_string() => "test".to_string(),
            "type".to_string() => "append-only".to_string(),
            "properties.queue.buffering.max.kbytes".to_string() => "-114514".to_string(), // usize cannot be negative
        };
        assert!(KafkaConfig::from_hashmap(props).is_err());

        let props: HashMap<String, String> = hashmap! {
            // basic
            "connector".to_string() => "kafka".to_string(),
            "properties.bootstrap.server".to_string() => "localhost:9092".to_string(),
            "topic".to_string() => "test".to_string(),
            "type".to_string() => "append-only".to_string(),
            "properties.compression.codec".to_string() => "notvalid".to_string(), // has to be a valid CompressionCodec
        };
        assert!(KafkaConfig::from_hashmap(props).is_err());
    }

    #[test]
    fn parse_kafka_config() {
        let properties: HashMap<String, String> = hashmap! {
            "connector".to_string() => "kafka".to_string(),
            "properties.bootstrap.server".to_string() => "localhost:9092".to_string(),
            "topic".to_string() => "test".to_string(),
            "type".to_string() => "append-only".to_string(),
            "force_append_only".to_string() => "true".to_string(),
            "properties.security.protocol".to_string() => "SASL".to_string(),
            "properties.sasl.mechanism".to_string() => "SASL".to_string(),
            "properties.sasl.username".to_string() => "test".to_string(),
            "properties.sasl.password".to_string() => "test".to_string(),
            "properties.retry.max".to_string() => "20".to_string(),
            "properties.retry.interval".to_string() => "500ms".to_string(),
        };
        let config = KafkaConfig::from_hashmap(properties).unwrap();
        assert_eq!(config.common.brokers, "localhost:9092");
        assert_eq!(config.common.topic, "test");
        assert_eq!(config.r#type, "append-only");
        assert!(config.force_append_only);
        assert_eq!(config.max_retry_num, 20);
        assert_eq!(config.retry_interval, Duration::from_millis(500));

        // Optional fields eliminated.
        let properties: HashMap<String, String> = hashmap! {
            "connector".to_string() => "kafka".to_string(),
            "properties.bootstrap.server".to_string() => "localhost:9092".to_string(),
            "topic".to_string() => "test".to_string(),
            "type".to_string() => "upsert".to_string(),
        };
        let config = KafkaConfig::from_hashmap(properties).unwrap();
        assert!(!config.force_append_only);
        assert_eq!(config.max_retry_num, 3);
        assert_eq!(config.retry_interval, Duration::from_millis(100));

        // Invalid u32 input.
        let properties: HashMap<String, String> = hashmap! {
            "connector".to_string() => "kafka".to_string(),
            "properties.bootstrap.server".to_string() => "localhost:9092".to_string(),
            "topic".to_string() => "test".to_string(),
            "type".to_string() => "upsert".to_string(),
            "properties.retry.max".to_string() => "-20".to_string(),  // error!
        };
        assert!(KafkaConfig::from_hashmap(properties).is_err());

        // Invalid bool input.
        let properties: HashMap<String, String> = hashmap! {
            "connector".to_string() => "kafka".to_string(),
            "properties.bootstrap.server".to_string() => "localhost:9092".to_string(),
            "topic".to_string() => "test".to_string(),
            "type".to_string() => "upsert".to_string(),
            "force_append_only".to_string() => "yes".to_string(),  // error!
        };
        assert!(KafkaConfig::from_hashmap(properties).is_err());

        // Invalid duration input.
        let properties: HashMap<String, String> = hashmap! {
            "connector".to_string() => "kafka".to_string(),
            "properties.bootstrap.server".to_string() => "localhost:9092".to_string(),
            "topic".to_string() => "test".to_string(),
            "type".to_string() => "upsert".to_string(),
            "properties.retry.interval".to_string() => "500minutes".to_string(),  // error!
        };
        assert!(KafkaConfig::from_hashmap(properties).is_err());
    }

    /// Note: Please enable the kafka by running `./risedev configure` before commenting #[ignore]
    /// to run the test, also remember to modify `risedev.yml`
    #[ignore]
    #[tokio::test]
    async fn test_kafka_producer() -> Result<()> {
        // Create a dummy kafka properties
        let properties = hashmap! {
            "connector".to_string() => "kafka".to_string(),
            "properties.bootstrap.server".to_string() => "localhost:29092".to_string(),
            "type".to_string() => "append-only".to_string(),
            "topic".to_string() => "test_topic".to_string(),
            "properties.compression.codec".to_string() => "zstd".to_string(),
        };

        // Create a table with two columns (| id : INT32 | v2 : VARCHAR |) here
        let schema = Schema::new(vec![
            Field {
                data_type: DataType::Int32,
                name: "id".into(),
                sub_fields: vec![],
                type_name: "".into(),
            },
            Field {
                data_type: DataType::Varchar,
                name: "v2".into(),
                sub_fields: vec![],
                type_name: "".into(),
            },
        ]);

        // We do not specify primary key for this schema
        let pk_indices = vec![];
        let kafka_config = KafkaConfig::from_hashmap(properties)?;

        // Create the actual sink writer to Kafka
        let mut sink = KafkaSinkWriter::new(
            kafka_config.clone(),
            SinkFormatterImpl::AppendOnlyJson(AppendOnlyFormatter::new(
                JsonEncoder::new(
                    schema.clone(),
                    Some(pk_indices),
                    TimestampHandlingMode::Milli,
                ),
                JsonEncoder::new(schema, None, TimestampHandlingMode::Milli),
            )),
        )
        .await
        .unwrap();

        for i in 0..10 {
            let mut fail_flag = false;
            sink.begin_epoch(i).await?;
            println!("epoch: {}", i);
            for j in 0..100 {
                match sink
                    .payload_writer
                    .send_result(
                        FutureRecord::to(kafka_config.common.topic.as_str())
                            .payload(format!("value-{}", j).as_bytes())
                            .key(format!("dummy_key_for_epoch-{}", i).as_bytes()),
                    )
                    .await
                {
                    Ok(_) => {}
                    Err(e) => {
                        fail_flag = true;
                        println!("{:?}", e);
                        sink.abort().await?;
                    }
                };
            }
            if !fail_flag {
                sink.commit().await?;
                println!("commit success");
            }
        }

        Ok(())
    }
}
