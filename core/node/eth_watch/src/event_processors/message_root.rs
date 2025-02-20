use zksync_dal::{eth_watcher_dal::EventType, Connection, Core, CoreDal, DalError};
use zksync_types::{api::Log, ethabi, L1BatchNumber, SLChainId, H256};

use crate::event_processors::{EventProcessor, EventProcessorError, EventsSource};

/// Responsible for `AppendedChainBatchRoot` events and saving `BatchAndChainMerklePath` for batches.
#[derive(Debug)]
pub struct MessageRootProcessor {
    appended_message_root_signature: H256,
    event_source: EventsSource,
    pub dependency_chain_number: Option<usize>,
}

impl MessageRootProcessor {
    pub fn new(event_source: EventsSource, dependency_chain_number: Option<usize>) -> Self {
        Self {
            appended_message_root_signature: ethabi::long_signature(
                "NewMessageRoot",
                &[
                    ethabi::ParamType::Uint(256),
                    ethabi::ParamType::Uint(256),
                    ethabi::ParamType::Uint(256),
                    ethabi::ParamType::Array(Box::new(ethabi::ParamType::FixedBytes(32))),
                ],
            ),
            event_source: event_source,
            dependency_chain_number: dependency_chain_number,
        }
    }
}

#[async_trait::async_trait]
impl EventProcessor for MessageRootProcessor {
    async fn process_events(
        &mut self,
        storage: &mut Connection<'_, Core>,
        events: Vec<Log>,
    ) -> Result<usize, EventProcessorError> {
        let events_count = events.len();
        let mut transaction = storage
            .start_transaction()
            .await
            .map_err(DalError::generalize)?;

        for event in events {
            println!("source {:?}", self.event_source);
            println!("event in global {:?}", event);
            // let root = event.topics[3];
            let mut tokens = ethabi::decode(
                &[ethabi::ParamType::Array(Box::new(
                    ethabi::ParamType::FixedBytes(32),
                ))],
                event.data.0.as_slice(),
            )
            .expect("Failed to decode BytecodeL1PublicationRequested message");
            println!("tokens in global {:?}", tokens);
            let token = tokens.remove(0);
            // println!("formatted 1 {:?}", token);
            // println!("formatted 1.5 {:?}", token.type_check(&ethabi::ParamType::Array(Box::new(ethabi::ParamType::FixedBytes(32)))));
            // println!("formatted 2 {:?}", token.clone().into_array());
            // println!("formatted 3 {:?}", (token.clone().into_array().unwrap())[0].clone().into_fixed_bytes());
            // println!("formatted 4 {:?}", H256::from_slice(&token.clone().into_array().unwrap()[0].clone().into_fixed_bytes().unwrap()));

            // .iter().map(|t| format!("{:02x}", t)).collect::<String>());

            let mut root: Vec<H256> = vec![];
            if self.event_source == EventsSource::Dependency {
                root.push(H256::zero());
            }
            root = [
                root,
                token
                    .into_array()
                    .unwrap()
                    .into_iter()
                    .map(|t| H256::from_slice(&t.clone().into_fixed_bytes().unwrap()))
                    .collect::<Vec<_>>(),
            ]
            .concat();
            println!("root in global {:?}", root);
            assert_eq!(event.topics[0], self.appended_message_root_signature); // guaranteed by the watcher
                                                                               // tracing::info!(%root, "Saving global message root");
                                                                               // let block_number = event.block_number; // kl todo
                                                                               // let block_number = block_number.unwrap().0[0] as u64;
            let block_bytes: [u8; 8] = event.topics[2].as_bytes()[24..32].try_into().unwrap();
            let chain_id_bytes: [u8; 8] = event.topics[1].as_bytes()[24..32].try_into().unwrap();
            let block_number: u64 = u64::from_be_bytes(block_bytes);
            let chain_id = u64::from_be_bytes(chain_id_bytes);
            println!("block_number in global {:?}", block_number);
            println!("chain_id in global {:?}", chain_id);
            transaction
                .message_root_dal()
                .set_message_root(
                    SLChainId(chain_id),
                    L1BatchNumber(block_number as u32),
                    &root,
                )
                .await
                .map_err(DalError::generalize)?;
        }

        transaction.commit().await.map_err(DalError::generalize)?;

        Ok(events_count)
    }

    fn topic1(&self) -> H256 {
        // println!("appended_message_root_signature: {:?}", self.appended_message_root_signature);

        self.appended_message_root_signature
    }

    // fn topic2(&self) -> Option<H256> {
    //     Some(H256::from_low_u64_be(self.l2_chain_id.0))
    // }

    fn event_source(&self) -> EventsSource {
        self.event_source.clone()
    }

    fn event_type(&self) -> EventType {
        EventType::ChainBatchRoot
    }

    fn only_finalized_block(&self) -> bool {
        true
    }

    fn dependency_chain_number(&self) -> Option<usize> {
        self.dependency_chain_number
    }
}

impl MessageRootProcessor {}
