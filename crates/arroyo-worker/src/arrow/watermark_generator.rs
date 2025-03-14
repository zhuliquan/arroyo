use arrow::compute::kernels;
use arrow_array::RecordBatch;
use arroyo_operator::context::{Collector, OperatorContext};
use arroyo_operator::get_timestamp_col;
use arroyo_operator::operator::{
    ArrowOperator, AsDisplayable, ConstructedOperator, DisplayableOperator, OperatorConstructor,
    Registry,
};
use arroyo_rpc::df::ArroyoSchema;
use arroyo_rpc::grpc::api::ExpressionWatermarkConfig;
use arroyo_rpc::grpc::rpc::TableConfig;
use arroyo_state::global_table_config;
use arroyo_types::{from_nanos, to_millis, CheckpointBarrier, SignalMessage, Watermark};
use async_trait::async_trait;
use bincode::{Decode, Encode};
use datafusion::physical_expr::PhysicalExpr;
use datafusion_proto::physical_plan::from_proto::parse_physical_expr;
use datafusion_proto::physical_plan::DefaultPhysicalExtensionCodec;
use datafusion_proto::protobuf::PhysicalExprNode;
use prost::Message;
use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tracing::{debug, info};

#[derive(Encode, Decode, Copy, Clone, Debug, PartialEq)]
pub struct WatermarkGeneratorState {
    last_watermark_emitted_at: SystemTime,
    max_watermark: SystemTime,
}

pub struct WatermarkGenerator {
    interval: Duration,
    state_cache: WatermarkGeneratorState,
    idle_time: Option<Duration>,
    last_event: SystemTime,
    idle: bool,
    expression: Arc<dyn PhysicalExpr>,
}

impl WatermarkGenerator {
    pub fn expression(
        interval: Duration,
        idle_time: Option<Duration>,
        expression: Arc<dyn PhysicalExpr>,
    ) -> WatermarkGenerator {
        WatermarkGenerator {
            interval,
            state_cache: WatermarkGeneratorState {
                last_watermark_emitted_at: SystemTime::UNIX_EPOCH,
                max_watermark: SystemTime::UNIX_EPOCH,
            },
            idle_time,
            last_event: SystemTime::now(),
            idle: false,
            expression,
        }
    }
}

pub struct WatermarkGeneratorConstructor;

impl OperatorConstructor for WatermarkGeneratorConstructor {
    type ConfigT = ExpressionWatermarkConfig;
    fn with_config(
        &self,
        config: Self::ConfigT,
        registry: Arc<Registry>,
    ) -> anyhow::Result<ConstructedOperator> {
        let input_schema: ArroyoSchema = config.input_schema.unwrap().try_into()?;
        let expression = PhysicalExprNode::decode(&mut config.expression.as_slice())?;
        let expression = parse_physical_expr(
            &expression,
            registry.as_ref(),
            &input_schema.schema,
            &DefaultPhysicalExtensionCodec {},
        )?;

        Ok(ConstructedOperator::from_operator(Box::new(
            WatermarkGenerator::expression(
                Duration::from_micros(config.period_micros),
                config.idle_time_micros.map(Duration::from_micros),
                expression,
            ),
        )))
    }
}

#[async_trait]
impl ArrowOperator for WatermarkGenerator {
    fn tables(&self) -> HashMap<String, TableConfig> {
        global_table_config("s", "expression watermark state")
    }

    fn name(&self) -> String {
        "expression_watermark_generator".to_string()
    }

    fn display(&self) -> DisplayableOperator {
        DisplayableOperator {
            name: Cow::Borrowed("WatermarkGenerator"),
            fields: vec![
                ("interval", AsDisplayable::Debug(&self.interval)),
                ("idle_time", AsDisplayable::Debug(&self.idle_time)),
                ("expression", AsDisplayable::Debug(&self.expression)),
            ],
        }
    }

    fn tick_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(1))
    }

    async fn on_start(&mut self, ctx: &mut OperatorContext) {
        let gs = ctx
            .table_manager
            .get_global_keyed_state("s")
            .await
            .expect("should have watermark table.");
        self.last_event = SystemTime::now();

        let state = *(gs
            .get(&ctx.task_info.task_index)
            .unwrap_or(&WatermarkGeneratorState {
                last_watermark_emitted_at: SystemTime::UNIX_EPOCH,
                max_watermark: SystemTime::UNIX_EPOCH,
            }));

        self.state_cache = state;
    }

    async fn on_close(
        &mut self,
        final_message: &Option<SignalMessage>,
        _: &mut OperatorContext,
        collector: &mut dyn Collector,
    ) {
        if let Some(SignalMessage::EndOfData) = final_message {
            // send final watermark on close
            collector
                .broadcast_watermark(
                    // this is in the year 2554, far enough out be close to infinity,
                    // but can still be formatted.
                    Watermark::EventTime(from_nanos(u64::MAX as u128)),
                )
                .await;
        }
    }

    async fn process_batch(
        &mut self,
        record: RecordBatch,
        ctx: &mut OperatorContext,
        collector: &mut dyn Collector,
    ) {
        collector.collect(record.clone()).await;
        self.last_event = SystemTime::now();

        let timestamp_column = get_timestamp_col(&record, ctx);
        let Some(max_timestamp) = kernels::aggregate::max(timestamp_column) else {
            return;
        };
        let max_timestamp = from_nanos(max_timestamp as u128);

        // calculate watermark using expression
        let watermark = self
            .expression
            .evaluate(&record)
            .unwrap()
            .into_array(record.num_rows())
            .unwrap();

        let watermark = watermark
            .as_any()
            .downcast_ref::<arrow::array::TimestampNanosecondArray>()
            .unwrap();

        let watermark = from_nanos(kernels::aggregate::min(watermark).unwrap() as u128);

        self.state_cache.max_watermark = self.state_cache.max_watermark.max(watermark);
        if self.idle
            || max_timestamp
                .duration_since(self.state_cache.last_watermark_emitted_at)
                .unwrap_or(Duration::ZERO)
                > self.interval
        {
            debug!(
                "[{}] Emitting expression watermark {}",
                ctx.task_info.task_index,
                to_millis(watermark)
            );
            collector
                .broadcast_watermark(Watermark::EventTime(watermark))
                .await;
            self.state_cache.last_watermark_emitted_at = max_timestamp;
            self.idle = false;
        }
    }

    async fn handle_checkpoint(
        &mut self,
        _: CheckpointBarrier,
        ctx: &mut OperatorContext,
        _: &mut dyn Collector,
    ) {
        let gs = ctx
            .table_manager
            .get_global_keyed_state("s")
            .await
            .expect("state");

        gs.insert(ctx.task_info.task_index, self.state_cache).await;
    }

    async fn handle_tick(
        &mut self,
        _: u64,
        ctx: &mut OperatorContext,
        collector: &mut dyn Collector,
    ) {
        if let Some(idle_time) = self.idle_time {
            if self.last_event.elapsed().unwrap_or(Duration::ZERO) > idle_time && !self.idle {
                info!(
                    "Setting partition {} to idle after {:?}",
                    ctx.task_info.task_index, idle_time
                );
                collector.broadcast_watermark(Watermark::Idle).await;
                self.idle = true;
            }
        }
    }
}
