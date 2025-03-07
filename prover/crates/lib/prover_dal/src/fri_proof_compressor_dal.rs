#![doc = include_str!("../doc/FriProofCompressorDal.md")]
use std::{collections::HashMap, str::FromStr, time::Duration};

use zksync_basic_types::{
    protocol_version::{ProtocolSemanticVersion, ProtocolVersionId, VersionPatch},
    prover_dal::{
        JobCountStatistics, ProofCompressionJobInfo, ProofCompressionJobStatus, StuckJobs,
    },
    L1BatchNumber, L2ChainId,
};
use zksync_db_connection::{connection::Connection, error::DalResult, instrument::InstrumentExt};

use crate::{duration_to_naive_time, pg_interval_from_duration, Prover};

#[derive(Debug)]
pub struct FriProofCompressorDal<'a, 'c> {
    pub(crate) storage: &'a mut Connection<'c, Prover>,
}

impl FriProofCompressorDal<'_, '_> {
    pub async fn insert_proof_compression_job(
        &mut self,
        block_number: L1BatchNumber,
        chain_id: L2ChainId,
        fri_proof_blob_url: &str,
        protocol_version: ProtocolSemanticVersion,
    ) {
        sqlx::query!(
            r#"
            INSERT INTO
            proof_compression_jobs_fri (
                l1_batch_number,
                chain_id,
                fri_proof_blob_url,
                status,
                created_at,
                updated_at,
                protocol_version,
                protocol_version_patch
            )
            VALUES
            ($1, $2, $3, $4, NOW(), NOW(), $5, $6)
            ON CONFLICT (l1_batch_number) DO NOTHING
            "#,
            i64::from(block_number.0),
            chain_id.as_u64() as i32,
            fri_proof_blob_url,
            ProofCompressionJobStatus::Queued.to_string(),
            protocol_version.minor as i32,
            protocol_version.patch.0 as i32
        )
        .fetch_optional(self.storage.conn())
        .await
        .unwrap();
    }

    pub async fn get_next_proof_compression_job(
        &mut self,
        picked_by: &str,
        protocol_version: ProtocolSemanticVersion,
    ) -> Option<(L2ChainId, L1BatchNumber)> {
        sqlx::query!(
            r#"
            UPDATE proof_compression_jobs_fri
            SET
                status = $1,
                attempts = attempts + 1,
                updated_at = NOW(),
                processing_started_at = NOW(),
                picked_by = $3
            WHERE
                (l1_batch_number, chain_id) = (
                    SELECT
                        l1_batch_number,
                        chain_id
                    FROM
                        proof_compression_jobs_fri
                    WHERE
                        status = $2
                        AND protocol_version = $4
                        AND protocol_version_patch = $5
                    ORDER BY
                        priority DESC,
                        created_at ASC
                    LIMIT
                        1
                    FOR UPDATE
                    SKIP LOCKED
                )
            RETURNING
            proof_compression_jobs_fri.l1_batch_number,
            proof_compression_jobs_fri.chain_id
            "#,
            ProofCompressionJobStatus::InProgress.to_string(),
            ProofCompressionJobStatus::Queued.to_string(),
            picked_by,
            protocol_version.minor as i32,
            protocol_version.patch.0 as i32
        )
        .fetch_optional(self.storage.conn())
        .await
        .unwrap()
        .map(|row| {
            (
                L2ChainId::new(row.chain_id as u64).unwrap(),
                L1BatchNumber(row.l1_batch_number as u32),
            )
        })
    }

    pub async fn get_proof_compression_job_attempts(
        &mut self,
        l1_batch_number: L1BatchNumber,
        chain_id: L2ChainId,
    ) -> sqlx::Result<Option<u32>> {
        let attempts = sqlx::query!(
            r#"
            SELECT
                attempts
            FROM
                proof_compression_jobs_fri
            WHERE
                l1_batch_number = $1
                AND chain_id = $2
            "#,
            i64::from(l1_batch_number.0),
            chain_id.as_u64() as i32
        )
        .fetch_optional(self.storage.conn())
        .await?
        .map(|row| row.attempts as u32);

        Ok(attempts)
    }

    pub async fn mark_proof_compression_job_successful(
        &mut self,
        block_number: L1BatchNumber,
        chain_id: L2ChainId,
        time_taken: Duration,
        l1_proof_blob_url: &str,
    ) {
        sqlx::query!(
            r#"
            UPDATE proof_compression_jobs_fri
            SET
                status = $1,
                updated_at = NOW(),
                time_taken = $2,
                l1_proof_blob_url = $3
            WHERE
                l1_batch_number = $4
                AND chain_id = $5
            "#,
            ProofCompressionJobStatus::Successful.to_string(),
            duration_to_naive_time(time_taken),
            l1_proof_blob_url,
            i64::from(block_number.0),
            chain_id.as_u64() as i32
        )
        .execute(self.storage.conn())
        .await
        .unwrap();
    }

    pub async fn mark_proof_compression_job_failed(
        &mut self,
        error: &str,
        block_number: L1BatchNumber,
        chain_id: L2ChainId,
    ) {
        sqlx::query!(
            r#"
            UPDATE proof_compression_jobs_fri
            SET
                status = $1,
                error = $2,
                updated_at = NOW()
            WHERE
                l1_batch_number = $3
                AND chain_id = $4
                AND status != $5
                AND status != $6
            "#,
            ProofCompressionJobStatus::Failed.to_string(),
            error,
            i64::from(block_number.0),
            chain_id.as_u64() as i32,
            ProofCompressionJobStatus::Successful.to_string(),
            ProofCompressionJobStatus::SentToServer.to_string(),
        )
        .execute(self.storage.conn())
        .await
        .unwrap();
    }

    pub async fn get_least_proven_block_not_sent_to_server(
        &mut self,
        chain_id: L2ChainId,
    ) -> Option<(
        L1BatchNumber,
        ProtocolSemanticVersion,
        ProofCompressionJobStatus,
    )> {
        let row = sqlx::query!(
            r#"
            SELECT
                l1_batch_number,
                chain_id,
                status,
                protocol_version,
                protocol_version_patch
            FROM
                proof_compression_jobs_fri
            WHERE
                (l1_batch_number) = (
                    SELECT
                        MIN(l1_batch_number)
                    FROM
                        proof_compression_jobs_fri
                    WHERE
                        (
                            status = $1
                            OR status = $2
                        )
                        AND chain_id = $3
                )
            "#,
            ProofCompressionJobStatus::Successful.to_string(),
            ProofCompressionJobStatus::Skipped.to_string(),
            chain_id.as_u64() as i32
        )
        .fetch_optional(self.storage.conn())
        .await
        .ok()?;
        match row {
            Some(row) => Some((
                L1BatchNumber(row.l1_batch_number as u32),
                ProtocolSemanticVersion::new(
                    ProtocolVersionId::try_from(row.protocol_version.unwrap() as u16).unwrap(),
                    VersionPatch(row.protocol_version_patch as u32),
                ),
                ProofCompressionJobStatus::from_str(&row.status).unwrap(),
            )),
            None => None,
        }
    }

    pub async fn mark_proof_sent_to_server(
        &mut self,
        block_number: L1BatchNumber,
        chain_id: L2ChainId,
    ) -> DalResult<()> {
        sqlx::query!(
            r#"
            UPDATE proof_compression_jobs_fri
            SET
                status = $1,
                updated_at = NOW()
            WHERE
                l1_batch_number = $2
                AND chain_id = $3
            "#,
            ProofCompressionJobStatus::SentToServer.to_string(),
            i64::from(block_number.0),
            chain_id.as_u64() as i32
        )
        .instrument("mark_proof_sent_to_server")
        .execute(self.storage)
        .await?;
        Ok(())
    }

    pub async fn get_jobs_stats(&mut self) -> HashMap<ProtocolSemanticVersion, JobCountStatistics> {
        sqlx::query!(
            r#"
            SELECT
                protocol_version,
                protocol_version_patch,
                COUNT(*) FILTER (
                    WHERE
                    status = 'queued'
                ) AS queued,
                COUNT(*) FILTER (
                    WHERE
                    status = 'in_progress'
                ) AS in_progress
            FROM
                proof_compression_jobs_fri
            WHERE
                protocol_version IS NOT NULL
            GROUP BY
                protocol_version,
                protocol_version_patch
            "#,
        )
        .fetch_all(self.storage.conn())
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            let key = ProtocolSemanticVersion::new(
                ProtocolVersionId::try_from(row.protocol_version.unwrap() as u16).unwrap(),
                VersionPatch(row.protocol_version_patch as u32),
            );
            let value = JobCountStatistics {
                queued: row.queued.unwrap() as usize,
                in_progress: row.in_progress.unwrap() as usize,
            };
            (key, value)
        })
        .collect()
    }

    pub async fn get_oldest_not_compressed_batch(&mut self) -> Option<(L2ChainId, L1BatchNumber)> {
        let result: Option<(L2ChainId, L1BatchNumber)> = sqlx::query!(
            r#"
            SELECT
                l1_batch_number,
                chain_id
            FROM
                proof_compression_jobs_fri
            WHERE
                status <> 'successful'
                AND status <> 'sent_to_server'
            ORDER BY
                l1_batch_number ASC
            LIMIT
                1
            "#,
        )
        .fetch_optional(self.storage.conn())
        .await
        .unwrap()
        .map(|row| {
            (
                L2ChainId::new(row.chain_id as u64).unwrap(),
                L1BatchNumber(row.l1_batch_number as u32),
            )
        });

        result
    }

    pub async fn requeue_stuck_jobs(
        &mut self,
        processing_timeout: Duration,
        max_attempts: u32,
    ) -> Vec<StuckJobs> {
        let processing_timeout = pg_interval_from_duration(processing_timeout);
        {
            sqlx::query!(
                r#"
                UPDATE proof_compression_jobs_fri
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
    }

    pub async fn get_proof_compression_job_for_batch(
        &mut self,
        block_number: L1BatchNumber,
        chain_id: L2ChainId,
    ) -> Option<ProofCompressionJobInfo> {
        sqlx::query!(
            r#"
            SELECT
                *
            FROM
                proof_compression_jobs_fri
            WHERE
                l1_batch_number = $1
                AND chain_id = $2
            "#,
            i64::from(block_number.0),
            chain_id.as_u64() as i32,
        )
        .fetch_optional(self.storage.conn())
        .await
        .unwrap()
        .map(|row| ProofCompressionJobInfo {
            l1_batch_number: block_number,
            chain_id: L2ChainId::new(row.chain_id as u64).unwrap(),
            attempts: row.attempts as u32,
            status: ProofCompressionJobStatus::from_str(&row.status).unwrap(),
            fri_proof_blob_url: row.fri_proof_blob_url,
            l1_proof_blob_url: row.l1_proof_blob_url,
            error: row.error,
            created_at: row.created_at,
            updated_at: row.updated_at,
            processing_started_at: row.processing_started_at,
            time_taken: row.time_taken,
            picked_by: row.picked_by,
        })
    }

    pub async fn delete_batch_data(
        &mut self,
        block_number: L1BatchNumber,
        chain_id: L2ChainId,
    ) -> sqlx::Result<sqlx::postgres::PgQueryResult> {
        sqlx::query!(
            r#"
            DELETE FROM proof_compression_jobs_fri
            WHERE
                l1_batch_number = $1
                AND chain_id = $2
            "#,
            i64::from(block_number.0),
            chain_id.as_u64() as i32,
        )
        .execute(self.storage.conn())
        .await
    }

    pub async fn delete(&mut self) -> sqlx::Result<sqlx::postgres::PgQueryResult> {
        sqlx::query!(
            r#"
            DELETE FROM proof_compression_jobs_fri
            "#
        )
        .execute(self.storage.conn())
        .await
    }

    pub async fn requeue_stuck_jobs_for_batch(
        &mut self,
        block_number: L1BatchNumber,
        chain_id: L2ChainId,
        max_attempts: u32,
    ) -> Vec<StuckJobs> {
        {
            sqlx::query!(
                r#"
                UPDATE proof_compression_jobs_fri
                SET
                    status = 'queued',
                    error = 'Manually requeued',
                    attempts = 2,
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
                status,
                attempts,
                error,
                picked_by
                "#,
                i64::from(block_number.0),
                chain_id.as_u64() as i32,
                max_attempts as i32,
            )
            .fetch_all(self.storage.conn())
            .await
            .unwrap()
            .into_iter()
            .map(|row| StuckJobs {
                id: block_number.0 as u64,
                chain_id,
                status: row.status,
                attempts: row.attempts as u64,
                circuit_id: None,
                error: row.error,
                picked_by: row.picked_by,
            })
            .collect()
        }
    }

    pub async fn check_reached_max_attempts(&mut self, max_attempts: u32) -> usize {
        sqlx::query_scalar!(
            r#"
            SELECT COUNT(*)
            FROM proof_compression_jobs_fri
            WHERE
                attempts >= $1
                AND status <> 'successful'
                AND status <> 'sent_to_server'
            "#,
            max_attempts as i64
        )
        .fetch_one(self.storage.conn())
        .await
        .unwrap()
        .unwrap_or(0) as usize
    }
}
