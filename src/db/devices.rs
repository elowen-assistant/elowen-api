//! Device persistence helpers.

use anyhow::anyhow;
use sqlx::{PgPool, types::Json as SqlxJson};

use crate::{
    error::AppError,
    models::{DeviceMetadata, DeviceRecord, DeviceRow},
};

pub(crate) async fn list_devices(pool: &PgPool) -> Result<Vec<DeviceRecord>, AppError> {
    let devices = sqlx::query_as::<_, DeviceRow>(
        r#"
        select id, name, primary_flag, metadata, created_at, updated_at
        from devices
        order by primary_flag desc, updated_at desc, name asc
        "#,
    )
    .fetch_all(pool)
    .await?;

    Ok(devices.into_iter().map(Into::into).collect())
}

pub(crate) async fn load_device_row(pool: &PgPool, device_id: &str) -> Result<DeviceRow, AppError> {
    load_device_row_optional(pool, device_id)
        .await?
        .ok_or_else(|| AppError::not_found(anyhow!("device not found")))
}

pub(crate) async fn load_device_row_optional(
    pool: &PgPool,
    device_id: &str,
) -> Result<Option<DeviceRow>, AppError> {
    let device = sqlx::query_as::<_, DeviceRow>(
        r#"
        select id, name, primary_flag, metadata, created_at, updated_at
        from devices
        where id = $1
        "#,
    )
    .bind(device_id)
    .fetch_optional(pool)
    .await?;

    Ok(device)
}

pub(crate) async fn upsert_device_row(
    pool: &PgPool,
    device_id: &str,
    name: &str,
    primary_flag: bool,
    metadata: DeviceMetadata,
) -> Result<DeviceRow, AppError> {
    let device = sqlx::query_as::<_, DeviceRow>(
        r#"
        insert into devices (id, name, primary_flag, metadata)
        values ($1, $2, $3, $4)
        on conflict (id) do update set
            name = excluded.name,
            primary_flag = excluded.primary_flag,
            metadata = excluded.metadata,
            updated_at = now()
        returning id, name, primary_flag, metadata, created_at, updated_at
        "#,
    )
    .bind(device_id)
    .bind(name)
    .bind(primary_flag)
    .bind(SqlxJson(metadata))
    .fetch_one(pool)
    .await?;

    Ok(device)
}
