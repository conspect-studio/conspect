//! An `sqlx`/SQLite-backed [`Repository`] (feature `sqlite`, **off by default**).
//!
//! `SQLite`'s bundled license is outside the cargo-deny allowlist, so this module
//! is gated behind the off-by-default `sqlite` feature and is **never** part of
//! the CI-green default build. It mirrors the in-memory store's contract:
//! version-stamped layouts with `ETag`/`If-Match` optimistic concurrency
//! (ADR-W006), persisted with WAL.
//!
//! This is intentionally a thin, synchronous-facing implementation: it blocks
//! on the `sqlx` pool inside the `Repository` methods (which are sync) using a
//! dedicated `tokio` handle, keeping the trait identical to the in-memory store.
//! It is control-plane state only and never sits on the engine's data plane, so
//! it cannot back-pressure the engine.
use multiview_core::alarm::{AckState, AlarmId, AlarmRecord};
use multiview_core::time::MediaTime;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::alarm_store::{AlarmFilter, AlarmRepository, VersionedAlarm, ALARM_KIND};
use crate::concurrency::Version;
use crate::error::{ControlError, ControlResult};
use crate::repository::{Layout, LayoutInput, Repository, VersionedLayout, LAYOUT_KIND};

/// A SQLite-backed repository.
#[derive(Debug, Clone)]
pub struct SqliteRepository {
    pool: SqlitePool,
    handle: tokio::runtime::Handle,
}

impl SqliteRepository {
    /// Open (or create) a `SQLite` database at `url` and ensure the schema.
    ///
    /// `url` is an sqlx `SQLite` connection string, e.g. `sqlite::memory:` or
    /// `sqlite:///var/lib/multiview/control.db?mode=rwc`.
    ///
    /// # Errors
    ///
    /// [`ControlError::Repository`] if the pool cannot be opened, the schema
    /// migration fails, or this is called outside a `tokio` runtime.
    pub async fn connect(url: &str) -> ControlResult<Self> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|e| ControlError::Repository(format!("no tokio runtime: {e}")))?;
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect(url)
            .await
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        // WAL + busy_timeout per the persistence brief.
        sqlx::query("PRAGMA journal_mode=WAL;")
            .execute(&pool)
            .await
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        sqlx::query("PRAGMA busy_timeout=5000;")
            .execute(&pool)
            .await
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS layouts (\
                id TEXT PRIMARY KEY,\
                name TEXT NOT NULL,\
                body TEXT NOT NULL,\
                version INTEGER NOT NULL\
            );",
        )
        .execute(&pool)
        .await
        .map_err(|e| ControlError::Repository(e.to_string()))?;
        Ok(Self { pool, handle })
    }

    /// Run an async closure to completion on the captured runtime handle,
    /// blocking the calling (sync) `Repository` method.
    fn block_on<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        tokio::task::block_in_place(|| self.handle.block_on(fut))
    }

    /// Map a row into a versioned layout, parsing the JSON body.
    fn row_to_layout(row: &sqlx::sqlite::SqliteRow) -> ControlResult<VersionedLayout> {
        let id: String = row
            .try_get("id")
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        let name: String = row
            .try_get("name")
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        let body_text: String = row
            .try_get("body")
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        let version: i64 = row
            .try_get("version")
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        let body = serde_json::from_str(&body_text)
            .map_err(|e| ControlError::Repository(format!("corrupt layout body: {e}")))?;
        let version = u64::try_from(version)
            .map_err(|_| ControlError::Repository("negative version".to_owned()))?;
        Ok(VersionedLayout {
            version: Version::new(version),
            layout: Layout { id, name, body },
        })
    }
}

impl Repository for SqliteRepository {
    fn list_layouts(&self) -> ControlResult<Vec<VersionedLayout>> {
        self.block_on(async {
            let rows = sqlx::query("SELECT id, name, body, version FROM layouts ORDER BY id")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| ControlError::Repository(e.to_string()))?;
            rows.iter().map(Self::row_to_layout).collect()
        })
    }

    fn get_layout(&self, id: &str) -> ControlResult<VersionedLayout> {
        self.block_on(async {
            let row = sqlx::query("SELECT id, name, body, version FROM layouts WHERE id = ?")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|e| ControlError::Repository(e.to_string()))?;
            let row = row.ok_or_else(|| ControlError::NotFound {
                kind: LAYOUT_KIND,
                id: id.to_owned(),
            })?;
            Self::row_to_layout(&row)
        })
    }

    fn create_layout(&self, id: &str, input: LayoutInput) -> ControlResult<VersionedLayout> {
        self.block_on(async {
            let body = serde_json::to_string(&input.body)
                .map_err(|e| ControlError::Repository(e.to_string()))?;
            let version = i64::try_from(Version::INITIAL.get())
                .map_err(|_| ControlError::Repository("version overflow".to_owned()))?;
            let result = sqlx::query(
                "INSERT OR IGNORE INTO layouts (id, name, body, version) VALUES (?, ?, ?, ?)",
            )
            .bind(id)
            .bind(&input.name)
            .bind(&body)
            .bind(version)
            .execute(&self.pool)
            .await
            .map_err(|e| ControlError::Repository(e.to_string()))?;
            if result.rows_affected() == 0 {
                return Err(ControlError::Validation(format!(
                    "layout {id:?} already exists"
                )));
            }
            Ok(VersionedLayout {
                version: Version::INITIAL,
                layout: Layout {
                    id: id.to_owned(),
                    name: input.name,
                    body: input.body,
                },
            })
        })
    }

    fn update_layout(&self, id: &str, input: LayoutInput) -> ControlResult<VersionedLayout> {
        self.block_on(async {
            let body = serde_json::to_string(&input.body)
                .map_err(|e| ControlError::Repository(e.to_string()))?;
            let result = sqlx::query(
                "UPDATE layouts SET name = ?, body = ?, version = version + 1 WHERE id = ?",
            )
            .bind(&input.name)
            .bind(&body)
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|e| ControlError::Repository(e.to_string()))?;
            if result.rows_affected() == 0 {
                return Err(ControlError::NotFound {
                    kind: LAYOUT_KIND,
                    id: id.to_owned(),
                });
            }
            self.get_layout(id)
        })
    }

    fn delete_layout(&self, id: &str) -> ControlResult<()> {
        self.block_on(async {
            let result = sqlx::query("DELETE FROM layouts WHERE id = ?")
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(|e| ControlError::Repository(e.to_string()))?;
            if result.rows_affected() == 0 {
                Err(ControlError::NotFound {
                    kind: LAYOUT_KIND,
                    id: id.to_owned(),
                })
            } else {
                Ok(())
            }
        })
    }
}

/// A `SQLite`-backed [`AlarmRepository`] (feature `sqlite`, off by default).
///
/// Mirrors [`InMemoryAlarmStore`](crate::alarm_store::InMemoryAlarmStore)'s
/// contract: each alarm is stored as its `serde`-JSON [`AlarmRecord`] plus a
/// monotonic version for `ETag`/`If-Match`. An [`upsert`](AlarmRepository::upsert)
/// of an identical record does not churn the version. Control-plane state only —
/// never on the engine's data plane, so it cannot back-pressure the engine.
#[derive(Debug, Clone)]
pub struct SqliteAlarmStore {
    pool: SqlitePool,
    handle: tokio::runtime::Handle,
}

impl SqliteAlarmStore {
    /// Open (or create) a `SQLite` database at `url` and ensure the alarm schema.
    ///
    /// `url` is an sqlx `SQLite` connection string, e.g. `sqlite::memory:`.
    ///
    /// # Errors
    ///
    /// [`ControlError::Repository`] if the pool cannot be opened, the schema
    /// migration fails, or this is called outside a `tokio` runtime.
    pub async fn connect(url: &str) -> ControlResult<Self> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|e| ControlError::Repository(format!("no tokio runtime: {e}")))?;
        let pool = SqlitePoolOptions::new()
            .max_connections(4)
            .connect(url)
            .await
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        sqlx::query("PRAGMA journal_mode=WAL;")
            .execute(&pool)
            .await
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        sqlx::query("PRAGMA busy_timeout=5000;")
            .execute(&pool)
            .await
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS alarms (\
                id TEXT PRIMARY KEY,\
                record TEXT NOT NULL,\
                version INTEGER NOT NULL\
            );",
        )
        .execute(&pool)
        .await
        .map_err(|e| ControlError::Repository(e.to_string()))?;
        Ok(Self { pool, handle })
    }

    /// Run an async closure to completion on the captured runtime handle,
    /// blocking the calling (sync) [`AlarmRepository`] method.
    fn block_on<F, T>(&self, fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        tokio::task::block_in_place(|| self.handle.block_on(fut))
    }

    /// Parse a row into a versioned alarm, deserialising the JSON record.
    fn row_to_alarm(row: &sqlx::sqlite::SqliteRow) -> ControlResult<VersionedAlarm> {
        let record_text: String = row
            .try_get("record")
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        let version: i64 = row
            .try_get("version")
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        let record: AlarmRecord = serde_json::from_str(&record_text)
            .map_err(|e| ControlError::Repository(format!("corrupt alarm record: {e}")))?;
        let version = u64::try_from(version)
            .map_err(|_| ControlError::Repository("negative version".to_owned()))?;
        Ok(VersionedAlarm {
            version: Version::new(version),
            record,
        })
    }

    /// Fetch the stored alarm for `id`, or `None` if absent.
    async fn fetch(&self, id: &str) -> ControlResult<Option<VersionedAlarm>> {
        let row = sqlx::query("SELECT id, record, version FROM alarms WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        row.as_ref().map(Self::row_to_alarm).transpose()
    }

    /// Write `versioned` (insert or replace) for `id`.
    async fn write(&self, id: &str, versioned: &VersionedAlarm) -> ControlResult<()> {
        let record = serde_json::to_string(&versioned.record)
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        let version = i64::try_from(versioned.version.get())
            .map_err(|_| ControlError::Repository("version overflow".to_owned()))?;
        sqlx::query("INSERT OR REPLACE INTO alarms (id, record, version) VALUES (?, ?, ?)")
            .bind(id)
            .bind(&record)
            .bind(version)
            .execute(&self.pool)
            .await
            .map_err(|e| ControlError::Repository(e.to_string()))?;
        Ok(())
    }
}

impl AlarmRepository for SqliteAlarmStore {
    fn list(&self, filter: &AlarmFilter) -> ControlResult<Vec<VersionedAlarm>> {
        self.block_on(async {
            let rows = sqlx::query("SELECT id, record, version FROM alarms ORDER BY id")
                .fetch_all(&self.pool)
                .await
                .map_err(|e| ControlError::Repository(e.to_string()))?;
            let all: Vec<VersionedAlarm> = rows
                .iter()
                .map(Self::row_to_alarm)
                .collect::<ControlResult<Vec<_>>>()?;
            Ok(all
                .into_iter()
                .filter(|v| filter.matches(&v.record))
                .collect())
        })
    }

    fn get(&self, id: &AlarmId) -> ControlResult<VersionedAlarm> {
        self.block_on(async {
            self.fetch(id.as_str())
                .await?
                .ok_or_else(|| ControlError::NotFound {
                    kind: ALARM_KIND,
                    id: id.as_str().to_owned(),
                })
        })
    }

    fn upsert(&self, record: AlarmRecord) -> ControlResult<VersionedAlarm> {
        self.block_on(async {
            let key = record.id.as_str().to_owned();
            let next = match self.fetch(&key).await? {
                Some(existing) if existing.record == record => return Ok(existing),
                Some(existing) => VersionedAlarm {
                    version: existing.version.next(),
                    record,
                },
                None => VersionedAlarm {
                    version: Version::INITIAL,
                    record,
                },
            };
            self.write(&key, &next).await?;
            Ok(next)
        })
    }

    fn acknowledge(
        &self,
        id: &AlarmId,
        who: &str,
        when: MediaTime,
    ) -> ControlResult<VersionedAlarm> {
        self.block_on(async {
            let existing =
                self.fetch(id.as_str())
                    .await?
                    .ok_or_else(|| ControlError::NotFound {
                        kind: ALARM_KIND,
                        id: id.as_str().to_owned(),
                    })?;
            let mut record = existing.record;
            record.ack = AckState::acked(who, when);
            let next = VersionedAlarm {
                version: existing.version.next(),
                record,
            };
            self.write(id.as_str(), &next).await?;
            Ok(next)
        })
    }
}
