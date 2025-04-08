// Copyright 2025 RisingWave Labs
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

use std::collections::HashMap;
use std::fmt::Debug;
use std::sync::LazyLock;

use auto_enums::auto_enum;
pub use avro::AvroParserConfig;
pub use canal::*;
pub use chunk_builder::{SourceStreamChunkBuilder, SourceStreamChunkRowWriter};
use csv_parser::CsvParser;
pub use debezium::*;
use futures::{Future, TryFutureExt};
use futures_async_stream::try_stream;
pub use json_parser::*;
pub use parquet_parser::ParquetParser;
pub use protobuf::*;
use risingwave_common::array::StreamChunk;
use risingwave_common::catalog::{CDC_TABLE_NAME_COLUMN_NAME, KAFKA_TIMESTAMP_COLUMN_NAME};
use risingwave_common::log::LogSuppresser;
use risingwave_common::metrics::GLOBAL_ERROR_METRICS;
use risingwave_common::types::{DatumCow, DatumRef};
use risingwave_common::util::tracing::InstrumentStream;
use risingwave_connector_codec::decoder::avro::MapHandling;
use thiserror_ext::AsReport;

pub use self::mysql::{mysql_datum_to_rw_datum, mysql_row_to_owned_row};
use self::plain_parser::PlainParser;
pub use self::postgres::postgres_row_to_owned_row;
pub use self::sql_server::{ScalarImplTiberiusWrapper, sql_server_row_to_owned_row};
pub use self::unified::Access;
pub use self::unified::json::{JsonAccess, TimestamptzHandling};
use self::upsert_parser::UpsertParser;
use crate::error::ConnectorResult;
use crate::parser::maxwell::MaxwellParser;
use crate::schema::schema_registry::SchemaRegistryConfig;
use crate::source::monitor::GLOBAL_SOURCE_METRICS;
use crate::source::{
    BoxSourceMessageStream, SourceChunkStream, SourceColumnDesc, SourceColumnType, SourceContext,
    SourceContextRef, SourceCtrlOpts, SourceMeta,
};

mod access_builder;
pub mod additional_columns;
mod avro;
mod bytes_parser;
mod canal;
mod chunk_builder;
mod config;
mod csv_parser;
mod debezium;
mod json_parser;
mod maxwell;
mod mysql;
pub mod parquet_parser;
pub mod plain_parser;
mod postgres;
mod protobuf;
pub mod scalar_adapter;
mod sql_server;
mod unified;
mod upsert_parser;
mod utils;

use access_builder::{AccessBuilder, AccessBuilderImpl};
pub use config::*;
pub use debezium::DEBEZIUM_IGNORE_KEY;
use debezium::schema_change::SchemaChangeEnvelope;
pub use unified::{AccessError, AccessResult};

/// The meta data of the original message for a row writer.
///
/// Extracted from the `SourceMessage`.
#[derive(Clone, Copy, Debug)]
pub struct MessageMeta<'a> {
    source_meta: &'a SourceMeta,
    split_id: &'a str,
    offset: &'a str,
}

impl<'a> MessageMeta<'a> {
    /// Extract the value for the given column.
    ///
    /// Returns `None` if the column is not a meta column.
    fn value_for_column(self, desc: &SourceColumnDesc) -> DatumRef<'a> {
        let datum: DatumRef<'_> = match desc.column_type {
            // Row id columns are filled with `NULL` here and will be filled with the real
            // row id generated by `RowIdGenExecutor` later.
            SourceColumnType::RowId => None,
            // Extract the offset from the meta data.
            SourceColumnType::Offset => Some(self.offset.into()),
            // Extract custom meta data per connector.
            SourceColumnType::Meta if let SourceMeta::Kafka(kafka_meta) = self.source_meta => {
                assert_eq!(
                    desc.name.as_str(),
                    KAFKA_TIMESTAMP_COLUMN_NAME,
                    "unexpected kafka meta column name"
                );
                kafka_meta.extract_timestamp()
            }
            SourceColumnType::Meta if let SourceMeta::DebeziumCdc(cdc_meta) = self.source_meta => {
                assert_eq!(
                    desc.name.as_str(),
                    CDC_TABLE_NAME_COLUMN_NAME,
                    "unexpected cdc meta column name"
                );
                Some(cdc_meta.full_table_name.as_str().into())
            }

            // For other cases, return `None`.
            SourceColumnType::Meta | SourceColumnType::Normal => return None,
        };

        datum
    }
}

/// Transaction control message. Currently only used by Debezium messages.
#[derive(Debug)]
pub enum TransactionControl {
    Begin { id: Box<str> },
    Commit { id: Box<str> },
}

/// The result of parsing a message.
#[derive(Debug)]
pub enum ParseResult {
    /// Some rows are parsed and written to the [`SourceStreamChunkRowWriter`].
    Rows,
    /// A transaction control message is parsed.
    TransactionControl(TransactionControl),

    /// A schema change message is parsed.
    SchemaChange(SchemaChangeEnvelope),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParserFormat {
    CanalJson,
    Csv,
    Json,
    Maxwell,
    Debezium,
    DebeziumMongo,
    Upsert,
    Plain,
}

/// `ByteStreamSourceParser` is the entrypoint abstraction for parsing messages.
/// It consumes bytes of one individual message and produces parsed records.
///
/// It's used by [`ByteStreamSourceParserImpl::parse_stream`]. `pub` is for benchmark only.
pub trait ByteStreamSourceParser: Send + Debug + Sized + 'static {
    /// The column descriptors of the output chunk.
    fn columns(&self) -> &[SourceColumnDesc];

    /// The source context, used to report parsing error.
    fn source_ctx(&self) -> &SourceContext;

    /// The format of the specific parser.
    fn parser_format(&self) -> ParserFormat;

    /// Parse one record from the given `payload` and write rows to the `writer`.
    ///
    /// Returns error if **any** of the rows in the message failed to parse.
    fn parse_one<'a>(
        &'a mut self,
        key: Option<Vec<u8>>,
        payload: Option<Vec<u8>>,
        writer: SourceStreamChunkRowWriter<'a>,
    ) -> impl Future<Output = ConnectorResult<()>> + Send + 'a;

    /// Parse one record from the given `payload`, either write rows to the `writer` or interpret it
    /// as a transaction control message.
    ///
    /// The default implementation forwards to [`ByteStreamSourceParser::parse_one`] for
    /// non-transactional sources.
    ///
    /// Returns error if **any** of the rows in the message failed to parse.
    fn parse_one_with_txn<'a>(
        &'a mut self,
        key: Option<Vec<u8>>,
        payload: Option<Vec<u8>>,
        writer: SourceStreamChunkRowWriter<'a>,
    ) -> impl Future<Output = ConnectorResult<ParseResult>> + Send + 'a {
        self.parse_one(key, payload, writer)
            .map_ok(|_| ParseResult::Rows)
    }
}

#[easy_ext::ext(SourceParserIntoStreamExt)]
impl<P: ByteStreamSourceParser> P {
    /// Parse a `SourceMessage` stream into a [`StreamChunk`] stream.
    ///
    /// # Arguments
    ///
    /// - `msg_stream`: A stream of batches of `SourceMessage`.
    ///
    /// # Returns
    ///
    /// A [`SourceChunkStream`] of parsed chunks. Each of the parsed chunks are guaranteed
    /// to have less than or equal to `source_ctrl_opts.chunk_size` rows, unless there's a
    /// large transaction and `source_ctrl_opts.split_txn` is false.
    pub fn parse_stream(self, msg_stream: BoxSourceMessageStream) -> impl SourceChunkStream {
        let actor_id = self.source_ctx().actor_id;
        let source_id = self.source_ctx().source_id.table_id();

        // The stream will be long-lived. We use `instrument_with` here to create
        // a new span for the polling of each chunk.
        let source_ctrl_opts = self.source_ctx().source_ctrl_opts;
        parse_message_stream(self, msg_stream, source_ctrl_opts)
            .instrument_with(move || tracing::info_span!("source_parse_chunk", actor_id, source_id))
    }
}

// TODO: when upsert is disabled, how to filter those empty payload
// Currently, an err is returned for non upsert with empty payload
#[try_stream(ok = StreamChunk, error = crate::error::ConnectorError)]
async fn parse_message_stream<P: ByteStreamSourceParser>(
    mut parser: P,
    msg_stream: BoxSourceMessageStream,
    source_ctrl_opts: SourceCtrlOpts,
) {
    let mut chunk_builder =
        SourceStreamChunkBuilder::new(parser.columns().to_vec(), source_ctrl_opts);

    let mut direct_cdc_event_lag_latency_metrics = HashMap::new();

    #[for_await]
    for batch in msg_stream {
        // It's possible that the split is not active, which means the next batch may arrive
        // very lately, so we should prefer emitting all records in current batch before the end
        // of each iteration, instead of merging them with the next batch. An exception is when
        // a transaction is not committed yet, in which yield when the transaction is committed.

        let batch = batch?;
        let batch_len = batch.len();

        if batch_len == 0 {
            continue;
        }

        if batch.iter().all(|msg| msg.is_cdc_heartbeat()) {
            // This `.iter().all(...)` will short-circuit after seeing the first `false`, so in
            // normal cases, this should only involve a constant time cost.

            // Now we know that there is no data message in the batch, let's just emit the latest
            // heartbeat message. Note that all messages in `batch` should belong to the same
            // split, so we don't have to do a split to heartbeats mapping here.

            let heartbeat_msg = batch.last().unwrap();
            tracing::debug!(
                offset = heartbeat_msg.offset,
                "handling a heartbeat message"
            );
            chunk_builder.heartbeat(MessageMeta {
                source_meta: &heartbeat_msg.meta,
                split_id: &heartbeat_msg.split_id,
                offset: &heartbeat_msg.offset,
            });

            for chunk in chunk_builder.consume_ready_chunks() {
                yield chunk;
            }
            continue; // continue to next batch
        }

        // When we reach here, there is at least one data message in the batch. We should ignore all
        // heartbeat messages.

        let mut txn_started_in_last_batch = chunk_builder.is_in_transaction();
        let process_time_ms = chrono::Utc::now().timestamp_millis();

        for msg in batch {
            if msg.is_cdc_heartbeat() {
                // ignore heartbeat messages
                continue;
            }

            // calculate process_time - event_time lag
            if let SourceMeta::DebeziumCdc(msg_meta) = &msg.meta {
                let lag_ms = process_time_ms - msg_meta.source_ts_ms;
                // report to promethus
                let full_table_name = msg_meta.full_table_name.clone();
                let direct_cdc_event_lag_latency = direct_cdc_event_lag_latency_metrics
                    .entry(full_table_name)
                    .or_insert_with(|| {
                        GLOBAL_SOURCE_METRICS
                            .direct_cdc_event_lag_latency
                            .with_guarded_label_values(&[&msg_meta.full_table_name])
                    });
                direct_cdc_event_lag_latency.observe(lag_ms as f64);
            }

            // Parse the message and write to the chunk builder, it's possible that the message
            // contains multiple rows. When the chunk size reached the limit during parsing, the
            // chunk builder may yield the chunk to `ready_chunks` and start a new chunk.
            match parser
                .parse_one_with_txn(
                    msg.key,
                    msg.payload,
                    chunk_builder.row_writer().with_meta(MessageMeta {
                        source_meta: &msg.meta,
                        split_id: &msg.split_id,
                        offset: &msg.offset,
                    }),
                )
                .await
            {
                // It's possible that parsing multiple rows in a single message PARTIALLY failed.
                // We still have to maintain the row number in this case.
                res @ (Ok(ParseResult::Rows) | Err(_)) => {
                    if let Err(error) = res {
                        // TODO: not using tracing span to provide `split_id` and `offset` due to performance concern,
                        //       see #13105
                        static LOG_SUPPERSSER: LazyLock<LogSuppresser> =
                            LazyLock::new(LogSuppresser::default);
                        if let Ok(suppressed_count) = LOG_SUPPERSSER.check() {
                            tracing::error!(
                                error = %error.as_report(),
                                split_id = &*msg.split_id,
                                offset = msg.offset,
                                suppressed_count,
                                "failed to parse message, skipping"
                            );
                        }

                        // report to error metrics
                        let context = parser.source_ctx();
                        GLOBAL_ERROR_METRICS.user_source_error.report([
                            error.variant_name().to_owned(),
                            context.source_id.to_string(),
                            context.source_name.clone(),
                            context.fragment_id.to_string(),
                        ]);
                    }

                    for chunk in chunk_builder.consume_ready_chunks() {
                        yield chunk;
                    }
                }

                Ok(ParseResult::TransactionControl(txn_ctl)) => match txn_ctl {
                    TransactionControl::Begin { id } => {
                        chunk_builder.begin_transaction(id);
                    }
                    TransactionControl::Commit { id } => {
                        chunk_builder.commit_transaction(id);
                        assert!(!chunk_builder.is_in_transaction());

                        if txn_started_in_last_batch {
                            // If a transaction is across multiple batches, we yield the chunk
                            // immediately after the transaction is committed.
                            chunk_builder.finish_current_chunk();
                            txn_started_in_last_batch = false;
                        }

                        for chunk in chunk_builder.consume_ready_chunks() {
                            yield chunk;
                        }
                    }
                },

                Ok(ParseResult::SchemaChange(schema_change)) => {
                    if schema_change.is_empty() {
                        continue;
                    }

                    let (oneshot_tx, oneshot_rx) = tokio::sync::oneshot::channel();
                    // we bubble up the schema change event to the source executor via channel,
                    // and wait for the source executor to finish the schema change process before
                    // parsing the following messages.
                    if let Some(ref tx) = parser.source_ctx().schema_change_tx {
                        tx.send((schema_change, oneshot_tx))
                            .await
                            .expect("send schema change to executor");
                        match oneshot_rx.await {
                            Ok(()) => {}
                            Err(e) => {
                                tracing::error!(error = %e.as_report(), "failed to wait for schema change");
                            }
                        }
                    }
                }
            }
        }

        // Finish the remaining records in the batch.
        if !chunk_builder.is_in_transaction() {
            chunk_builder.finish_current_chunk();
        }
        for chunk in chunk_builder.consume_ready_chunks() {
            yield chunk;
        }
    }
}

#[derive(Debug)]
pub enum EncodingType {
    Key,
    Value,
}

/// The entrypoint of parsing. It parses `SourceMessage` stream (byte stream) into [`StreamChunk`] stream.
/// Used by [`crate::source::into_chunk_stream`].
#[derive(Debug)]
pub enum ByteStreamSourceParserImpl {
    Csv(CsvParser),
    Debezium(DebeziumParser),
    Plain(PlainParser),
    Upsert(UpsertParser),
    DebeziumMongoJson(DebeziumMongoJsonParser),
    Maxwell(MaxwellParser),
    CanalJson(CanalJsonParser),
}

impl ByteStreamSourceParserImpl {
    /// Converts `SourceMessage` vec stream into [`StreamChunk`] stream.
    pub fn parse_stream(
        self,
        msg_stream: BoxSourceMessageStream,
    ) -> impl SourceChunkStream + Unpin {
        #[auto_enum(futures03::Stream)]
        let stream = match self {
            Self::Csv(parser) => parser.parse_stream(msg_stream),
            Self::Debezium(parser) => parser.parse_stream(msg_stream),
            Self::DebeziumMongoJson(parser) => parser.parse_stream(msg_stream),
            Self::Maxwell(parser) => parser.parse_stream(msg_stream),
            Self::CanalJson(parser) => parser.parse_stream(msg_stream),
            Self::Plain(parser) => parser.parse_stream(msg_stream),
            Self::Upsert(parser) => parser.parse_stream(msg_stream),
        };
        Box::pin(stream)
    }
}

impl ByteStreamSourceParserImpl {
    pub async fn create(
        parser_config: ParserConfig,
        source_ctx: SourceContextRef,
    ) -> ConnectorResult<Self> {
        let CommonParserConfig { rw_columns } = parser_config.common;
        let protocol = &parser_config.specific.protocol_config;
        let encode = &parser_config.specific.encoding_config;
        match (protocol, encode) {
            (ProtocolProperties::Plain, EncodingProperties::Csv(config)) => {
                CsvParser::new(rw_columns, *config, source_ctx).map(Self::Csv)
            }
            (ProtocolProperties::DebeziumMongo, EncodingProperties::MongoJson(props)) => {
                DebeziumMongoJsonParser::new(rw_columns, source_ctx, props.clone())
                    .map(Self::DebeziumMongoJson)
            }
            (ProtocolProperties::Canal, EncodingProperties::Json(config)) => {
                CanalJsonParser::new(rw_columns, source_ctx, config).map(Self::CanalJson)
            }
            (ProtocolProperties::Native, _) => unreachable!("Native parser should not be created"),
            (ProtocolProperties::Upsert, _) => {
                let parser =
                    UpsertParser::new(parser_config.specific, rw_columns, source_ctx).await?;
                Ok(Self::Upsert(parser))
            }
            (ProtocolProperties::Plain, _) => {
                let parser =
                    PlainParser::new(parser_config.specific, rw_columns, source_ctx).await?;
                Ok(Self::Plain(parser))
            }
            (ProtocolProperties::Debezium(_), _) => {
                let parser =
                    DebeziumParser::new(parser_config.specific, rw_columns, source_ctx).await?;
                Ok(Self::Debezium(parser))
            }
            (ProtocolProperties::Maxwell, _) => {
                let parser =
                    MaxwellParser::new(parser_config.specific, rw_columns, source_ctx).await?;
                Ok(Self::Maxwell(parser))
            }
            _ => unreachable!(),
        }
    }

    /// Create a parser for testing purposes.
    pub fn create_for_test(parser_config: ParserConfig) -> ConnectorResult<Self> {
        futures::executor::block_on(Self::create(parser_config, SourceContext::dummy().into()))
    }
}

/// Test utilities for [`ByteStreamSourceParserImpl`].
#[cfg(test)]
pub mod test_utils {
    use futures::StreamExt;
    use itertools::Itertools;

    use super::*;
    use crate::source::SourceMessage;

    #[easy_ext::ext(ByteStreamSourceParserImplTestExt)]
    pub(crate) impl ByteStreamSourceParserImpl {
        /// Parse the given payloads into a [`StreamChunk`].
        async fn parse(self, payloads: Vec<Vec<u8>>) -> StreamChunk {
            let source_messages = payloads
                .into_iter()
                .map(|p| SourceMessage {
                    payload: (!p.is_empty()).then_some(p),
                    ..SourceMessage::dummy()
                })
                .collect_vec();

            self.parse_stream(futures::stream::once(async { Ok(source_messages) }).boxed())
                .next()
                .await
                .unwrap()
                .unwrap()
        }

        /// Parse the given key-value pairs into a [`StreamChunk`].
        async fn parse_upsert(self, kvs: Vec<(Vec<u8>, Vec<u8>)>) -> StreamChunk {
            let source_messages = kvs
                .into_iter()
                .map(|(k, v)| SourceMessage {
                    key: (!k.is_empty()).then_some(k),
                    payload: (!v.is_empty()).then_some(v),
                    ..SourceMessage::dummy()
                })
                .collect_vec();

            self.parse_stream(futures::stream::once(async { Ok(source_messages) }).boxed())
                .next()
                .await
                .unwrap()
                .unwrap()
        }
    }
}
