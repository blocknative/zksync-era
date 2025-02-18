use anyhow::Context as _;
use zksync_dal::{eth_watcher_dal::EventType, Connection, Core, CoreDal};
use zksync_types::{api::Log, ethabi::Contract, L1BlockNumber, L2ChainId, H256, U256};

use crate::event_processors::{EventProcessor, EventProcessorError, EventsSource};

#[derive(Debug, Clone)]
struct ServerNotification {
    block_number: L1BlockNumber,
}

#[derive(Debug)]
pub struct GatewayMigration {
    gateway_migration_topic: H256,
    l2chain_id: L2ChainId,
}

impl GatewayMigration {
    pub fn new(server_notifier: &Contract, l2chain_id: L2ChainId) -> Self {
        Self {
            gateway_migration_topic: server_notifier
                .event("MigrateToGateway")
                .unwrap()
                .signature(),
            l2chain_id,
        }
    }
}

#[async_trait::async_trait]
impl EventProcessor for GatewayMigration {
    async fn process_events(
        &mut self,
        storage: &mut Connection<'_, Core>,
        events: Vec<Log>,
    ) -> Result<usize, EventProcessorError> {
        for event in &events {
            let chain_id = U256::from_big_endian(
                event
                    .topics
                    .get(1)
                    .copied()
                    .context("missing topic 1")?
                    .as_bytes(),
            );

            if L2ChainId::from(chain_id.as_u32()) != self.l2chain_id {
                continue;
            }

            storage
                .server_notifications_dal()
                .save_notification(
                    *event.topics.first().unwrap(),
                    L1BlockNumber(event.block_number.unwrap().as_u32()),
                    Default::default(),
                )
                .await
                .unwrap();
        }
        Ok(events.len())
    }

    fn topic1(&self) -> H256 {
        self.gateway_migration_topic
    }

    fn event_source(&self) -> EventsSource {
        EventsSource::L1
    }

    fn event_type(&self) -> EventType {
        EventType::ServerNotification
    }
}
