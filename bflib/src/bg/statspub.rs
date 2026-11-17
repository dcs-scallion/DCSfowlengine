use anyhow::{Context, Result, anyhow};
use arcstr::ArcStr;
use bfprotocols::stats::{PATH, Stat};
use chrono::prelude::*;
use log::{error, warn};
use netidx::{
    chars::Chars,
    config::Config,
    path::Path,
    publisher::{PublisherBuilder, Value},
    resolver_client::GlobSet,
    subscriber::Event,
};
use netidx_archive::{
    config::{ConfigBuilder, PublishConfigBuilder, RecordConfigBuilder},
    logfile::{BATCH_POOL, BatchItem, Id},
    logfile_collection::ArchiveCollectionWriter,
    recorder::Recorder,
};
use std::{fs, path::PathBuf};
use tokio::task;

use super::encode;

pub(super) struct Statspub {
    id: Id,
    log: ArchiveCollectionWriter,
    _recorder: Recorder,
}

impl Statspub {
    /// Own publisher so archive failure cannot tear down API/RPC publisher.
    pub(super) async fn new(cfg: &Config, write_dir: PathBuf, base: Path) -> Result<Self> {
        let publisher = PublisherBuilder::new(cfg.clone())
            .build()
            .await
            .context("stats archive publisher")?;
        let shard = ArcStr::from("0");
        let config = ConfigBuilder::default()
            .archive_directory(write_dir)
            .publish(
                PublishConfigBuilder::default()
                    .base(base)
                    .bind_from_cfg(&cfg)
                    .build()
                    .context("publish config builder")?,
            )
            .record([(
                shard.clone(),
                RecordConfigBuilder::default()
                    .spec(GlobSet::new(true, vec![])?)
                    .build()
                    .context("record config builder")?,
            )])
            .netidx_config(cfg.clone())
            .build()?;
        let recorder = Recorder::start_with(config, Some(publisher), None)
            .await
            .context("starting recorder tasks")?;
        let shard_id = *recorder
            .shards
            .by_name
            .get(&shard)
            .ok_or_else(|| anyhow!("no shard id for shard {shard}"))?;
        let mut log = recorder
            .shards
            .writers
            .lock()
            .remove(&shard_id)
            .ok_or_else(|| anyhow!("no writer for shard"))?;
        if log.len()? > 4096 {
            let now = Utc::now();
            let rotated = match log.rotate_and_compress(now, None).await {
                Ok(_) => true,
                Err(e) => {
                    warn!("stats archive compress-rotate failed ({e:#}); trying plain rotate");
                    match log.rotate(now) {
                        Ok(_) => true,
                        Err(e2) => {
                            warn!(
                                "stats archive plain rotate failed ({e2:#}); continuing without rotate"
                            );
                            false
                        }
                    }
                }
            };
            if rotated {
                let reader = log
                    .current_reader()
                    .context("get current reader after rotate")?;
                recorder
                    .shards
                    .notify_rotated(shard_id, now, reader)
                    .context("notify rotate")?;
            }
        }
        let id = match log.id_for_path(&Path::from(PATH)) {
            Some(id) => id,
            None => {
                log.add_paths([&Path::from(PATH)])?;
                task::block_in_place(|| log.flush_pathindex())?;
                log.id_for_path(&Path::from(PATH))
                    .ok_or_else(|| anyhow!("no id after adding id"))?
            }
        };
        Ok(Self {
            id,
            log,
            _recorder: recorder,
        })
    }

    /// Init with one quarantine+retry if the shard archive is corrupt/locked.
    pub(super) async fn new_resilient(
        cfg: &Config,
        write_dir: PathBuf,
        base: Path,
    ) -> Result<Self> {
        match Self::new(cfg, write_dir.clone(), base.clone()).await {
            Ok(s) => Ok(s),
            Err(e) => {
                error!(
                    "stats archive init failed ({e:#}); quarantining shard and retrying once"
                );
                quarantine_stats_shard(&write_dir);
                Self::new(cfg, write_dir, base)
                    .await
                    .context("stats archive init after quarantine")
            }
        }
    }

    /// This will not block
    pub(super) fn append(&mut self, ts: DateTime<Utc>, stat: &Stat) -> Result<()> {
        task::block_in_place(|| {
            let mut batch = BATCH_POOL.take();
            let buf = Chars::from_bytes(encode(&stat)?.freeze())?;
            batch.push(BatchItem(self.id, Event::Update(Value::String(buf))));
            self.log.add_batch(false, ts, &batch)
        })
    }

    /// Flush the log
    pub(super) fn flush(&mut self) -> Result<()> {
        self.log.flush_current()
    }
}

fn quarantine_stats_shard(stats_dir: &std::path::Path) {
    let shard = stats_dir.join("0");
    if !shard.exists() {
        return;
    }
    let bak = stats_dir.join(format!(
        "0.bak-filemode-{}",
        Utc::now().format("%Y%m%d-%H%M%S")
    ));
    match fs::rename(&shard, &bak) {
        Ok(()) => warn!("quarantined stats archive {shard:?} -> {bak:?}"),
        Err(e) => error!("failed to quarantine stats archive {shard:?}: {e:?}"),
    }
}
