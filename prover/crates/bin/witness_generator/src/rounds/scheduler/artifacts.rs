use std::time::Instant;

use async_trait::async_trait;
use circuit_definitions::circuit_definitions::recursion_layer::ZkSyncRecursionLayerStorageType;
use zksync_object_store::ObjectStore;
use zksync_prover_dal::{ConnectionPool, Prover, ProverDal};
use zksync_prover_fri_types::{keys::FriCircuitKey, CircuitWrapper, FriProofWrapper};
use zksync_types::{
    basic_fri_types::AggregationRound, ChainAwareL1BatchNumber, L1BatchNumber, L2ChainId,
};

use crate::{
    artifacts::ArtifactsManager,
    rounds::scheduler::{Scheduler, SchedulerArtifacts},
};

#[async_trait]
impl ArtifactsManager for Scheduler {
    type InputMetadata = (L2ChainId, u32);
    type InputArtifacts = FriProofWrapper;
    type OutputArtifacts = SchedulerArtifacts;
    type BlobUrls = String;

    async fn get_artifacts(
        metadata: &Self::InputMetadata,
        object_store: &dyn ObjectStore,
    ) -> anyhow::Result<Self::InputArtifacts> {
        let artifacts = object_store.get(*metadata).await?;

        Ok(artifacts)
    }

    async fn save_to_bucket(
        job_id: u32,
        chain_id: L2ChainId,
        artifacts: Self::OutputArtifacts,
        object_store: &dyn ObjectStore,
        _shall_save_to_public_bucket: bool,
        _public_blob_store: Option<std::sync::Arc<dyn ObjectStore>>,
    ) -> String {
        let key = FriCircuitKey {
            chain_id,
            block_number: L1BatchNumber(job_id),
            circuit_id: 1,
            sequence_number: 0,
            depth: 0,
            aggregation_round: AggregationRound::Scheduler,
        };

        object_store
            .put(
                key,
                &CircuitWrapper::Recursive(artifacts.scheduler_circuit.clone()),
            )
            .await
            .unwrap()
    }

    async fn save_to_database(
        connection_pool: &ConnectionPool<Prover>,
        job_id: u32,
        chain_id: L2ChainId,
        started_at: Instant,
        blob_urls: String,
        _artifacts: Self::OutputArtifacts,
    ) -> anyhow::Result<()> {
        let mut prover_connection = connection_pool.connection().await?;
        let mut transaction = prover_connection.start_transaction().await?;

        let batch_number = ChainAwareL1BatchNumber::new(chain_id, L1BatchNumber(job_id));

        let protocol_version_id = transaction
            .fri_basic_witness_generator_dal()
            .protocol_version_for_l1_batch_and_chain(batch_number)
            .await;

        transaction
            .fri_prover_jobs_dal()
            .insert_prover_job(
                batch_number,
                ZkSyncRecursionLayerStorageType::SchedulerCircuit as u8,
                0,
                0,
                AggregationRound::Scheduler,
                &blob_urls,
                false,
                protocol_version_id,
            )
            .await;

        transaction
            .fri_scheduler_witness_generator_dal()
            .mark_scheduler_job_as_successful(batch_number, started_at.elapsed())
            .await;

        transaction.commit().await?;
        Ok(())
    }
}
