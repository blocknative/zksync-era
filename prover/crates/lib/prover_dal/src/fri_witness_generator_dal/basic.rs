use std::time::Duration;

use zksync_basic_types::{
    protocol_version::{ProtocolSemanticVersion, ProtocolVersionId, VersionPatch},
    prover_dal::{BasicWitnessGeneratorJobInfo, StuckJobs, WitnessJobStatus},
    ChainAwareL1BatchNumber, L2ChainId,
};
use zksync_db_connection::{
    connection::Connection,
    error::DalResult,
    instrument::InstrumentExt,
    utils::{duration_to_naive_time, pg_interval_from_duration},
};

use crate::{fri_witness_generator_dal::FriWitnessJobStatus, Prover};

#[derive(Debug)]
pub struct FriBasicWitnessGeneratorDal<'a, 'c> {
    pub(crate) storage: &'a mut Connection<'c, Prover>,
}

impl FriBasicWitnessGeneratorDal<'_, '_> {
    pub async fn save_witness_inputs(
        &mut self,
        batch_number: ChainAwareL1BatchNumber,
        witness_inputs_blob_url: &str,
        protocol_version: ProtocolSemanticVersion,
    ) -> DalResult<()> {
        sqlx::query!(
            r#"
            INSERT INTO
            witness_inputs_fri (
                l1_batch_number,
                chain_id,
                witness_inputs_blob_url,
                protocol_version,
                status,
                created_at,
                updated_at,
                protocol_version_patch
            )
            VALUES
            ($1, $2, $3, $4, 'queued', NOW(), NOW(), $5)
            ON CONFLICT (l1_batch_number, chain_id) DO NOTHING
            "#,
            batch_number.raw_batch_number() as i64,
            batch_number.raw_chain_id() as i32,
            witness_inputs_blob_url,
            protocol_version.minor as i32,
            protocol_version.patch.0 as i32,
        )
        .instrument("save_witness_inputs")
        .execute(self.storage)
        .await?;
        Ok(())
    }

    /// Gets the next job to be executed. Returns the batch number and its corresponding blobs.
    /// The blobs arrive from core via prover gateway, as pubdata, this method loads the blobs.
    pub async fn get_next_basic_circuit_witness_job(
        &mut self,
        protocol_version: ProtocolSemanticVersion,
        picked_by: &str,
    ) -> Option<ChainAwareL1BatchNumber> {
        sqlx::query!(
            r#"
            UPDATE witness_inputs_fri
            SET
                status = 'in_progress',
                attempts = attempts + 1,
                updated_at = NOW(),
                processing_started_at = NOW(),
                picked_by = $2
            WHERE
                l1_batch_number = (
                    SELECT
                        l1_batch_number
                    FROM
                        witness_inputs_fri
                    WHERE
                        status = 'queued'
                        AND protocol_version = $1
                        AND protocol_version_patch = $3
                    ORDER BY
                        priority DESC,
                        created_at ASC
                    LIMIT
                        1
                    FOR UPDATE
                    SKIP LOCKED
                )
            RETURNING
            witness_inputs_fri.chain_id,
            witness_inputs_fri.l1_batch_number
            "#,
            protocol_version.minor as i32,
            picked_by,
            protocol_version.patch.0 as i32,
        )
        .fetch_optional(self.storage.conn())
        .await
        .unwrap()
        .map(|row| {
            ChainAwareL1BatchNumber::from_raw(row.chain_id as u64, row.l1_batch_number as u32)
        })
    }

    pub async fn set_status_for_basic_witness_job(
        &mut self,
        status: FriWitnessJobStatus,
        batch_number: ChainAwareL1BatchNumber,
    ) {
        sqlx::query!(
            r#"
            UPDATE witness_inputs_fri
            SET
                status = $1,
                updated_at = NOW()
            WHERE
                l1_batch_number = $2
                AND chain_id = $3
                AND status != 'successful'
            "#,
            status.to_string(),
            batch_number.raw_batch_number() as i64,
            batch_number.raw_chain_id() as i32
        )
        .execute(self.storage.conn())
        .await
        .unwrap();
    }

    pub async fn mark_witness_job_as_successful(
        &mut self,
        batch_number: ChainAwareL1BatchNumber,
        time_taken: Duration,
    ) {
        sqlx::query!(
            r#"
            UPDATE witness_inputs_fri
            SET
                status = 'successful',
                updated_at = NOW(),
                time_taken = $1
            WHERE
                l1_batch_number = $2
                AND chain_id = $3
            "#,
            duration_to_naive_time(time_taken),
            batch_number.raw_batch_number() as i32,
            batch_number.raw_chain_id() as i32
        )
        .execute(self.storage.conn())
        .await
        .unwrap();
    }

    pub async fn requeue_stuck_basic_jobs(
        &mut self,
        processing_timeout: Duration,
        max_attempts: u32,
    ) -> Vec<StuckJobs> {
        let processing_timeout = pg_interval_from_duration(processing_timeout);
        sqlx::query!(
            r#"
            UPDATE witness_inputs_fri
            SET
                status = 'queued',
                updated_at = NOW(),
                processing_started_at = NOW(),
                priority = priority + 1
            WHERE
                (
                    status = 'in_progress'
                    AND processing_started_at <= NOW() - $1::INTERVAL
                    AND attempts < $2
                )
                OR (
                    status = 'failed'
                    AND attempts < $2
                )
            RETURNING
            l1_batch_number,
            chain_id,
            status,
            attempts,
            error,
            picked_by
            "#,
            &processing_timeout,
            max_attempts as i32,
        )
        .fetch_all(self.storage.conn())
        .await
        .unwrap()
        .into_iter()
        .map(|row| StuckJobs {
            id: row.l1_batch_number as u64,
            chain_id: L2ChainId::new(row.chain_id as u64).unwrap(),
            status: row.status,
            attempts: row.attempts as u64,
            circuit_id: None,
            error: row.error,
            picked_by: row.picked_by,
        })
        .collect()
    }

    pub async fn protocol_version_for_l1_batch_and_chain(
        &mut self,
        batch_number: ChainAwareL1BatchNumber,
    ) -> ProtocolSemanticVersion {
        let result = sqlx::query!(
            r#"
            SELECT
                protocol_version,
                protocol_version_patch
            FROM
                witness_inputs_fri
            WHERE
                l1_batch_number = $1
                AND chain_id = $2
            "#,
            batch_number.raw_batch_number() as i64,
            batch_number.raw_chain_id() as i32
        )
        .fetch_one(self.storage.conn())
        .await
        .unwrap();

        ProtocolSemanticVersion::new(
            ProtocolVersionId::try_from(result.protocol_version.unwrap() as u16).unwrap(),
            VersionPatch(result.protocol_version_patch as u32),
        )
    }

    pub async fn get_basic_witness_generator_job_for_batch(
        &mut self,
        batch_number: ChainAwareL1BatchNumber,
    ) -> Option<BasicWitnessGeneratorJobInfo> {
        sqlx::query!(
            r#"
            SELECT
                *
            FROM
                witness_inputs_fri
            WHERE
                l1_batch_number = $1
                AND chain_id = $2
            "#,
            i64::from(batch_number.raw_batch_number()),
            batch_number.raw_chain_id() as i32
        )
        .fetch_optional(self.storage.conn())
        .await
        .unwrap()
        .map(|row| BasicWitnessGeneratorJobInfo {
            l1_batch_number: batch_number.batch_number(),
            chain_id: L2ChainId::new(row.chain_id as u64).unwrap(),
            witness_inputs_blob_url: row.witness_inputs_blob_url,
            attempts: row.attempts as u32,
            status: row.status.parse::<WitnessJobStatus>().unwrap(),
            error: row.error,
            created_at: row.created_at,
            updated_at: row.updated_at,
            processing_started_at: row.processing_started_at,
            time_taken: row.time_taken,
            protocol_version: row.protocol_version,
            picked_by: row.picked_by,
        })
    }

    pub async fn requeue_stuck_witness_inputs_jobs_for_batch(
        &mut self,
        batch_number: ChainAwareL1BatchNumber,
        max_attempts: u32,
    ) -> Vec<StuckJobs> {
        sqlx::query!(
            r#"
            UPDATE witness_inputs_fri
            SET
                status = 'queued',
                updated_at = NOW(),
                processing_started_at = NOW(),
                priority = priority + 1
            WHERE
                l1_batch_number = $1
                AND chain_id = $2
                AND attempts >= $3
                AND (
                    status = 'in_progress'
                    OR status = 'failed'
                )
            RETURNING
            l1_batch_number,
            chain_id,
            status,
            attempts,
            error,
            picked_by
            "#,
            batch_number.raw_batch_number() as i64,
            batch_number.raw_chain_id() as i32,
            max_attempts as i64
        )
        .fetch_all(self.storage.conn())
        .await
        .unwrap()
        .into_iter()
        .map(|row| StuckJobs {
            id: row.l1_batch_number as u64,
            chain_id: L2ChainId::new(row.chain_id as u64).unwrap(),
            status: row.status,
            attempts: row.attempts as u64,
            circuit_id: None,
            error: row.error,
            picked_by: row.picked_by,
        })
        .collect()
    }

    pub async fn check_reached_max_attempts(&mut self, max_attempts: u32) -> usize {
        sqlx::query_scalar!(
            r#"
            SELECT COUNT(*)
            FROM witness_inputs_fri
            WHERE
                attempts >= $1
                AND status <> 'successful'
            "#,
            max_attempts as i64
        )
        .fetch_one(self.storage.conn())
        .await
        .unwrap()
        .unwrap_or(0) as usize
    }
}
