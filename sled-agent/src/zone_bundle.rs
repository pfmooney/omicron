// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2023 Oxide Computer Company

//! Tools for collecting and inspecting service bundles for zones.

use crate::storage_manager::StorageResources;
use anyhow::anyhow;
use anyhow::Context;
use camino::FromPathBufError;
use camino::Utf8Path;
use camino::Utf8PathBuf;
use chrono::DateTime;
use chrono::Utc;
use flate2::bufread::GzDecoder;
use illumos_utils::running_zone::is_oxide_smf_log_file;
use illumos_utils::running_zone::RunningZone;
use illumos_utils::zfs::ZFS;
use illumos_utils::zone::AdmError;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use slog::Logger;
use std::cmp::Ord;
use std::cmp::Ordering;
use std::cmp::PartialOrd;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::collections::HashSet;
use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use tar::Archive;
use tar::Builder;
use tar::Header;
use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::sync::Notify;
use tokio::time::sleep;
use tokio::time::Instant;
use uuid::Uuid;

/// An identifier for a zone bundle.
#[derive(
    Clone,
    Debug,
    Deserialize,
    Eq,
    Hash,
    JsonSchema,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
)]
pub struct ZoneBundleId {
    /// The name of the zone this bundle is derived from.
    pub zone_name: String,
    /// The ID for this bundle itself.
    pub bundle_id: Uuid,
}

/// The reason or cause for a zone bundle, i.e., why it was created.
//
// NOTE: The ordering of the enum variants is important, and should not be
// changed without careful consideration.
//
// The ordering is used when deciding which bundles to remove automatically. In
// addition to time, the cause is used to sort bundles, so changing the variant
// order will change that priority.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    Deserialize,
    Eq,
    Hash,
    JsonSchema,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ZoneBundleCause {
    /// Some other, unspecified reason.
    #[default]
    Other,
    /// A zone bundle taken when a sled agent finds a zone that it does not
    /// expect to be running.
    UnexpectedZone,
    /// An instance zone was terminated.
    TerminatedInstance,
    /// Generated in response to an explicit request to the sled agent.
    ExplicitRequest,
}

/// Metadata about a zone bundle.
#[derive(
    Clone,
    Debug,
    Deserialize,
    Eq,
    Hash,
    JsonSchema,
    Ord,
    PartialEq,
    PartialOrd,
    Serialize,
)]
pub struct ZoneBundleMetadata {
    /// Identifier for this zone bundle
    pub id: ZoneBundleId,
    /// The time at which this zone bundle was created.
    pub time_created: DateTime<Utc>,
    /// A version number for this zone bundle.
    pub version: u8,
    /// The reason or cause a bundle was created.
    pub cause: ZoneBundleCause,
}

impl ZoneBundleMetadata {
    const VERSION: u8 = 0;

    /// Create a new set of metadata for the provided zone.
    pub(crate) fn new(zone_name: &str, cause: ZoneBundleCause) -> Self {
        Self {
            id: ZoneBundleId {
                zone_name: zone_name.to_string(),
                bundle_id: Uuid::new_v4(),
            },
            time_created: Utc::now(),
            version: Self::VERSION,
            cause,
        }
    }
}

/// A type managing zone bundle creation and automatic cleanup.
#[derive(Clone)]
pub struct ZoneBundler {
    log: Logger,
    inner: Arc<Mutex<Inner>>,
    // Channel for notifying the cleanup task that it should reevaluate.
    notify_cleanup: Arc<Notify>,
    // Tokio task handle running the period cleanup operation.
    cleanup_task: Arc<tokio::task::JoinHandle<()>>,
}

impl Drop for ZoneBundler {
    fn drop(&mut self) {
        self.cleanup_task.abort();
    }
}

// State shared between tasks, e.g., used when creating a bundle in different
// tasks or between a creation and cleanup.
struct Inner {
    resources: StorageResources,
    cleanup_context: CleanupContext,
    last_cleanup_at: Instant,
}

impl Inner {
    // Return the time at which the next cleanup should occur, and the duration
    // until that time.
    //
    // The instant may be in the past, in which case duration would be 0.
    fn next_cleanup(&self) -> (Instant, Duration) {
        let next =
            self.last_cleanup_at + self.cleanup_context.period.as_duration();
        let delta = next.saturating_duration_since(Instant::now());
        (next, delta)
    }

    // Ensure that the zone bundle directories that _can_ exist in fact do.
    //
    // The zone bundles are stored in a ZFS dataset on each M.2. These datasets
    // are created by the storage manager upon request. Until those parent
    // datasets exist, the bundle directories themselves cannot be accessed
    // either.
    //
    // This method takes the _expected_ zone bundle directories; creates any
    // that can exist but do not, i.e., those whose parent datasets already
    // exist; and returns those.
    async fn bundle_directories(&self) -> Vec<Utf8PathBuf> {
        let expected = self.resources.all_zone_bundle_directories().await;
        let mut out = Vec::with_capacity(expected.len());
        for each in expected.into_iter() {
            if tokio::fs::create_dir_all(&each).await.is_ok() {
                out.push(each);
            }
        }
        out
    }
}

impl ZoneBundler {
    // A task run in the background that periodically cleans up bundles.
    //
    // This waits for:
    //
    // - A timeout at the current cleanup period
    // - A notification that the cleanup context has changed.
    //
    // When needed, it actually runs the period cleanup itself, using the
    // current context.
    async fn periodic_cleanup(
        log: Logger,
        inner: Arc<Mutex<Inner>>,
        notify_cleanup: Arc<Notify>,
    ) {
        let (mut next_cleanup, mut time_to_next_cleanup) =
            inner.lock().await.next_cleanup();
        loop {
            info!(
                log,
                "top of bundle cleanup loop";
                "next_cleanup" => ?&next_cleanup,
                "time_to_next_cleanup" => ?time_to_next_cleanup,
            );

            // Wait for the cleanup period to expire, or a notification that the
            // context has been changed.
            tokio::select! {
                _ = sleep(time_to_next_cleanup) => {
                    info!(log, "running automatic periodic zone bundle cleanup");
                    let mut inner_ = inner.lock().await;
                    let dirs = inner_.bundle_directories().await;
                    let res = run_cleanup(&log, &dirs, &inner_.cleanup_context).await;
                    inner_.last_cleanup_at = Instant::now();
                    (next_cleanup, time_to_next_cleanup) = inner_.next_cleanup();
                    debug!(log, "cleanup completed"; "result" => ?res);
                }
                _ = notify_cleanup.notified() => {
                    debug!(log, "notified about cleanup context change");
                    let inner_ = inner.lock().await;
                    (next_cleanup, time_to_next_cleanup) = inner_.next_cleanup();
                }
            }
        }
    }

    /// Create a new zone bundler.
    ///
    /// This creates an object that manages zone bundles on the system. It can
    /// be used to create bundles from running zones, and runs a period task to
    /// clean them up to free up space.
    pub fn new(
        log: Logger,
        resources: StorageResources,
        cleanup_context: CleanupContext,
    ) -> Self {
        let notify_cleanup = Arc::new(Notify::new());
        let inner = Arc::new(Mutex::new(Inner {
            resources,
            cleanup_context,
            last_cleanup_at: Instant::now(),
        }));
        let cleanup_log = log.new(slog::o!("component" => "auto-cleanup-task"));
        let notify_clone = notify_cleanup.clone();
        let inner_clone = inner.clone();
        let cleanup_task = Arc::new(tokio::task::spawn(
            Self::periodic_cleanup(cleanup_log, inner_clone, notify_clone),
        ));
        Self { log, inner, notify_cleanup, cleanup_task }
    }

    /// Trigger an immediate cleanup of low-priority zone bundles.
    pub async fn cleanup(
        &self,
    ) -> Result<BTreeMap<Utf8PathBuf, CleanupCount>, BundleError> {
        let mut inner = self.inner.lock().await;
        let dirs = inner.bundle_directories().await;
        let res = run_cleanup(&self.log, &dirs, &inner.cleanup_context).await;
        inner.last_cleanup_at = Instant::now();
        self.notify_cleanup.notify_one();
        res
    }

    /// Return the utilization of the system for zone bundles.
    pub async fn utilization(
        &self,
    ) -> Result<BTreeMap<Utf8PathBuf, BundleUtilization>, BundleError> {
        let inner = self.inner.lock().await;
        let dirs = inner.bundle_directories().await;
        compute_bundle_utilization(&self.log, &dirs, &inner.cleanup_context)
            .await
    }

    /// Return the context used to periodically clean up zone bundles.
    pub async fn cleanup_context(&self) -> CleanupContext {
        self.inner.lock().await.cleanup_context
    }

    /// Update the context used to periodically clean up zone bundles.
    pub async fn update_cleanup_context(
        &self,
        new_period: Option<CleanupPeriod>,
        new_storage_limit: Option<StorageLimit>,
        new_priority: Option<PriorityOrder>,
    ) -> Result<(), BundleError> {
        let mut inner = self.inner.lock().await;
        info!(
            self.log,
            "received request to update cleanup context";
            "period" => ?new_period,
            "priority" => ?new_priority,
            "storage_limit" => ?new_storage_limit,
        );
        let mut notify_cleanup_task = false;
        if let Some(new_period) = new_period {
            if new_period < inner.cleanup_context.period {
                warn!(
                    self.log,
                    "auto cleanup period has been reduced, \
                    the cleanup task will be notified"
                );
                notify_cleanup_task = true;
            }
            inner.cleanup_context.period = new_period;
        }
        if let Some(new_priority) = new_priority {
            inner.cleanup_context.priority = new_priority;
        }
        if let Some(new_storage_limit) = new_storage_limit {
            if new_storage_limit < inner.cleanup_context.storage_limit {
                notify_cleanup_task = true;
                warn!(
                    self.log,
                    "storage limit has been lowered, a \
                    cleanup will be run immediately"
                );
            }
            inner.cleanup_context.storage_limit = new_storage_limit;
        }
        if notify_cleanup_task {
            self.notify_cleanup.notify_one();
        }
        Ok(())
    }

    /// Create a bundle from the provided zone.
    pub async fn create(
        &self,
        zone: &RunningZone,
        cause: ZoneBundleCause,
    ) -> Result<ZoneBundleMetadata, BundleError> {
        let inner = self.inner.lock().await;
        let storage_dirs = inner.bundle_directories().await;
        let extra_log_dirs = inner
            .resources
            .all_u2_mountpoints(sled_hardware::disk::U2_DEBUG_DATASET)
            .await
            .into_iter()
            .map(|p| p.join(zone.name()))
            .collect();
        let context = ZoneBundleContext { cause, storage_dirs, extra_log_dirs };
        info!(
            self.log,
            "creating zone bundle";
            "zone_name" => zone.name(),
            "context" => ?context,
        );
        create(&self.log, zone, &context).await
    }

    /// Return the paths for all bundles of the provided zone and ID.
    pub async fn bundle_paths(
        &self,
        name: &str,
        id: &Uuid,
    ) -> Result<Vec<Utf8PathBuf>, BundleError> {
        let inner = self.inner.lock().await;
        let dirs = inner.bundle_directories().await;
        get_zone_bundle_paths(&self.log, &dirs, name, id).await
    }

    /// List bundles for a zone with the provided name.
    pub async fn list_for_zone(
        &self,
        name: &str,
    ) -> Result<Vec<ZoneBundleMetadata>, BundleError> {
        // The zone bundles are replicated in several places, so we'll use a set
        // to collect them all, to avoid duplicating.
        let mut bundles = BTreeSet::new();
        let inner = self.inner.lock().await;
        let dirs = inner.bundle_directories().await;
        for dir in dirs.iter() {
            bundles.extend(
                list_bundles_for_zone(&self.log, &dir, name)
                    .await?
                    .into_iter()
                    .map(|(_path, bdl)| bdl),
            );
        }
        Ok(bundles.into_iter().collect())
    }

    /// List all zone bundles that match the provided filter, if any.
    ///
    /// The filter is a simple substring match -- any zone bundle with a zone
    /// name that contains the filter anywhere will match. If no filter is
    /// provided, all extant bundles will be listed.
    pub async fn list(
        &self,
        filter: Option<&str>,
    ) -> Result<Vec<ZoneBundleMetadata>, BundleError> {
        // The zone bundles are replicated in several places, so we'll use a set
        // to collect them all, to avoid duplicating.
        let mut bundles = BTreeSet::new();
        let inner = self.inner.lock().await;
        let dirs = inner.bundle_directories().await;
        for dir in dirs.iter() {
            let mut rd = tokio::fs::read_dir(dir).await.map_err(|err| {
                BundleError::ReadDirectory { directory: dir.to_owned(), err }
            })?;
            while let Some(entry) = rd.next_entry().await.map_err(|err| {
                BundleError::ReadDirectory { directory: dir.to_owned(), err }
            })? {
                let search_dir = Utf8PathBuf::try_from(entry.path())?;
                bundles.extend(
                    filter_zone_bundles(&self.log, &search_dir, |md| {
                        filter
                            .map(|filt| md.id.zone_name.contains(filt))
                            .unwrap_or(true)
                    })
                    .await?
                    .into_values(),
                );
            }
        }
        Ok(bundles.into_iter().collect())
    }
}

// Context for creating a bundle of a specified zone.
#[derive(Debug, Default)]
struct ZoneBundleContext {
    // The directories into which the zone bundles are written.
    storage_dirs: Vec<Utf8PathBuf>,
    // The reason or cause for creating a zone bundle.
    cause: ZoneBundleCause,
    // Extra directories searched for logfiles for the name zone.
    //
    // Logs are periodically archived out of their original location, and onto
    // one or more U.2 drives. This field is used to specify that archive
    // location, so that rotated logs for the zone's services may be found.
    extra_log_dirs: Vec<Utf8PathBuf>,
}

// The set of zone-wide commands, which don't require any details about the
// processes we've launched in the zone.
const ZONE_WIDE_COMMANDS: [&[&str]; 6] = [
    &["ptree"],
    &["uptime"],
    &["last"],
    &["who"],
    &["svcs", "-p"],
    &["netstat", "-an"],
];

// The name for zone bundle metadata files.
const ZONE_BUNDLE_METADATA_FILENAME: &str = "metadata.toml";

/// Errors related to managing service zone bundles.
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("I/O error running command '{cmd}'")]
    Command {
        cmd: String,
        #[source]
        err: std::io::Error,
    },

    #[error("I/O error creating directory '{directory}'")]
    CreateDirectory {
        directory: Utf8PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("I/O error opening bundle tarball '{path}'")]
    OpenBundleFile {
        path: Utf8PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("I/O error adding bundle tarball data to '{tarball_path}'")]
    AddBundleData {
        tarball_path: Utf8PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("I/O error reading bundle tarball data from '{path}'")]
    ReadBundleData {
        path: Utf8PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("I/O error copying bundle tarball from '{from}' to '{to}'")]
    CopyArchive {
        from: Utf8PathBuf,
        to: Utf8PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("I/O error reading directory '{directory}'")]
    ReadDirectory {
        directory: Utf8PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("I/O error fetching metadata for '{path}'")]
    Metadata {
        path: Utf8PathBuf,
        #[source]
        err: std::io::Error,
    },

    #[error("TOML serialization failure")]
    Serialization(#[from] toml::ser::Error),

    #[error("TOML deserialization failure")]
    Deserialization(#[from] toml::de::Error),

    #[error("No zone named '{name}' is available for bundling")]
    NoSuchZone { name: String },

    #[error("No storage available for bundles")]
    NoStorage,

    #[error("Failed to join zone bundling task")]
    Task(#[from] tokio::task::JoinError),

    #[error("Failed to create bundle: {0}")]
    BundleFailed(#[from] anyhow::Error),

    #[error("Zone error")]
    Zone(#[from] AdmError),

    #[error(transparent)]
    PathBuf(#[from] FromPathBufError),

    #[error("Zone '{name}' cannot currently be bundled")]
    Unavailable { name: String },

    #[error("Storage limit must be expressed as a percentage in (0, 100]")]
    InvalidStorageLimit,

    #[error(
        "Cleanup period must be between {min:?} and {max:?}, inclusive",
        min = CleanupPeriod::MIN,
        max = CleanupPeriod::MAX,
    )]
    InvalidCleanupPeriod,

    #[error(
        "Invalid priority ordering. Each element must appear exactly once."
    )]
    InvalidPriorityOrder,

    #[error("Cleanup failed")]
    Cleanup(#[source] anyhow::Error),
}

// Helper function to write an array of bytes into the tar archive, with
// the provided name.
fn insert_data<W: std::io::Write>(
    builder: &mut Builder<W>,
    name: &str,
    contents: &[u8],
) -> Result<(), BundleError> {
    let mtime = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .context("failed to compute mtime")?
        .as_secs();

    let mut hdr = Header::new_ustar();
    hdr.set_size(contents.len().try_into().unwrap());
    hdr.set_mode(0o444);
    hdr.set_mtime(mtime);
    hdr.set_entry_type(tar::EntryType::Regular);
    // NOTE: This internally sets the path and checksum.
    builder.append_data(&mut hdr, name, Cursor::new(contents)).map_err(|err| {
        BundleError::AddBundleData { tarball_path: name.into(), err }
    })
}

// Create a service bundle for the provided zone.
//
// This runs a series of debugging commands in the zone, to collect data about
// the state of the zone and any Oxide service processes running inside. The
// data is packaged into a tarball, and placed in the provided output
// directories.
async fn create(
    log: &Logger,
    zone: &RunningZone,
    context: &ZoneBundleContext,
) -> Result<ZoneBundleMetadata, BundleError> {
    // Fetch the directory into which we'll store data, and ensure it exists.
    if context.storage_dirs.is_empty() {
        warn!(log, "no directories available for zone bundles");
        return Err(BundleError::NoStorage);
    }
    let mut zone_bundle_dirs = Vec::with_capacity(context.storage_dirs.len());
    for dir in context.storage_dirs.iter() {
        let bundle_dir = dir.join(zone.name());
        debug!(log, "creating bundle directory"; "dir" => %bundle_dir);
        tokio::fs::create_dir_all(&bundle_dir).await.map_err(|err| {
            BundleError::CreateDirectory {
                directory: bundle_dir.to_owned(),
                err,
            }
        })?;
        zone_bundle_dirs.push(bundle_dir);
    }

    // Create metadata and the tarball writer.
    //
    // We'll write the contents of the bundle into a gzipped tar archive,
    // including metadata and a file for the output of each command we run in
    // the zone.
    let zone_metadata = ZoneBundleMetadata::new(zone.name(), context.cause);
    let filename = format!("{}.tar.gz", zone_metadata.id.bundle_id);
    let full_path = zone_bundle_dirs[0].join(&filename);
    let file = match tokio::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .open(&full_path)
        .await
    {
        Ok(f) => f.into_std().await,
        Err(e) => {
            error!(
                log,
                "failed to create bundle file";
                "zone" => zone.name(),
                "file" => %full_path,
                "error" => ?e,
            );
            return Err(BundleError::OpenBundleFile {
                path: full_path.to_owned(),
                err: e,
            });
        }
    };
    debug!(
        log,
        "created bundle tarball file";
        "zone" => zone.name(),
        "path" => %full_path
    );
    let gz = flate2::GzBuilder::new()
        .filename(filename.as_str())
        .write(file, flate2::Compression::best());
    let mut builder = Builder::new(gz);

    // Write the metadata file itself, in TOML format.
    let contents = toml::to_string(&zone_metadata)?;
    insert_data(
        &mut builder,
        ZONE_BUNDLE_METADATA_FILENAME,
        contents.as_bytes(),
    )?;
    debug!(
        log,
        "wrote zone bundle metadata";
        "zone" => zone.name(),
    );
    for cmd in ZONE_WIDE_COMMANDS {
        debug!(
            log,
            "running zone bundle command";
            "zone" => zone.name(),
            "command" => ?cmd,
        );
        let output = match zone.run_cmd(cmd) {
            Ok(s) => s,
            Err(e) => format!("{}", e),
        };
        let contents = format!("Command: {:?}\n{}", cmd, output).into_bytes();
        if let Err(e) = insert_data(&mut builder, cmd[0], &contents) {
            error!(
                log,
                "failed to save zone bundle command output";
                "zone" => zone.name(),
                "command" => ?cmd,
                "error" => ?e,
            );
        }
    }

    // Debugging commands run on the specific processes this zone defines.
    const ZONE_PROCESS_COMMANDS: [&str; 3] = [
        "pfiles", "pstack",
        "pargs",
        // TODO-completeness: We may want `gcore`, since that encompasses
        // the above commands and much more. It seems like overkill now,
        // however.
    ];
    let procs = match zone
        .service_processes()
        .context("failed to enumerate zone service processes")
    {
        Ok(p) => {
            debug!(
                log,
                "enumerated service processes";
                "zone" => zone.name(),
                "procs" => ?p,
            );
            p
        }
        Err(e) => {
            error!(
                log,
                "failed to enumerate zone service processes";
                "zone" => zone.name(),
                "error" => ?e,
            );
            return Err(BundleError::from(e));
        }
    };
    for svc in procs.into_iter() {
        let pid_s = svc.pid.to_string();
        for cmd in ZONE_PROCESS_COMMANDS {
            let args = &[cmd, &pid_s];
            debug!(
                log,
                "running zone bundle command";
                "zone" => zone.name(),
                "command" => ?args,
            );
            let output = match zone.run_cmd(args) {
                Ok(s) => s,
                Err(e) => format!("{}", e),
            };
            let contents =
                format!("Command: {:?}\n{}", args, output).into_bytes();

            // There may be multiple Oxide service processes for which we
            // want to capture the command output. Name each output after
            // the command and PID to disambiguate.
            let filename = format!("{}.{}", cmd, svc.pid);
            if let Err(e) = insert_data(&mut builder, &filename, &contents) {
                error!(
                    log,
                    "failed to save zone bundle command output";
                    "zone" => zone.name(),
                    "command" => ?args,
                    "error" => ?e,
                );
            }
        }

        // We may need to extract log files that have been archived out of the
        // zone filesystem itself. See `crate::dump_setup` for the logic which
        // does this.
        let archived_log_files = find_archived_log_files(
            log,
            zone.name(),
            &svc.service_name,
            &context.extra_log_dirs,
        )
        .await;

        // Copy any log files, current and rotated, into the tarball as
        // well.
        //
        // Safety: This pathbuf was retrieved by locating an existing file
        // on the filesystem, so we're sure it has a name and the unwrap is
        // safe.
        debug!(
            log,
            "appending current log file to zone bundle";
            "zone" => zone.name(),
            "log_file" => %svc.log_file,
        );
        if let Err(e) = builder.append_path_with_name(
            &svc.log_file,
            svc.log_file.file_name().unwrap(),
        ) {
            error!(
                log,
                "failed to append current log file to zone bundle";
                "zone" => zone.name(),
                "log_file" => %svc.log_file,
                "error" => ?e,
            );
            return Err(BundleError::AddBundleData {
                tarball_path: svc.log_file.file_name().unwrap().into(),
                err: e,
            });
        }
        for f in svc.rotated_log_files.iter().chain(archived_log_files.iter()) {
            debug!(
                log,
                "appending rotated log file to zone bundle";
                "zone" => zone.name(),
                "log_file" => %f,
            );
            if let Err(e) =
                builder.append_path_with_name(f, f.file_name().unwrap())
            {
                error!(
                    log,
                    "failed to append rotated log file to zone bundle";
                    "zone" => zone.name(),
                    "log_file" => %f,
                    "error" => ?e,
                );
                return Err(BundleError::AddBundleData {
                    tarball_path: f.file_name().unwrap().into(),
                    err: e,
                });
            }
        }
    }

    // Finish writing out the tarball itself.
    builder.into_inner().context("Failed to build bundle")?;

    // Copy the bundle to the other locations. We really want the bundles to
    // be duplicates, not an additional, new bundle.
    //
    // TODO-robustness: We should probably create the bundle in a temp dir, and
    // copy it to all the final locations. This would make it easier to cleanup
    // the final locations should that last copy fail for any of them.
    //
    // See: https://github.com/oxidecomputer/omicron/issues/3876.
    for other_dir in zone_bundle_dirs.iter().skip(1) {
        let to = other_dir.join(&filename);
        debug!(log, "copying bundle"; "from" => %full_path, "to" => %to);
        tokio::fs::copy(&full_path, &to).await.map_err(|err| {
            BundleError::CopyArchive { from: full_path.to_owned(), to, err }
        })?;
    }

    info!(log, "finished zone bundle"; "metadata" => ?zone_metadata);
    Ok(zone_metadata)
}

// Find log files for the specified zone / SMF service, which may have been
// archived out to a U.2 dataset.
//
// Note that errors are logged, rather than failing the whole function, so that
// one failed listing does not prevent collecting any other log files.
async fn find_archived_log_files(
    log: &Logger,
    zone_name: &str,
    svc_name: &str,
    dirs: &[Utf8PathBuf],
) -> Vec<Utf8PathBuf> {
    // The `dirs` should be things like
    // `/pool/ext/<ZPOOL_UUID>/crypt/debug/<ZONE_NAME>`, but it's really up to
    // the caller to verify these exist and possibly contain what they expect.
    //
    // Within that, we'll just look for things that appear to be Oxide-managed
    // SMF service log files.
    let mut files = Vec::new();
    for dir in dirs.iter() {
        if dir.exists() {
            let mut rd = match tokio::fs::read_dir(&dir).await {
                Ok(rd) => rd,
                Err(e) => {
                    error!(
                        log,
                        "failed to read zone debug directory";
                        "directory" => ?dir,
                        "reason" => ?e,
                    );
                    continue;
                }
            };
            loop {
                match rd.next_entry().await {
                    Ok(None) => break,
                    Ok(Some(entry)) => {
                        let Ok(path) = Utf8PathBuf::try_from(entry.path())
                        else {
                            error!(
                                log,
                                "skipping possible archived log file with \
                                non-UTF-8 path";
                                "path" => ?entry.path(),
                            );
                            continue;
                        };
                        let fname = path.file_name().unwrap();
                        let is_oxide = is_oxide_smf_log_file(fname);
                        let contains = fname.contains(svc_name);
                        if is_oxide && contains {
                            debug!(
                                log,
                                "found archived log file";
                                "zone_name" => zone_name,
                                "service_name" => svc_name,
                                "path" => ?path,
                            );
                            files.push(path);
                        } else {
                            debug!(
                                log,
                                "skipping non-matching log file";
                                "filename" => fname,
                                "is_oxide_smf_log_file" => is_oxide,
                                "contains_svc_name" => contains,
                            );
                        }
                    }
                    Err(e) => {
                        error!(
                            log,
                            "failed to fetch zone debug directory entry";
                            "directory" => ?dir,
                            "reason" => ?e,
                        );
                    }
                }
            }
        } else {
            // The logic in `dump_setup` picks some U.2 in which to start
            // archiving logs, and thereafter tries to keep placing new ones
            // there, subject to space constraints. It's not really an error for
            // there to be no entries for the named zone in any particular U.2
            // debug dataset.
            slog::trace!(
                log,
                "attempting to find archived log files in \
                non-existent directory";
                "directory" => ?dir,
            );
        }
    }
    files
}

// Extract the zone bundle metadata from a file, if it exists.
fn extract_zone_bundle_metadata_impl(
    path: &Utf8PathBuf,
) -> Result<ZoneBundleMetadata, BundleError> {
    // Build a reader for the whole archive.
    let reader = std::fs::File::open(path).map_err(|err| {
        BundleError::OpenBundleFile { path: path.clone(), err }
    })?;
    let buf_reader = std::io::BufReader::new(reader);
    let gz = GzDecoder::new(buf_reader);
    let mut archive = Archive::new(gz);

    // Find the metadata entry, if it exists.
    let entries = archive.entries().map_err(|err| {
        BundleError::ReadBundleData { path: path.clone(), err }
    })?;
    let Some(md_entry) = entries
        // The `Archive::entries` iterator
        // returns a result, so filter to those
        // that are OK first.
        .filter_map(Result::ok)
        .find(|entry| {
            entry
                .path()
                .map(|p| p.to_str() == Some(ZONE_BUNDLE_METADATA_FILENAME))
                .unwrap_or(false)
        })
    else {
        return Err(BundleError::from(anyhow!(
            "Zone bundle is missing metadata file"
        )));
    };

    // Extract its contents and parse as metadata.
    let contents = std::io::read_to_string(md_entry).map_err(|err| {
        BundleError::ReadBundleData { path: path.clone(), err }
    })?;
    toml::from_str(&contents).map_err(BundleError::from)
}

// List the extant zone bundles for the provided zone, in the provided
// directory.
async fn list_bundles_for_zone(
    log: &Logger,
    path: &Utf8Path,
    zone_name: &str,
) -> Result<Vec<(Utf8PathBuf, ZoneBundleMetadata)>, BundleError> {
    let zone_bundle_dir = path.join(zone_name);
    Ok(filter_zone_bundles(log, &zone_bundle_dir, |md| {
        md.id.zone_name == zone_name
    })
    .await?
    .into_iter()
    .collect::<Vec<_>>())
}

// Extract zone bundle metadata from the provided file, if possible.
async fn extract_zone_bundle_metadata(
    path: Utf8PathBuf,
) -> Result<ZoneBundleMetadata, BundleError> {
    let task = tokio::task::spawn_blocking(move || {
        extract_zone_bundle_metadata_impl(&path)
    });
    task.await?
}

// Find zone bundles in the provided directory, which match the filter function.
async fn filter_zone_bundles(
    log: &Logger,
    directory: &Utf8PathBuf,
    filter: impl Fn(&ZoneBundleMetadata) -> bool,
) -> Result<BTreeMap<Utf8PathBuf, ZoneBundleMetadata>, BundleError> {
    let mut out = BTreeMap::new();
    debug!(log, "searching directory for zone bundles"; "directory" => %directory);
    let mut rd = tokio::fs::read_dir(directory).await.map_err(|err| {
        BundleError::ReadDirectory { directory: directory.to_owned(), err }
    })?;
    while let Some(entry) = rd.next_entry().await.map_err(|err| {
        BundleError::ReadDirectory { directory: directory.to_owned(), err }
    })? {
        let path = Utf8PathBuf::try_from(entry.path())?;
        debug!(log, "checking path as zone bundle"; "path" => %path);
        match extract_zone_bundle_metadata(path.clone()).await {
            Ok(md) => {
                trace!(log, "extracted zone bundle metadata"; "metadata" => ?md);
                if filter(&md) {
                    trace!(log, "filter matches bundle metadata"; "metadata" => ?md);
                    out.insert(path, md);
                }
            }
            Err(e) => {
                warn!(
                    log,
                    "failed to extract zone bundle metadata, skipping";
                    "path" => %path,
                    "reason" => ?e,
                );
            }
        }
    }
    Ok(out)
}

// Get the paths to a zone bundle, if it exists.
//
// Zone bundles are replicated in multiple storage directories. This returns
// every path at which the bundle with the provided ID exists, in the same
// order as `directories`.
async fn get_zone_bundle_paths(
    log: &Logger,
    directories: &[Utf8PathBuf],
    zone_name: &str,
    id: &Uuid,
) -> Result<Vec<Utf8PathBuf>, BundleError> {
    let mut out = Vec::with_capacity(directories.len());
    for dir in directories {
        let mut rd = tokio::fs::read_dir(dir).await.map_err(|err| {
            BundleError::ReadDirectory { directory: dir.to_owned(), err }
        })?;
        while let Some(entry) = rd.next_entry().await.map_err(|err| {
            BundleError::ReadDirectory { directory: dir.to_owned(), err }
        })? {
            let search_dir = Utf8PathBuf::try_from(entry.path())?;
            out.extend(
                filter_zone_bundles(log, &search_dir, |md| {
                    md.id.zone_name == zone_name && md.id.bundle_id == *id
                })
                .await?
                .into_keys(),
            );
        }
    }
    Ok(out)
}

/// The portion of a debug dataset used for zone bundles.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, Serialize)]
pub struct BundleUtilization {
    /// The total dataset quota, in bytes.
    pub dataset_quota: u64,
    /// The total number of bytes available for zone bundles.
    ///
    /// This is `dataset_quota` multiplied by the context's storage limit.
    pub bytes_available: u64,
    /// Total bundle usage, in bytes.
    pub bytes_used: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct ZoneBundleInfo {
    // The raw metadata for the bundle
    metadata: ZoneBundleMetadata,
    // The full path to the bundle
    path: Utf8PathBuf,
    // The number of bytes consumed on disk by the bundle
    bytes: u64,
}

// Enumerate all zone bundles under the provided directory.
async fn enumerate_zone_bundles(
    log: &Logger,
    dirs: &[Utf8PathBuf],
) -> Result<BTreeMap<Utf8PathBuf, Vec<ZoneBundleInfo>>, BundleError> {
    let mut out = BTreeMap::new();

    // Each of these is a storage directory.
    //
    // We should have under here zone-names, followed by bundles within each of
    // those.
    for dir in dirs.iter() {
        let mut rd = tokio::fs::read_dir(dir).await.map_err(|err| {
            BundleError::ReadDirectory { directory: dir.to_owned(), err }
        })?;
        let mut info_by_dir = Vec::new();
        while let Some(zone_dir) = rd.next_entry().await.map_err(|err| {
            BundleError::ReadDirectory { directory: dir.to_owned(), err }
        })? {
            let mut zone_rd = tokio::fs::read_dir(zone_dir.path())
                .await
                .map_err(|err| BundleError::ReadDirectory {
                    directory: zone_dir.path().try_into().unwrap(),
                    err,
                })?;
            while let Some(maybe_bundle) =
                zone_rd.next_entry().await.map_err(|err| {
                    BundleError::ReadDirectory {
                        directory: zone_dir.path().try_into().unwrap(),
                        err,
                    }
                })?
            {
                // TODO-robustness: What do we do with files that do _not_
                // appear to be valid zone bundles.
                //
                // On the one hand, someone may have put something there
                // intentionally. On the other hand, that would be weird, and we
                // _also_ know that it's possible that IO errors happen while
                // creating the bundle that render it impossible to recover the
                // metadata. So it's plausible that we end up with a lot of
                // detritus here in that case.
                let path = Utf8PathBuf::try_from(maybe_bundle.path())?;
                if let Ok(metadata) =
                    extract_zone_bundle_metadata(path.clone()).await
                {
                    let info = ZoneBundleInfo {
                        metadata,
                        path: path.clone(),
                        bytes: maybe_bundle
                            .metadata()
                            .await
                            .map_err(|err| BundleError::Metadata { path, err })?
                            .len(),
                    };
                    info_by_dir.push(info);
                } else {
                    warn!(
                        log,
                        "found non-zone-bundle file in zone bundle directory";
                        "path" => %path,
                    );
                }
            }
        }
        out.insert(dir.clone(), info_by_dir);
    }
    Ok(out)
}

/// The count of bundles / bytes removed during a cleanup operation.
#[derive(Clone, Copy, Debug, Default, Deserialize, JsonSchema, Serialize)]
pub struct CleanupCount {
    /// The number of bundles removed.
    bundles: u64,
    /// The number of bytes removed.
    bytes: u64,
}

// Run a cleanup, removing old bundles according to the strategy.
//
// Return the number of bundles removed and the new usage.
async fn run_cleanup(
    log: &Logger,
    storage_dirs: &[Utf8PathBuf],
    context: &CleanupContext,
) -> Result<BTreeMap<Utf8PathBuf, CleanupCount>, BundleError> {
    // First, determine how much space we are allowed to use and have used.
    //
    // Let's avoid doing anything at all if we're still within the limits.
    let usages = compute_bundle_utilization(log, storage_dirs, context).await?;
    if usages.values().all(|usage| usage.bytes_used <= usage.bytes_available) {
        debug!(log, "all usages below storage limit, returning");
        return Ok(BTreeMap::new());
    }

    // There's some work to do, let's enumerate all the bundles.
    let bundles = enumerate_zone_bundles(log, &storage_dirs).await?;
    debug!(
        log,
        "enumerated {} zone bundles across {} directories",
        bundles.values().map(Vec::len).sum::<usize>(),
        bundles.len(),
    );

    // Remove bundles from each storage directory, until we fall below the
    // number of bytes we would like to use to satisfy the storage limit.
    let mut cleanup_counts = BTreeMap::new();
    for (dir, mut info) in bundles.into_iter() {
        debug!(
            log,
            "cleaning up bundles from directory";
            "directory" => dir.as_str()
        );
        let mut count = CleanupCount::default();

        // Sort all the bundles in the current directory, using the priority
        // described in `context.priority`.
        info.sort_by(|lhs, rhs| context.priority.compare_bundles(lhs, rhs));
        let current_usage = usages.get(&dir).unwrap();

        // Remove bundles until we fall below the threshold.
        let mut n_bytes = current_usage.bytes_used;
        for each in info.into_iter() {
            if n_bytes <= current_usage.bytes_available {
                break;
            }
            tokio::fs::remove_file(&each.path).await.map_err(|_| {
                BundleError::Cleanup(anyhow!("failed to remove bundle"))
            })?;
            trace!(log, "removed old zone bundle"; "info" => ?&each);
            n_bytes = n_bytes.saturating_sub(each.bytes);
            count.bundles += 1;
            count.bytes += each.bytes;
        }

        cleanup_counts.insert(dir, count);
    }
    info!(log, "finished bundle cleanup"; "cleanup_counts" => ?&cleanup_counts);
    Ok(cleanup_counts)
}

// Return the total utilization for all zone bundles.
async fn compute_bundle_utilization(
    log: &Logger,
    storage_dirs: &[Utf8PathBuf],
    context: &CleanupContext,
) -> Result<BTreeMap<Utf8PathBuf, BundleUtilization>, BundleError> {
    let mut out = BTreeMap::new();
    for dir in storage_dirs.iter() {
        debug!(log, "computing bundle usage"; "directory" => %dir);
        // Fetch the ZFS dataset quota.
        let dataset_quota = zfs_quota(dir).await?;
        debug!(log, "computed dataset quota"; "quota" => dataset_quota);

        // Compute the bytes available, using the provided storage limit.
        let bytes_available =
            context.storage_limit.bytes_available(dataset_quota);
        debug!(
            log,
            "computed bytes available";
            "storage_limit" => %context.storage_limit,
            "bytes_available" => bytes_available
        );

        // Compute the size of the actual storage directory.
        //
        // TODO-correctness: This takes into account the directories themselves,
        // and may be not quite what we want. But it is very easy and pretty
        // close.
        let bytes_used = disk_usage(dir).await?;
        debug!(log, "computed bytes used"; "bytes_used" => bytes_used);
        out.insert(
            dir.clone(),
            BundleUtilization { dataset_quota, bytes_available, bytes_used },
        );
    }
    Ok(out)
}

/// Context provided for the zone bundle cleanup task.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize,
)]
pub struct CleanupContext {
    /// The period on which automatic checks and cleanup is performed.
    pub period: CleanupPeriod,
    /// The limit on the dataset quota available for zone bundles.
    pub storage_limit: StorageLimit,
    /// The priority ordering for keeping old bundles.
    pub priority: PriorityOrder,
}

// Return the number of bytes occupied by the provided directory.
//
// This returns an error if:
//
// - The "du" command fails
// - Parsing stdout fails
// - Parsing the actual size as a u64 fails
async fn disk_usage(path: &Utf8PathBuf) -> Result<u64, BundleError> {
    // Each OS implements slightly different `du` options.
    //
    // Linux and illumos support the "apparent" size in bytes, though using
    // different options. macOS doesn't support bytes at all, and has a minimum
    // block size of 512.
    //
    // We'll suffer the lower resolution on macOS, and get higher resolution on
    // the others.
    cfg_if::cfg_if! {
        if #[cfg(target_os = "illumos")] {
            const BLOCK_SIZE: u64 = 1;
            const DU_ARG: &str = "-A";
        } else if #[cfg(target_os = "linux")] {
            const BLOCK_SIZE: u64 = 1;
            const DU_ARG: &str = "-b";
        } else if #[cfg(target_os = "macos")] {
            const BLOCK_SIZE: u64 = 512;
            const DU_ARG: &str = "-k";
        } else {
            compile_error!("unsupported target OS");
        }
    }
    const DU: &str = "/usr/bin/du";
    let args = &[DU_ARG, "-s", path.as_str()];
    let output = Command::new(DU).args(args).output().await.map_err(|err| {
        BundleError::Command { cmd: format!("{DU} {}", args.join(" ")), err }
    })?;
    let err = |msg: &str| {
        BundleError::Cleanup(anyhow!(
            "failed to fetch disk usage for {}: {}",
            path,
            msg,
        ))
    };
    if !output.status.success() {
        return Err(err("du command failed"));
    }
    let Ok(s) = std::str::from_utf8(&output.stdout) else {
        return Err(err("non-UTF8 stdout"));
    };
    let Some(line) = s.lines().next() else {
        return Err(err("no lines in du output"));
    };
    let Some(part) = line.trim().split_ascii_whitespace().next() else {
        return Err(err("no disk usage size computed in output"));
    };
    part.parse()
        .map(|x: u64| x.saturating_mul(BLOCK_SIZE))
        .map_err(|_| err("failed to parse du output"))
}

// Return the quota for a ZFS dataset, or the available size.
//
// This fails if:
//
// - The "zfs" command fails
// - Parsing stdout fails
// - Parsing the actual quota as a u64 fails
async fn zfs_quota(path: &Utf8PathBuf) -> Result<u64, BundleError> {
    let args = &["list", "-Hpo", "quota,avail", path.as_str()];
    let output =
        Command::new(ZFS).args(args).output().await.map_err(|err| {
            BundleError::Command {
                cmd: format!("{ZFS} {}", args.join(" ")),
                err,
            }
        })?;
    let err = |msg: &str| {
        BundleError::Cleanup(anyhow!(
            "failed to fetch ZFS quota for {}: {}",
            path,
            msg,
        ))
    };
    if !output.status.success() {
        return Err(err("zfs list command failed"));
    }
    let Ok(s) = std::str::from_utf8(&output.stdout) else {
        return Err(err("non-UTF8 stdout"));
    };
    let Some(line) = s.lines().next() else {
        return Err(err("no lines in zfs list output"));
    };
    let mut parts = line.split_ascii_whitespace();
    let quota = parts.next().ok_or_else(|| err("no quota part of line"))?;
    let avail = parts.next().ok_or_else(|| err("no avail part of line"))?;

    // Parse the available space, which is always defined.
    let avail = avail
        .trim()
        .parse()
        .map_err(|_| err("failed to parse available space"))?;

    // Quotas can be reported a few different ways.
    //
    // If the dataset is a volume (which should not happen, but we don't enforce
    // here), then this is a literal dash `-`. Something without a quota is
    // reported as `0`. Anything else is an integer.
    //
    // No quota is reported as `u64::MAX`.
    match quota.trim() {
        "-" | "0" => Ok(avail),
        x => x.parse().or(Ok(avail)),
    }
}

/// The limit on space allowed for zone bundles, as a percentage of the overall
/// dataset's quota.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    JsonSchema,
    PartialEq,
    PartialOrd,
    Serialize,
)]
pub struct StorageLimit(u8);

impl std::fmt::Display for StorageLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}%", self.as_u8())
    }
}

impl Default for StorageLimit {
    fn default() -> Self {
        StorageLimit(25)
    }
}

impl StorageLimit {
    /// Minimum percentage of dataset quota supported.
    pub const MIN: Self = Self(0);

    /// Maximum percentage of dataset quota supported.
    pub const MAX: Self = Self(50);

    /// Construct a new limit allowed for zone bundles.
    ///
    /// This should be expressed as a percentage, in the range (Self::MIN,
    /// Self::MAX].
    pub const fn new(percentage: u8) -> Result<Self, BundleError> {
        if percentage > Self::MIN.0 && percentage <= Self::MAX.0 {
            Ok(Self(percentage))
        } else {
            Err(BundleError::InvalidStorageLimit)
        }
    }

    /// Return the contained quota percentage.
    pub const fn as_u8(&self) -> u8 {
        self.0
    }

    // Compute the number of bytes available from a dataset quota, in bytes.
    const fn bytes_available(&self, dataset_quota: u64) -> u64 {
        (dataset_quota * self.as_u8() as u64) / 100
    }
}

/// A dimension along with bundles can be sorted, to determine priority.
#[derive(
    Clone,
    Copy,
    Debug,
    Deserialize,
    Eq,
    Hash,
    JsonSchema,
    Serialize,
    Ord,
    PartialEq,
    PartialOrd,
)]
#[serde(rename_all = "snake_case")]
pub enum PriorityDimension {
    /// Sorting by time, with older bundles with lower priority.
    Time,
    /// Sorting by the cause for creating the bundle.
    Cause,
    // TODO-completeness: Support zone or zone type (e.g., service vs instance)?
}

/// The priority order for bundles during cleanup.
///
/// Bundles are sorted along the dimensions in [`PriorityDimension`], with each
/// dimension appearing exactly once. During cleanup, lesser-priority bundles
/// are pruned first, to maintain the dataset quota. Note that bundles are
/// sorted by each dimension in the order in which they appear, with each
/// dimension having higher priority than the next.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
pub struct PriorityOrder([PriorityDimension; PriorityOrder::EXPECTED_SIZE]);

impl std::ops::Deref for PriorityOrder {
    type Target = [PriorityDimension; PriorityOrder::EXPECTED_SIZE];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Default for PriorityOrder {
    fn default() -> Self {
        Self::DEFAULT
    }
}

impl PriorityOrder {
    // NOTE: Must match the number of variants in `PriorityDimension`.
    const EXPECTED_SIZE: usize = 2;
    const DEFAULT: Self =
        Self([PriorityDimension::Cause, PriorityDimension::Time]);

    /// Construct a new priority order.
    ///
    /// This requires that each dimension appear exactly once.
    pub fn new(dims: &[PriorityDimension]) -> Result<Self, BundleError> {
        if dims.len() != Self::EXPECTED_SIZE {
            return Err(BundleError::InvalidPriorityOrder);
        }
        let mut seen = HashSet::new();
        for dim in dims.iter() {
            if !seen.insert(dim) {
                return Err(BundleError::InvalidPriorityOrder);
            }
        }
        Ok(Self(dims.try_into().unwrap()))
    }

    // Order zone bundle info according to the contained priority.
    //
    // We sort the info by each dimension, in the order in which it appears.
    // That means earlier dimensions have higher priority than later ones.
    fn compare_bundles(
        &self,
        lhs: &ZoneBundleInfo,
        rhs: &ZoneBundleInfo,
    ) -> Ordering {
        for dim in self.0.iter() {
            let ord = match dim {
                PriorityDimension::Cause => {
                    lhs.metadata.cause.cmp(&rhs.metadata.cause)
                }
                PriorityDimension::Time => {
                    lhs.metadata.time_created.cmp(&rhs.metadata.time_created)
                }
            };
            if matches!(ord, Ordering::Equal) {
                continue;
            }
            return ord;
        }
        Ordering::Equal
    }
}

/// A period on which bundles are automatically cleaned up.
#[derive(
    Clone, Copy, Deserialize, JsonSchema, PartialEq, PartialOrd, Serialize,
)]
pub struct CleanupPeriod(Duration);

impl Default for CleanupPeriod {
    fn default() -> Self {
        Self(Duration::from_secs(600))
    }
}

impl CleanupPeriod {
    /// The minimum supported cleanup period.
    pub const MIN: Self = Self(Duration::from_secs(60));

    /// The maximum supported cleanup period.
    pub const MAX: Self = Self(Duration::from_secs(60 * 60 * 24));

    /// Construct a new cleanup period, checking that it's valid.
    pub fn new(duration: Duration) -> Result<Self, BundleError> {
        if duration >= Self::MIN.as_duration()
            && duration <= Self::MAX.as_duration()
        {
            Ok(Self(duration))
        } else {
            Err(BundleError::InvalidCleanupPeriod)
        }
    }

    /// Return the period as a duration.
    pub const fn as_duration(&self) -> Duration {
        self.0
    }
}

impl TryFrom<Duration> for CleanupPeriod {
    type Error = BundleError;

    fn try_from(duration: Duration) -> Result<Self, Self::Error> {
        Self::new(duration)
    }
}

impl std::fmt::Debug for CleanupPeriod {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
mod tests {
    use super::disk_usage;
    use super::PriorityDimension;
    use super::PriorityOrder;
    use super::StorageLimit;
    use super::Utf8PathBuf;
    use super::ZoneBundleCause;
    use super::ZoneBundleId;
    use super::ZoneBundleInfo;
    use super::ZoneBundleMetadata;
    use chrono::TimeZone;
    use chrono::Utc;

    #[test]
    fn test_sort_zone_bundle_cause() {
        use ZoneBundleCause::*;
        let mut original =
            [ExplicitRequest, Other, TerminatedInstance, UnexpectedZone];
        let expected =
            [Other, UnexpectedZone, TerminatedInstance, ExplicitRequest];
        original.sort();
        assert_eq!(original, expected);
    }

    #[test]
    fn test_priority_dimension() {
        assert!(PriorityOrder::new(&[]).is_err());
        assert!(PriorityOrder::new(&[PriorityDimension::Cause]).is_err());
        assert!(PriorityOrder::new(&[
            PriorityDimension::Cause,
            PriorityDimension::Cause
        ])
        .is_err());
        assert!(PriorityOrder::new(&[
            PriorityDimension::Cause,
            PriorityDimension::Cause,
            PriorityDimension::Time
        ])
        .is_err());

        assert!(PriorityOrder::new(&[
            PriorityDimension::Cause,
            PriorityDimension::Time
        ])
        .is_ok());
        assert_eq!(
            PriorityOrder::new(&PriorityOrder::default().0).unwrap(),
            PriorityOrder::default()
        );
    }

    #[tokio::test]
    async fn test_disk_usage() {
        let path =
            Utf8PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/src"));
        let usage = disk_usage(&path).await.unwrap();
        // Run `du -As /path/to/omicron/sled-agent/src`, which currently shows this
        // directory is ~450 KiB.
        assert!(
            usage >= 1024 * 400,
            "sled-agent manifest directory disk usage not correct?"
        );
        let path = Utf8PathBuf::from("/some/nonexistent/path");
        assert!(disk_usage(&path).await.is_err());
    }

    #[test]
    fn test_storage_limit_bytes_available() {
        let pct = StorageLimit(1);
        assert_eq!(pct.bytes_available(100), 1);
        assert_eq!(pct.bytes_available(1000), 10);

        let pct = StorageLimit(100);
        assert_eq!(pct.bytes_available(100), 100);
        assert_eq!(pct.bytes_available(1000), 1000);

        let pct = StorageLimit(100);
        assert_eq!(pct.bytes_available(99), 99);

        let pct = StorageLimit(99);
        assert_eq!(pct.bytes_available(1), 0);

        // Test non-power of 10.
        let pct = StorageLimit(25);
        assert_eq!(pct.bytes_available(32768), 8192);
    }

    #[test]
    fn test_compare_bundles() {
        use PriorityDimension::*;
        let time_first = PriorityOrder([Time, Cause]);
        let cause_first = PriorityOrder([Cause, Time]);

        fn make_info(
            year: i32,
            month: u32,
            day: u32,
            cause: ZoneBundleCause,
        ) -> ZoneBundleInfo {
            ZoneBundleInfo {
                metadata: ZoneBundleMetadata {
                    id: ZoneBundleId {
                        zone_name: String::from("oxz_whatever"),
                        bundle_id: uuid::Uuid::new_v4(),
                    },
                    time_created: Utc
                        .with_ymd_and_hms(year, month, day, 0, 0, 0)
                        .single()
                        .unwrap(),
                    cause,
                    version: 0,
                },
                path: Utf8PathBuf::from("/some/path"),
                bytes: 0,
            }
        }

        let info = [
            make_info(2020, 1, 2, ZoneBundleCause::TerminatedInstance),
            make_info(2020, 1, 2, ZoneBundleCause::ExplicitRequest),
            make_info(2020, 1, 1, ZoneBundleCause::TerminatedInstance),
            make_info(2020, 1, 1, ZoneBundleCause::ExplicitRequest),
        ];

        let mut sorted = info.clone();
        sorted.sort_by(|lhs, rhs| time_first.compare_bundles(lhs, rhs));
        // Low -> high priority
        // [old/terminated, old/explicit, new/terminated, new/explicit]
        let expected = [
            info[2].clone(),
            info[3].clone(),
            info[0].clone(),
            info[1].clone(),
        ];
        assert_eq!(
            sorted, expected,
            "sorting zone bundles by time-then-cause failed"
        );

        let mut sorted = info.clone();
        sorted.sort_by(|lhs, rhs| cause_first.compare_bundles(lhs, rhs));
        // Low -> high priority
        // [old/terminated, new/terminated, old/explicit, new/explicit]
        let expected = [
            info[2].clone(),
            info[0].clone(),
            info[3].clone(),
            info[1].clone(),
        ];
        assert_eq!(
            sorted, expected,
            "sorting zone bundles by cause-then-time failed"
        );
    }
}

#[cfg(all(target_os = "illumos", test))]
mod illumos_tests {
    use super::find_archived_log_files;
    use super::zfs_quota;
    use super::CleanupContext;
    use super::CleanupPeriod;
    use super::PriorityOrder;
    use super::StorageLimit;
    use super::StorageResources;
    use super::Utf8Path;
    use super::Utf8PathBuf;
    use super::Uuid;
    use super::ZoneBundleCause;
    use super::ZoneBundleId;
    use super::ZoneBundleInfo;
    use super::ZoneBundleMetadata;
    use super::ZoneBundler;
    use super::ZFS;
    use anyhow::Context;
    use chrono::TimeZone;
    use chrono::Utc;
    use slog::Drain;
    use slog::Logger;
    use tokio::process::Command;

    #[tokio::test]
    async fn test_zfs_quota() {
        let path =
            Utf8PathBuf::try_from(std::env::current_dir().unwrap()).unwrap();
        let quota = zfs_quota(&path).await.unwrap();
        assert!(
            quota < (100 * 1024 * 1024 * 1024 * 1024),
            "100TiB should be enough for anyone",
        );
        let path = Utf8PathBuf::from("/some/nonexistent/path");
        assert!(zfs_quota(&path).await.is_err());
    }

    struct CleanupTestContext {
        resource_wrapper: ResourceWrapper,
        context: CleanupContext,
        bundler: ZoneBundler,
    }

    // A wrapper around `StorageResources`, that automatically creates dummy
    // directories in the provided test locations and removes them on drop.
    //
    // I'd much prefer this to be done in $TEMPDIR. However, `StorageResources`
    // is difficult to mock out or modify in such a way that the underlying
    // dataset locations can be controlled.
    //
    // This creates completely BS disks, and fake names for the zpools on them.
    // Those pools are _supposed_ to live at directories like:
    //
    // `/pool/int/<UUID>`
    //
    // They don't exist when you just do `StorageResources::new_for_test()`.
    // This type creates the datasets at the expected mountpoints, backed by the
    // ramdisk, and removes them on drop. This is basically a tempdir-like
    // system, that creates the directories implied by the `StorageResources`
    // expected disk structure.
    struct ResourceWrapper {
        resources: StorageResources,
        dirs: Vec<Utf8PathBuf>,
    }

    impl ResourceWrapper {
        // Create new storage resources, and mount fake datasets at the required
        // locations.
        async fn new() -> Self {
            let resources = StorageResources::new_for_test();
            let dirs = resources.all_zone_bundle_directories().await;
            for d in dirs.iter() {
                let id =
                    d.components().nth(3).unwrap().as_str().parse().unwrap();
                create_test_dataset(&id, d).await.unwrap();
            }
            Self { resources, dirs }
        }
    }

    impl Drop for ResourceWrapper {
        fn drop(&mut self) {
            for d in self.dirs.iter() {
                let id =
                    d.components().nth(3).unwrap().as_str().parse().unwrap();
                remove_test_dataset(&id).unwrap();
            }
        }
    }

    fn test_logger() -> Logger {
        let dec =
            slog_term::PlainSyncDecorator::new(slog_term::TestStdoutWriter);
        let drain = slog_term::FullFormat::new(dec).build().fuse();
        let log =
            Logger::root(drain, slog::o!("component" => "fake-cleanup-task"));
        log
    }

    async fn setup_fake_cleanup_task() -> anyhow::Result<CleanupTestContext> {
        let log = test_logger();
        let context = CleanupContext::default();
        let resource_wrapper = ResourceWrapper::new().await;
        let bundler =
            ZoneBundler::new(log, resource_wrapper.resources.clone(), context);
        Ok(CleanupTestContext { resource_wrapper, context, bundler })
    }

    #[tokio::test]
    async fn test_context() {
        let ctx = setup_fake_cleanup_task().await.unwrap();
        let context = ctx.bundler.cleanup_context().await;
        assert_eq!(context, ctx.context, "received incorrect context");
    }

    #[tokio::test]
    async fn test_update_context() {
        let ctx = setup_fake_cleanup_task().await.unwrap();
        let new_context = CleanupContext {
            period: CleanupPeriod::new(ctx.context.period.as_duration() / 2)
                .unwrap(),
            storage_limit: StorageLimit(ctx.context.storage_limit.as_u8() / 2),
            priority: PriorityOrder::new(
                &ctx.context.priority.iter().copied().rev().collect::<Vec<_>>(),
            )
            .unwrap(),
        };
        ctx.bundler
            .update_cleanup_context(
                Some(new_context.period),
                Some(new_context.storage_limit),
                Some(new_context.priority),
            )
            .await
            .expect("failed to set context");
        let context = ctx.bundler.cleanup_context().await;
        assert_eq!(context, new_context, "failed to update context");
    }

    // Quota applied to test datasets.
    //
    // This needs to be at least this big lest we get "out of space" errors when
    // creating. Not sure where those come from, but could be ZFS overhead.
    const TEST_QUOTA: u64 = 1024 * 32;

    async fn create_test_dataset(
        id: &Uuid,
        mountpoint: &Utf8PathBuf,
    ) -> anyhow::Result<()> {
        let output = Command::new("/usr/bin/pfexec")
            .arg(ZFS)
            .arg("create")
            .arg("-o")
            .arg(format!("quota={TEST_QUOTA}"))
            .arg("-o")
            .arg(format!("mountpoint={mountpoint}"))
            .arg(format!("rpool/{id}"))
            .output()
            .await
            .context("failed to spawn zfs create operation")?;
        anyhow::ensure!(
            output.status.success(),
            "zfs create operation failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );

        // Make the path operable by the test code.
        let output = Command::new("/usr/bin/pfexec")
            .arg("chmod")
            .arg("a+rw")
            .arg(&mountpoint)
            .output()
            .await
            .context("failed to spawn chmod operation")?;
        anyhow::ensure!(
            output.status.success(),
            "chmod-ing the dataset failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(())
    }

    fn remove_test_dataset(id: &Uuid) -> anyhow::Result<()> {
        let output = std::process::Command::new("/usr/bin/pfexec")
            .arg(ZFS)
            .arg("destroy")
            .arg(format!("rpool/{id}"))
            .output()
            .context("failed to spawn zfs destroy operation")?;
        anyhow::ensure!(
            output.status.success(),
            "zfs destroy operation failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );
        Ok(())
    }

    async fn run_test_with_zfs_dataset<T, Fut>(test: T)
    where
        T: FnOnce(CleanupTestContext) -> Fut,
        Fut: std::future::Future<Output = anyhow::Result<()>>,
    {
        let context = setup_fake_cleanup_task()
            .await
            .expect("failed to create cleanup task");
        let result = test(context).await;
        result.expect("test failed!");
    }

    #[tokio::test]
    async fn test_utilization() {
        run_test_with_zfs_dataset(test_utilization_body).await;
    }

    async fn test_utilization_body(
        ctx: CleanupTestContext,
    ) -> anyhow::Result<()> {
        let utilization = ctx.bundler.utilization().await?;
        let paths = utilization.keys().cloned().collect::<Vec<_>>();

        // Check that we've looked at all the paths in the context.
        anyhow::ensure!(
            paths == ctx.resource_wrapper.dirs,
            "Usage RPC returned incorrect paths"
        );

        // Check that we fetched the correct quota from the datasets.
        let bundle_utilization = utilization
            .values()
            .next()
            .context("no utilization information?")?;
        anyhow::ensure!(
            bundle_utilization.dataset_quota == TEST_QUOTA,
            "computed incorrect dataset quota"
        );

        // Check that the number of bytes available is accurate.
        let pct = u64::from(ctx.context.storage_limit.as_u8());
        let expected_bytes_available =
            (bundle_utilization.dataset_quota * pct) / 100;
        anyhow::ensure!(
            bundle_utilization.bytes_available == expected_bytes_available,
            "incorrect bytes available computed for storage: actual {}, expected {}",
            bundle_utilization.bytes_available,
            expected_bytes_available,
        );
        anyhow::ensure!(
            bundle_utilization.bytes_used < 64,
            "there should be basically zero bytes used"
        );

        // Now let's add a fake bundle, and make sure that we get the right size
        // back.
        let info = insert_fake_bundle(
            &paths[0],
            2020,
            1,
            1,
            ZoneBundleCause::ExplicitRequest,
        )
        .await?;

        let new_utilization = ctx.bundler.utilization().await?;
        anyhow::ensure!(
            paths == new_utilization.keys().cloned().collect::<Vec<_>>(),
            "paths should not change"
        );
        let new_bundle_utilization = new_utilization
            .values()
            .next()
            .context("no utilization information?")?;
        anyhow::ensure!(
            bundle_utilization.dataset_quota
                == new_bundle_utilization.dataset_quota,
            "dataset quota should not change"
        );
        anyhow::ensure!(
            bundle_utilization.bytes_available
                == new_bundle_utilization.bytes_available,
            "bytes available for bundling should not change",
        );

        // We should have consumed _some_ bytes, at least the size of the
        // tarball itself.
        let change =
            new_bundle_utilization.bytes_used - bundle_utilization.bytes_used;
        anyhow::ensure!(
            change > info.bytes,
            "bytes used should drop by at least the size of the tarball",
        );

        // This is a pretty weak test, but let's make sure that the actual
        // number of bytes we use is within 5% of the computed size of the
        // tarball in bytes. This should account for the directories containing
        // it.
        const THRESHOLD: f64 = 0.05;
        let used = new_bundle_utilization.bytes_used as f64;
        let size = info.bytes as f64;
        let change = (used - size) / used;
        anyhow::ensure!(
            change > 0.0 && change <= THRESHOLD,
            "bytes used should be positive and within {:02} of the \
            size of the new tarball, used = {}, tarball size = {}",
            THRESHOLD,
            used,
            size,
        );
        Ok(())
    }

    #[tokio::test]
    async fn test_cleanup() {
        run_test_with_zfs_dataset(test_cleanup_body).await;
    }

    async fn test_cleanup_body(ctx: CleanupTestContext) -> anyhow::Result<()> {
        // Let's add a bunch of fake bundles, until we should be over the
        // storage limit. These will all be explicit requests, so the priority
        // should be decided based on time, i.e., the ones first added should be
        // removed.
        //
        // First, reduce the storage limit, so that we only need to add a few
        // bundles.
        ctx.bundler
            .update_cleanup_context(None, Some(StorageLimit(2)), None)
            .await
            .context("failed to update cleanup context")?;

        let mut day = 1;
        let mut info = Vec::new();
        let mut utilization = ctx.bundler.utilization().await?;
        loop {
            let us = utilization
                .values()
                .next()
                .context("no utilization information")?;
            if us.bytes_used > us.bytes_available {
                break;
            }
            let it = insert_fake_bundle(
                &ctx.resource_wrapper.dirs[0],
                2020,
                1,
                day,
                ZoneBundleCause::ExplicitRequest,
            )
            .await?;
            day += 1;
            info.push(it);
            utilization = ctx.bundler.utilization().await?;
        }

        // Trigger a cleanup.
        let counts =
            ctx.bundler.cleanup().await.context("failed to run cleanup")?;

        // We should have cleaned up items in the same paths that we have in the
        // context.
        anyhow::ensure!(
            counts.keys().zip(ctx.resource_wrapper.dirs.iter()).all(|(a, b)| a == b),
            "cleaned-up directories do not match the context's storage directories",
        );

        // We should have cleaned up the first-inserted bundle.
        let count = counts.values().next().context("no cleanup counts")?;
        anyhow::ensure!(count.bundles == 1, "expected to cleanup one bundle");
        anyhow::ensure!(
            count.bytes == info[0].bytes,
            "expected to cleanup the number of bytes occupied by the first bundle",
        );
        let exists = tokio::fs::try_exists(&info[0].path)
            .await
            .context("failed to check if file exists")?;
        anyhow::ensure!(
            !exists,
            "the cleaned up bundle still appears to exist on-disk",
        );
        for each in info.iter().skip(1) {
            let exists = tokio::fs::try_exists(&each.path)
                .await
                .context("failed to check if file exists")?;
            anyhow::ensure!(exists, "cleaned up an unexpected bundle");
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_list_with_filter() {
        run_test_with_zfs_dataset(test_list_with_filter_body).await;
    }

    async fn test_list_with_filter_body(
        ctx: CleanupTestContext,
    ) -> anyhow::Result<()> {
        let mut day = 1;
        let mut info = Vec::new();
        const N_BUNDLES: usize = 3;
        for i in 0..N_BUNDLES {
            let it = insert_fake_bundle_with_zone_name(
                &ctx.resource_wrapper.dirs[0],
                2020,
                1,
                day,
                ZoneBundleCause::ExplicitRequest,
                format!("oxz_whatever_{i}").as_str(),
            )
            .await?;
            day += 1;
            info.push(it);
        }

        // Listing with no filter should return all of them.
        let all_md = ctx.bundler.list(None).await?;
        anyhow::ensure!(
            all_md
                == info
                    .iter()
                    .map(|each| each.metadata.clone())
                    .collect::<Vec<_>>(),
            "Expected listing with no filter to return all bundles"
        );

        // Each bundle is from a zone named like `oxz_whatver_<INDEX>`.
        //
        // So filters like `oxz_` should return all of them, while ones on the
        // index should return exactly that one matching.
        let filt = Some("oxz_");
        let all_md = ctx.bundler.list(filt).await?;
        anyhow::ensure!(
            all_md
                == info
                    .iter()
                    .map(|each| each.metadata.clone())
                    .collect::<Vec<_>>(),
            "Expected listing with simple to return all bundles"
        );
        for i in 0..N_BUNDLES {
            let filt = Some(i.to_string());
            let matching_md = ctx.bundler.list(filt.as_deref()).await?;
            let expected_md = &info[i].metadata;
            anyhow::ensure!(
                matching_md.len() == 1,
                "expected exactly one bundle"
            );
            anyhow::ensure!(
                &matching_md[0] == expected_md,
                "Matched incorrect zone bundle with a filter",
            );
        }
        Ok(())
    }

    async fn insert_fake_bundle(
        dir: &Utf8Path,
        year: i32,
        month: u32,
        day: u32,
        cause: ZoneBundleCause,
    ) -> anyhow::Result<ZoneBundleInfo> {
        insert_fake_bundle_with_zone_name(
            dir,
            year,
            month,
            day,
            cause,
            "oxz_whatever",
        )
        .await
    }

    async fn insert_fake_bundle_with_zone_name(
        dir: &Utf8Path,
        year: i32,
        month: u32,
        day: u32,
        cause: ZoneBundleCause,
        zone_name: &str,
    ) -> anyhow::Result<ZoneBundleInfo> {
        let metadata = ZoneBundleMetadata {
            id: ZoneBundleId {
                zone_name: String::from(zone_name),
                bundle_id: uuid::Uuid::new_v4(),
            },
            time_created: Utc
                .with_ymd_and_hms(year, month, day, 0, 0, 0)
                .single()
                .context("invalid year/month/day")?,
            cause,
            version: 0,
        };

        let zone_dir = dir.join(&metadata.id.zone_name);
        tokio::fs::create_dir_all(&zone_dir)
            .await
            .context("failed to create zone directory")?;
        let path = zone_dir.join(format!("{}.tar.gz", metadata.id.bundle_id));

        // Create a tarball at the path with this fake metadata.
        let file = tokio::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(&path)
            .await
            .context("failed to open zone bundle path")?
            .into_std()
            .await;
        let gz = flate2::GzBuilder::new()
            .filename(path.as_str())
            .write(file, flate2::Compression::best());
        let mut builder = tar::Builder::new(gz);
        let contents = toml::to_string(&metadata)?;
        super::insert_data(
            &mut builder,
            super::ZONE_BUNDLE_METADATA_FILENAME,
            contents.as_bytes(),
        )?;
        let _ = builder.into_inner().context("failed to finish tarball")?;
        let bytes = tokio::fs::metadata(&path).await?.len();
        Ok(ZoneBundleInfo { metadata, path, bytes })
    }

    #[tokio::test]
    async fn test_find_archived_log_files() {
        let log = test_logger();
        let tmpdir = tempfile::tempdir().expect("Failed to make tempdir");

        let mut should_match = [
            "oxide-foo:default.log",
            "oxide-foo:default.log.1000",
            "system-illumos-foo:default.log",
            "system-illumos-foo:default.log.100",
        ];
        let should_not_match = [
            "oxide-foo:default",
            "not-oxide-foo:default.log.1000",
            "system-illumos-foo",
            "not-system-illumos-foo:default.log.100",
        ];
        for name in should_match.iter().chain(should_not_match.iter()) {
            let path = tmpdir.path().join(name);
            tokio::fs::File::create(path)
                .await
                .expect("failed to create dummy file");
        }

        let path =
            Utf8PathBuf::try_from(tmpdir.path().as_os_str().to_str().unwrap())
                .unwrap();
        let mut files = find_archived_log_files(
            &log,
            "zone-name", // unused here, for logging only
            "foo",
            &[path],
        )
        .await;

        // Sort everything to compare correctly.
        should_match.sort();
        files.sort();
        assert_eq!(files.len(), should_match.len());
        assert!(files
            .iter()
            .zip(should_match.iter())
            .all(|(file, name)| { file.file_name().unwrap() == *name }));
    }
}
