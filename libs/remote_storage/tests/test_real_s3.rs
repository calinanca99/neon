use std::env;
use std::fmt::{Debug, Display};
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use std::{collections::HashSet, time::SystemTime};

use crate::common::{download_to_vec, upload_stream};
use anyhow::Context;
use camino::Utf8Path;
use futures_util::Future;
use remote_storage::{
    GenericRemoteStorage, RemotePath, RemoteStorageConfig, RemoteStorageKind, S3Config,
};
use test_context::test_context;
use test_context::AsyncTestContext;
use tokio_util::sync::CancellationToken;
use tracing::info;

mod common;

#[path = "common/tests.rs"]
mod tests_s3;

use common::{cleanup, ensure_logging_ready, upload_remote_data, upload_simple_remote_data};
use utils::backoff;

const ENABLE_REAL_S3_REMOTE_STORAGE_ENV_VAR_NAME: &str = "ENABLE_REAL_S3_REMOTE_STORAGE";

const BASE_PREFIX: &str = "test";

#[test_context(MaybeEnabledStorage)]
#[tokio::test]
async fn s3_time_travel_recovery_works(ctx: &mut MaybeEnabledStorage) -> anyhow::Result<()> {
    let ctx = match ctx {
        MaybeEnabledStorage::Enabled(ctx) => ctx,
        MaybeEnabledStorage::Disabled => return Ok(()),
    };
    // Our test depends on discrepancies in the clock between S3 and the environment the tests
    // run in. Therefore, wait a little bit before and after. The alternative would be
    // to take the time from S3 response headers.
    const WAIT_TIME: Duration = Duration::from_millis(3_000);

    async fn retry<T, O, F, E>(op: O) -> Result<T, E>
    where
        E: Display + Debug + 'static,
        O: FnMut() -> F,
        F: Future<Output = Result<T, E>>,
    {
        let warn_threshold = 3;
        let max_retries = 10;
        backoff::retry(
            op,
            |_e| false,
            warn_threshold,
            max_retries,
            "test retry",
            &CancellationToken::new(),
        )
        .await
        .expect("never cancelled")
    }

    async fn time_point() -> SystemTime {
        tokio::time::sleep(WAIT_TIME).await;
        let ret = SystemTime::now();
        tokio::time::sleep(WAIT_TIME).await;
        ret
    }

    async fn list_files(client: &Arc<GenericRemoteStorage>) -> anyhow::Result<HashSet<RemotePath>> {
        Ok(retry(|| client.list_files(None, None))
            .await
            .context("list root files failure")?
            .into_iter()
            .collect::<HashSet<_>>())
    }

    let cancel = CancellationToken::new();

    let path1 = RemotePath::new(Utf8Path::new(format!("{}/path1", ctx.base_prefix).as_str()))
        .with_context(|| "RemotePath conversion")?;

    let path2 = RemotePath::new(Utf8Path::new(format!("{}/path2", ctx.base_prefix).as_str()))
        .with_context(|| "RemotePath conversion")?;

    let path3 = RemotePath::new(Utf8Path::new(format!("{}/path3", ctx.base_prefix).as_str()))
        .with_context(|| "RemotePath conversion")?;

    retry(|| {
        let (data, len) = upload_stream("remote blob data1".as_bytes().into());
        ctx.client.upload(data, len, &path1, None)
    })
    .await?;

    let t0_files = list_files(&ctx.client).await?;
    let t0 = time_point().await;
    println!("at t0: {t0_files:?}");

    let old_data = "remote blob data2";

    retry(|| {
        let (data, len) = upload_stream(old_data.as_bytes().into());
        ctx.client.upload(data, len, &path2, None)
    })
    .await?;

    let t1_files = list_files(&ctx.client).await?;
    let t1 = time_point().await;
    println!("at t1: {t1_files:?}");

    // A little check to ensure that our clock is not too far off from the S3 clock
    {
        let dl = retry(|| ctx.client.download(&path2)).await?;
        let last_modified = dl.last_modified.unwrap();
        let half_wt = WAIT_TIME.mul_f32(0.5);
        let t0_hwt = t0 + half_wt;
        let t1_hwt = t1 - half_wt;
        if !(t0_hwt..=t1_hwt).contains(&last_modified) {
            panic!("last_modified={last_modified:?} is not between t0_hwt={t0_hwt:?} and t1_hwt={t1_hwt:?}. \
                This likely means a large lock discrepancy between S3 and the local clock.");
        }
    }

    retry(|| {
        let (data, len) = upload_stream("remote blob data3".as_bytes().into());
        ctx.client.upload(data, len, &path3, None)
    })
    .await?;

    let new_data = "new remote blob data2";

    retry(|| {
        let (data, len) = upload_stream(new_data.as_bytes().into());
        ctx.client.upload(data, len, &path2, None)
    })
    .await?;

    retry(|| ctx.client.delete(&path1)).await?;
    let t2_files = list_files(&ctx.client).await?;
    let t2 = time_point().await;
    println!("at t2: {t2_files:?}");

    // No changes after recovery to t2 (no-op)
    let t_final = time_point().await;
    ctx.client
        .time_travel_recover(None, t2, t_final, &cancel)
        .await?;
    let t2_files_recovered = list_files(&ctx.client).await?;
    println!("after recovery to t2: {t2_files_recovered:?}");
    assert_eq!(t2_files, t2_files_recovered);
    let path2_recovered_t2 = download_to_vec(ctx.client.download(&path2).await?).await?;
    assert_eq!(path2_recovered_t2, new_data.as_bytes());

    // after recovery to t1: path1 is back, path2 has the old content
    let t_final = time_point().await;
    ctx.client
        .time_travel_recover(None, t1, t_final, &cancel)
        .await?;
    let t1_files_recovered = list_files(&ctx.client).await?;
    println!("after recovery to t1: {t1_files_recovered:?}");
    assert_eq!(t1_files, t1_files_recovered);
    let path2_recovered_t1 = download_to_vec(ctx.client.download(&path2).await?).await?;
    assert_eq!(path2_recovered_t1, old_data.as_bytes());

    // after recovery to t0: everything is gone except for path1
    let t_final = time_point().await;
    ctx.client
        .time_travel_recover(None, t0, t_final, &cancel)
        .await?;
    let t0_files_recovered = list_files(&ctx.client).await?;
    println!("after recovery to t0: {t0_files_recovered:?}");
    assert_eq!(t0_files, t0_files_recovered);

    // cleanup

    let paths = &[path1, path2, path3];
    retry(|| ctx.client.delete_objects(paths)).await?;

    Ok(())
}

struct EnabledS3 {
    client: Arc<GenericRemoteStorage>,
    base_prefix: &'static str,
}

impl EnabledS3 {
    async fn setup(max_keys_in_list_response: Option<i32>) -> Self {
        let client = create_s3_client(max_keys_in_list_response)
            .context("S3 client creation")
            .expect("S3 client creation failed");

        EnabledS3 {
            client,
            base_prefix: BASE_PREFIX,
        }
    }
}

enum MaybeEnabledStorage {
    Enabled(EnabledS3),
    Disabled,
}

#[async_trait::async_trait]
impl AsyncTestContext for MaybeEnabledStorage {
    async fn setup() -> Self {
        ensure_logging_ready();

        if env::var(ENABLE_REAL_S3_REMOTE_STORAGE_ENV_VAR_NAME).is_err() {
            info!(
                "`{}` env variable is not set, skipping the test",
                ENABLE_REAL_S3_REMOTE_STORAGE_ENV_VAR_NAME
            );
            return Self::Disabled;
        }

        Self::Enabled(EnabledS3::setup(None).await)
    }
}

enum MaybeEnabledStorageWithTestBlobs {
    Enabled(S3WithTestBlobs),
    Disabled,
    UploadsFailed(anyhow::Error, S3WithTestBlobs),
}

struct S3WithTestBlobs {
    enabled: EnabledS3,
    remote_prefixes: HashSet<RemotePath>,
    remote_blobs: HashSet<RemotePath>,
}

#[async_trait::async_trait]
impl AsyncTestContext for MaybeEnabledStorageWithTestBlobs {
    async fn setup() -> Self {
        ensure_logging_ready();
        if env::var(ENABLE_REAL_S3_REMOTE_STORAGE_ENV_VAR_NAME).is_err() {
            info!(
                "`{}` env variable is not set, skipping the test",
                ENABLE_REAL_S3_REMOTE_STORAGE_ENV_VAR_NAME
            );
            return Self::Disabled;
        }

        let max_keys_in_list_response = 10;
        let upload_tasks_count = 1 + (2 * usize::try_from(max_keys_in_list_response).unwrap());

        let enabled = EnabledS3::setup(Some(max_keys_in_list_response)).await;

        match upload_remote_data(&enabled.client, enabled.base_prefix, upload_tasks_count).await {
            ControlFlow::Continue(uploads) => {
                info!("Remote objects created successfully");

                Self::Enabled(S3WithTestBlobs {
                    enabled,
                    remote_prefixes: uploads.prefixes,
                    remote_blobs: uploads.blobs,
                })
            }
            ControlFlow::Break(uploads) => Self::UploadsFailed(
                anyhow::anyhow!("One or multiple blobs failed to upload to S3"),
                S3WithTestBlobs {
                    enabled,
                    remote_prefixes: uploads.prefixes,
                    remote_blobs: uploads.blobs,
                },
            ),
        }
    }

    async fn teardown(self) {
        match self {
            Self::Disabled => {}
            Self::Enabled(ctx) | Self::UploadsFailed(_, ctx) => {
                cleanup(&ctx.enabled.client, ctx.remote_blobs).await;
            }
        }
    }
}

// NOTE: the setups for the list_prefixes test and the list_files test are very similar
// However, they are not idential. The list_prefixes function is concerned with listing prefixes,
// whereas the list_files function is concerned with listing files.
// See `RemoteStorage::list_files` documentation for more details
enum MaybeEnabledStorageWithSimpleTestBlobs {
    Enabled(S3WithSimpleTestBlobs),
    Disabled,
    UploadsFailed(anyhow::Error, S3WithSimpleTestBlobs),
}
struct S3WithSimpleTestBlobs {
    enabled: EnabledS3,
    remote_blobs: HashSet<RemotePath>,
}

#[async_trait::async_trait]
impl AsyncTestContext for MaybeEnabledStorageWithSimpleTestBlobs {
    async fn setup() -> Self {
        ensure_logging_ready();
        if env::var(ENABLE_REAL_S3_REMOTE_STORAGE_ENV_VAR_NAME).is_err() {
            info!(
                "`{}` env variable is not set, skipping the test",
                ENABLE_REAL_S3_REMOTE_STORAGE_ENV_VAR_NAME
            );
            return Self::Disabled;
        }

        let max_keys_in_list_response = 10;
        let upload_tasks_count = 1 + (2 * usize::try_from(max_keys_in_list_response).unwrap());

        let enabled = EnabledS3::setup(Some(max_keys_in_list_response)).await;

        match upload_simple_remote_data(&enabled.client, upload_tasks_count).await {
            ControlFlow::Continue(uploads) => {
                info!("Remote objects created successfully");

                Self::Enabled(S3WithSimpleTestBlobs {
                    enabled,
                    remote_blobs: uploads,
                })
            }
            ControlFlow::Break(uploads) => Self::UploadsFailed(
                anyhow::anyhow!("One or multiple blobs failed to upload to S3"),
                S3WithSimpleTestBlobs {
                    enabled,
                    remote_blobs: uploads,
                },
            ),
        }
    }

    async fn teardown(self) {
        match self {
            Self::Disabled => {}
            Self::Enabled(ctx) | Self::UploadsFailed(_, ctx) => {
                cleanup(&ctx.enabled.client, ctx.remote_blobs).await;
            }
        }
    }
}

fn create_s3_client(
    max_keys_per_list_response: Option<i32>,
) -> anyhow::Result<Arc<GenericRemoteStorage>> {
    use rand::Rng;

    let remote_storage_s3_bucket = env::var("REMOTE_STORAGE_S3_BUCKET")
        .context("`REMOTE_STORAGE_S3_BUCKET` env var is not set, but real S3 tests are enabled")?;
    let remote_storage_s3_region = env::var("REMOTE_STORAGE_S3_REGION")
        .context("`REMOTE_STORAGE_S3_REGION` env var is not set, but real S3 tests are enabled")?;

    // due to how time works, we've had test runners use the same nanos as bucket prefixes.
    // millis is just a debugging aid for easier finding the prefix later.
    let millis = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("random s3 test prefix part calculation")?
        .as_millis();

    // because nanos can be the same for two threads so can millis, add randomness
    let random = rand::thread_rng().gen::<u32>();

    let remote_storage_config = RemoteStorageConfig {
        storage: RemoteStorageKind::AwsS3(S3Config {
            bucket_name: remote_storage_s3_bucket,
            bucket_region: remote_storage_s3_region,
            prefix_in_bucket: Some(format!("test_{millis}_{random:08x}/")),
            endpoint: None,
            concurrency_limit: NonZeroUsize::new(100).unwrap(),
            max_keys_per_list_response,
        }),
    };
    Ok(Arc::new(
        GenericRemoteStorage::from_config(&remote_storage_config).context("remote storage init")?,
    ))
}
