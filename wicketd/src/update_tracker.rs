// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// Copyright 2023 Oxide Computer Company

use crate::artifacts::ArtifactIdData;
use crate::artifacts::UpdatePlan;
use crate::artifacts::WicketdArtifactStore;
use crate::helpers::sps_to_string;
use crate::http_entrypoints::GetArtifactsAndEventReportsResponse;
use crate::http_entrypoints::StartUpdateOptions;
use crate::http_entrypoints::UpdateSimulatedResult;
use crate::installinator_progress::IprStartReceiver;
use crate::installinator_progress::IprUpdateTracker;
use crate::mgs::make_mgs_client;
use anyhow::anyhow;
use anyhow::bail;
use anyhow::ensure;
use anyhow::Context;
use display_error_chain::DisplayErrorChain;
use dropshot::HttpError;
use gateway_client::types::HostPhase2Progress;
use gateway_client::types::HostPhase2RecoveryImageId;
use gateway_client::types::HostStartupOptions;
use gateway_client::types::InstallinatorImageId;
use gateway_client::types::PowerState;
use gateway_client::types::SpComponentFirmwareSlot;
use gateway_client::types::SpIdentifier;
use gateway_client::types::SpType;
use gateway_client::types::SpUpdateStatus;
use gateway_messages::SpComponent;
use installinator_common::InstallinatorCompletionMetadata;
use installinator_common::InstallinatorSpec;
use installinator_common::M2Slot;
use installinator_common::WriteOutput;
use omicron_common::api::external::SemverVersion;
use omicron_common::backoff;
use omicron_common::update::ArtifactHash;
use slog::error;
use slog::info;
use slog::o;
use slog::warn;
use slog::Logger;
use std::collections::btree_map::Entry;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::io;
use std::net::SocketAddrV6;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;
use std::time::Instant;
use thiserror::Error;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use update_engine::events::ProgressUnits;
use update_engine::AbortHandle;
use update_engine::StepSpec;
use uuid::Uuid;
use wicket_common::update_events::ComponentRegistrar;
use wicket_common::update_events::EventBuffer;
use wicket_common::update_events::EventReport;
use wicket_common::update_events::SharedStepHandle;
use wicket_common::update_events::SpComponentUpdateSpec;
use wicket_common::update_events::SpComponentUpdateStage;
use wicket_common::update_events::SpComponentUpdateStepId;
use wicket_common::update_events::SpComponentUpdateTerminalError;
use wicket_common::update_events::StepContext;
use wicket_common::update_events::StepHandle;
use wicket_common::update_events::StepProgress;
use wicket_common::update_events::StepResult;
use wicket_common::update_events::StepSkipped;
use wicket_common::update_events::StepSuccess;
use wicket_common::update_events::StepWarning;
use wicket_common::update_events::TestStepComponent;
use wicket_common::update_events::TestStepId;
use wicket_common::update_events::TestStepSpec;
use wicket_common::update_events::UpdateComponent;
use wicket_common::update_events::UpdateEngine;
use wicket_common::update_events::UpdateStepId;
use wicket_common::update_events::UpdateTerminalError;

#[derive(Debug)]
struct SpUpdateData {
    task: JoinHandle<()>,
    abort_handle: AbortHandle,
    // Note: Our mutex here is a standard mutex, not a tokio mutex. We generally
    // hold it only log enough to update its state or push a new update event
    // into its running log; occasionally we hold it long enough to clone it.
    event_buffer: Arc<StdMutex<EventBuffer>>,
}

#[derive(Debug)]
struct UploadTrampolinePhase2ToMgsStatus {
    hash: ArtifactHash,
    // The upload task retries forever until it succeeds, so we don't need to
    // keep a "tried but failed" variant here; we just need to know the ID of
    // the uploaded image once it's done.
    uploaded_image_id: Option<HostPhase2RecoveryImageId>,
}

#[derive(Debug)]
struct UploadTrampolinePhase2ToMgs {
    // The tuple is the ID of the Trampoline image and a boolean for whether or
    // not it is complete. The upload task retries forever until it succeeds, so
    // we don't need to keep a "tried but failed" variant here.
    status: watch::Receiver<UploadTrampolinePhase2ToMgsStatus>,
    task: JoinHandle<()>,
}

#[derive(Debug)]
pub struct UpdateTracker {
    mgs_client: gateway_client::Client,
    sp_update_data: Mutex<UpdateTrackerData>,

    // Every sled update via trampoline requires MGS to serve the trampoline
    // phase 2 image to the sled's SP over the management network; however, that
    // doesn't mean we should upload the trampoline image to MGS for every sled
    // update - it's always the same (for any given update plan). Therefore, we
    // separate the status of uploading the trampoline phase 2 MGS from the
    // status of individual SP updates: we'll start this upload the first time a
    // sled update starts that uses it, and any update (including that one or
    // any future sled updates) will pause at the appropriate time (if needed)
    // to wait for the upload to complete.
    upload_trampoline_phase_2_to_mgs:
        Mutex<Option<UploadTrampolinePhase2ToMgs>>,

    log: Logger,
    ipr_update_tracker: IprUpdateTracker,
}

impl UpdateTracker {
    pub(crate) fn new(
        mgs_addr: SocketAddrV6,
        log: &Logger,
        artifact_store: WicketdArtifactStore,
        ipr_update_tracker: IprUpdateTracker,
    ) -> Self {
        let log = log.new(o!("component" => "wicketd update planner"));
        let sp_update_data = Mutex::new(UpdateTrackerData::new(artifact_store));
        let mgs_client = make_mgs_client(log.clone(), mgs_addr);
        let upload_trampoline_phase_2_to_mgs = Mutex::default();

        Self {
            mgs_client,
            sp_update_data,
            log,
            upload_trampoline_phase_2_to_mgs,
            ipr_update_tracker,
        }
    }

    pub(crate) async fn start(
        &self,
        sps: BTreeSet<SpIdentifier>,
        opts: StartUpdateOptions,
    ) -> Result<(), Vec<StartUpdateError>> {
        let imp = RealSpawnUpdateDriver { update_tracker: self, opts };
        self.start_impl(sps, Some(imp)).await
    }

    /// Starts a fake update that doesn't perform any steps, but simply waits
    /// for a watch receiver to resolve.
    #[doc(hidden)]
    pub async fn start_fake_update(
        &self,
        sps: BTreeSet<SpIdentifier>,
        watch_receiver: watch::Receiver<()>,
    ) -> Result<(), Vec<StartUpdateError>> {
        let imp = FakeUpdateDriver { watch_receiver, log: self.log.clone() };
        self.start_impl(sps, Some(imp)).await
    }

    pub(crate) async fn clear_update_state(
        &self,
        sp: SpIdentifier,
    ) -> Result<(), ClearUpdateStateError> {
        let mut update_data = self.sp_update_data.lock().await;
        update_data.clear_update_state(sp)
    }

    pub(crate) async fn abort_update(
        &self,
        sp: SpIdentifier,
        message: String,
    ) -> Result<(), AbortUpdateError> {
        let mut update_data = self.sp_update_data.lock().await;
        update_data.abort_update(sp, message).await
    }

    /// Checks whether an update can be started for the given SPs, without
    /// actually starting it.
    ///
    /// This should only be used in situations where starting the update is not
    /// desired (for example, if we've already encountered errors earlier in the
    /// process and we're just adding to the list of errors). In cases where the
    /// start method *is* desired, prefer the [`Self::start`] method, which also
    /// performs the same checks.
    pub(crate) async fn update_pre_checks(
        &self,
        sps: BTreeSet<SpIdentifier>,
    ) -> Result<(), Vec<StartUpdateError>> {
        self.start_impl::<NeverUpdateDriver>(sps, None).await
    }

    async fn start_impl<Spawn>(
        &self,
        sps: BTreeSet<SpIdentifier>,
        spawn_update_driver: Option<Spawn>,
    ) -> Result<(), Vec<StartUpdateError>>
    where
        Spawn: SpawnUpdateDriver,
    {
        let mut update_data = self.sp_update_data.lock().await;

        let mut errors = Vec::new();

        // Check that we're not already updating any of these SPs.
        let update_in_progress: Vec<_> = sps
            .iter()
            .filter(|sp| {
                // If we don't have any update data for this SP, it's not in
                // progress.
                //
                // If we do, it's in progress if the task is not finished.
                update_data
                    .sp_update_data
                    .get(sp)
                    .map_or(false, |data| !data.task.is_finished())
            })
            .copied()
            .collect();

        if !update_in_progress.is_empty() {
            errors.push(StartUpdateError::UpdateInProgress(update_in_progress));
        }

        let plan = update_data.artifact_store.current_plan();
        if plan.is_none() {
            // (1), referred to below.
            errors.push(StartUpdateError::TufRepositoryUnavailable);
        }

        // If there are any errors, return now.
        if !errors.is_empty() {
            return Err(errors);
        }

        let plan =
            plan.expect("we'd have returned an error at (1) if plan was None");

        // Call the setup method now.
        if let Some(mut spawn_update_driver) = spawn_update_driver {
            let setup_data = spawn_update_driver.setup(&plan).await;

            for sp in sps {
                match update_data.sp_update_data.entry(sp) {
                    // Vacant: this is the first time we've started an update to this
                    // sp.
                    Entry::Vacant(slot) => {
                        slot.insert(
                            spawn_update_driver
                                .spawn_update_driver(
                                    sp,
                                    plan.clone(),
                                    &setup_data,
                                )
                                .await,
                        );
                    }
                    // Occupied: we've previously started an update to this sp.
                    Entry::Occupied(mut slot) => {
                        assert!(
                            slot.get().task.is_finished(),
                            "we just checked that the task was finished"
                        );
                        slot.insert(
                            spawn_update_driver
                                .spawn_update_driver(
                                    sp,
                                    plan.clone(),
                                    &setup_data,
                                )
                                .await,
                        );
                    }
                }
            }
        }

        Ok(())
    }

    fn spawn_upload_trampoline_phase_2_to_mgs(
        &self,
        plan: &UpdatePlan,
    ) -> UploadTrampolinePhase2ToMgs {
        let artifact = plan.trampoline_phase_2.clone();
        let (status_tx, status_rx) =
            watch::channel(UploadTrampolinePhase2ToMgsStatus {
                hash: artifact.data.hash(),
                uploaded_image_id: None,
            });
        let task = tokio::spawn(upload_trampoline_phase_2_to_mgs(
            self.mgs_client.clone(),
            artifact,
            status_tx,
            self.log.clone(),
        ));
        UploadTrampolinePhase2ToMgs { status: status_rx, task }
    }

    /// Updates the repository stored inside the update tracker.
    pub(crate) async fn put_repository<T>(
        &self,
        data: T,
    ) -> Result<(), HttpError>
    where
        T: io::Read + io::Seek + Send + 'static,
    {
        let mut update_data = self.sp_update_data.lock().await;
        update_data.put_repository(data).await
    }

    /// Gets a list of artifacts stored in the update repository.
    pub(crate) async fn artifacts_and_event_reports(
        &self,
    ) -> GetArtifactsAndEventReportsResponse {
        let update_data = self.sp_update_data.lock().await;

        let (system_version, artifacts) = match update_data
            .artifact_store
            .system_version_and_artifact_ids()
        {
            Some((system_version, artifacts)) => {
                (Some(system_version), artifacts)
            }
            None => (None, Vec::new()),
        };

        let mut event_reports = BTreeMap::new();
        for (sp, update_data) in &update_data.sp_update_data {
            let event_report =
                update_data.event_buffer.lock().unwrap().generate_report();
            let inner: &mut BTreeMap<_, _> =
                event_reports.entry(sp.type_).or_default();
            inner.insert(sp.slot, event_report);
        }

        GetArtifactsAndEventReportsResponse {
            system_version,
            artifacts,
            event_reports,
        }
    }

    pub(crate) async fn event_report(&self, sp: SpIdentifier) -> EventReport {
        let mut update_data = self.sp_update_data.lock().await;
        match update_data.sp_update_data.entry(sp) {
            Entry::Vacant(_) => EventReport::default(),
            Entry::Occupied(slot) => {
                slot.get().event_buffer.lock().unwrap().generate_report()
            }
        }
    }
}

/// A trait that represents a backend implementation for spawning the update
/// driver.
#[async_trait::async_trait]
trait SpawnUpdateDriver {
    /// The type returned by the [`Self::setup`] method. This is passed in by
    /// reference to [`Self::spawn_update_driver`].
    type Setup;

    /// Perform setup required to spawn the update driver.
    ///
    /// This is called *once*, before any calls to
    /// [`Self::spawn_update_driver`].
    async fn setup(&mut self, plan: &UpdatePlan) -> Self::Setup;

    /// Spawn the update driver for the given SP.
    ///
    /// This is called once per SP.
    async fn spawn_update_driver(
        &mut self,
        sp: SpIdentifier,
        plan: UpdatePlan,
        setup_data: &Self::Setup,
    ) -> SpUpdateData;
}

/// The production implementation of [`SpawnUpdateDriver`].
///
/// This implementation spawns real update drivers.
#[derive(Debug)]
struct RealSpawnUpdateDriver<'tr> {
    update_tracker: &'tr UpdateTracker,
    opts: StartUpdateOptions,
}

#[async_trait::async_trait]
impl<'tr> SpawnUpdateDriver for RealSpawnUpdateDriver<'tr> {
    type Setup = watch::Receiver<UploadTrampolinePhase2ToMgsStatus>;

    async fn setup(&mut self, plan: &UpdatePlan) -> Self::Setup {
        // Do we need to upload this plan's trampoline phase 2 to MGS?

        let mut upload_trampoline_phase_2_to_mgs =
            self.update_tracker.upload_trampoline_phase_2_to_mgs.lock().await;

        match upload_trampoline_phase_2_to_mgs.as_mut() {
            Some(prev) => {
                // We've previously started an upload - does it match
                // this artifact? If not, cancel the old task (which
                // might still be trying to upload) and start a new one
                // with our current image.
                if prev.status.borrow().hash
                    != plan.trampoline_phase_2.data.hash()
                {
                    // It does _not_ match - we have a new plan with a
                    // different trampoline image. If the old task is
                    // still running, cancel it, and start a new one.
                    prev.task.abort();
                    *prev = self
                        .update_tracker
                        .spawn_upload_trampoline_phase_2_to_mgs(&plan);
                }
            }
            None => {
                *upload_trampoline_phase_2_to_mgs = Some(
                    self.update_tracker
                        .spawn_upload_trampoline_phase_2_to_mgs(&plan),
                );
            }
        }

        // Both branches above leave `upload_trampoline_phase_2_to_mgs`
        // with data, so we can unwrap here to clone the `watch`
        // channel.
        upload_trampoline_phase_2_to_mgs.as_ref().unwrap().status.clone()
    }

    async fn spawn_update_driver(
        &mut self,
        sp: SpIdentifier,
        plan: UpdatePlan,
        setup_data: &Self::Setup,
    ) -> SpUpdateData {
        // Generate an ID for this update; the update tracker will send it to the
        // sled as part of the InstallinatorImageId, and installinator will send it
        // back to our artifact server with its progress reports.
        let update_id = Uuid::new_v4();

        let event_buffer = Arc::new(StdMutex::new(EventBuffer::new(16)));
        let ipr_start_receiver =
            self.update_tracker.ipr_update_tracker.register(update_id);

        let update_cx = UpdateContext {
            update_id,
            sp,
            mgs_client: self.update_tracker.mgs_client.clone(),
            upload_trampoline_phase_2_to_mgs: setup_data.clone(),
            log: self.update_tracker.log.new(o!(
                "sp" => format!("{sp:?}"),
                "update_id" => update_id.to_string(),
            )),
        };
        // TODO do we need `UpdateDriver` as a distinct type?
        let update_driver = UpdateDriver {};

        // Using a oneshot channel to communicate the abort handle isn't
        // ideal, but it works and is the easiest way to send it without
        // restructuring this code.
        let (abort_handle_sender, abort_handle_receiver) = oneshot::channel();
        let task = tokio::spawn(update_driver.run(
            plan,
            update_cx,
            event_buffer.clone(),
            ipr_start_receiver,
            self.opts.clone(),
            abort_handle_sender,
        ));

        let abort_handle = abort_handle_receiver
            .await
            .expect("abort handle is sent immediately");

        SpUpdateData { task, abort_handle, event_buffer }
    }
}

/// A fake implementation of [`SpawnUpdateDriver`].
///
/// This implementation is only used by tests. It contains a single step that
/// waits for a [`watch::Receiver`] to resolve.
#[derive(Debug)]
struct FakeUpdateDriver {
    watch_receiver: watch::Receiver<()>,
    log: Logger,
}

#[async_trait::async_trait]
impl SpawnUpdateDriver for FakeUpdateDriver {
    type Setup = ();

    async fn setup(&mut self, _plan: &UpdatePlan) -> Self::Setup {}

    async fn spawn_update_driver(
        &mut self,
        _sp: SpIdentifier,
        _plan: UpdatePlan,
        _setup_data: &Self::Setup,
    ) -> SpUpdateData {
        let (sender, mut receiver) = mpsc::channel(128);
        let event_buffer = Arc::new(StdMutex::new(EventBuffer::new(16)));
        let event_buffer_2 = event_buffer.clone();
        let log = self.log.clone();

        let engine = UpdateEngine::new(&log, sender);
        let abort_handle = engine.abort_handle();

        let mut watch_receiver = self.watch_receiver.clone();

        let task = tokio::spawn(async move {
            // The step component and ID have been chosen arbitrarily here --
            // they aren't important.
            engine
                .new_step(
                    UpdateComponent::Host,
                    UpdateStepId::RunningInstallinator,
                    "Fake step that waits for receiver to resolve",
                    move |_cx| async move {
                        // This will resolve as soon as the watch sender
                        // (typically a test) sends a value over the watch
                        // channel.
                        _ = watch_receiver.changed().await;
                        StepSuccess::new(()).into()
                    },
                )
                .register();

            // Spawn a task to accept all events from the executing engine.
            let event_receiving_task = tokio::spawn(async move {
                while let Some(event) = receiver.recv().await {
                    event_buffer_2.lock().unwrap().add_event(event);
                }
            });

            match engine.execute().await {
                Ok(_cx) => (),
                Err(err) => {
                    error!(log, "update failed"; "err" => %err);
                }
            }

            // Wait for all events to be received and written to the event
            // buffer.
            event_receiving_task.await.expect("event receiving task panicked");
        });

        SpUpdateData { task, abort_handle, event_buffer }
    }
}

/// An implementation of [`SpawnUpdateDriver`] that cannot be constructed.
///
/// This is an uninhabited type (an empty enum), and is only used to provide a
/// type parameter for the [`UpdateTracker::update_pre_checks`] method.
enum NeverUpdateDriver {}

#[async_trait::async_trait]
impl SpawnUpdateDriver for NeverUpdateDriver {
    type Setup = ();

    async fn setup(&mut self, _plan: &UpdatePlan) -> Self::Setup {}

    async fn spawn_update_driver(
        &mut self,
        _sp: SpIdentifier,
        _plan: UpdatePlan,
        _setup_data: &Self::Setup,
    ) -> SpUpdateData {
        unreachable!("this update driver cannot be constructed")
    }
}

#[derive(Debug)]
struct UpdateTrackerData {
    artifact_store: WicketdArtifactStore,
    sp_update_data: BTreeMap<SpIdentifier, SpUpdateData>,
}

impl UpdateTrackerData {
    fn new(artifact_store: WicketdArtifactStore) -> Self {
        Self { artifact_store, sp_update_data: BTreeMap::new() }
    }

    fn clear_update_state(
        &mut self,
        sp: SpIdentifier,
    ) -> Result<(), ClearUpdateStateError> {
        // Is an update currently running? If so, then reject the request.
        let is_running = self
            .sp_update_data
            .get(&sp)
            .map_or(false, |update_data| !update_data.task.is_finished());
        if is_running {
            return Err(ClearUpdateStateError::UpdateInProgress);
        }

        self.sp_update_data.remove(&sp);
        Ok(())
    }

    async fn abort_update(
        &mut self,
        sp: SpIdentifier,
        message: String,
    ) -> Result<(), AbortUpdateError> {
        let Some(update_data) = self.sp_update_data.get(&sp) else {
            return Err(AbortUpdateError::UpdateNotStarted);
        };

        // We can only abort an update if it is still running.
        //
        // There's a race possible here between the task finishing and this
        // check, but that's totally fine: the worst case is that the abort is
        // ignored.
        if update_data.task.is_finished() {
            return Err(AbortUpdateError::UpdateFinished);
        }

        match update_data.abort_handle.abort(message) {
            Ok(waiter) => {
                waiter.await;
                Ok(())
            }
            Err(_) => {
                // This occurs if the engine has finished execution and has been
                // dropped.
                Err(AbortUpdateError::UpdateFinished)
            }
        }
    }

    async fn put_repository<T>(&mut self, data: T) -> Result<(), HttpError>
    where
        T: io::Read + io::Seek + Send + 'static,
    {
        // Are there any updates currently running? If so, then reject the new
        // repository.
        let running_sps = self
            .sp_update_data
            .iter()
            .filter_map(|(sp_identifier, update_data)| {
                (!update_data.task.is_finished()).then(|| *sp_identifier)
            })
            .collect::<Vec<_>>();
        if !running_sps.is_empty() {
            return Err(HttpError::for_bad_request(
                None,
                "Updates currently running for {running_sps:?}".to_owned(),
            ));
        }

        // Put the repository into the artifact store.
        self.artifact_store.put_repository(data).await?;

        // Reset all running data: a new repository means starting afresh.
        self.sp_update_data.clear();

        Ok(())
    }
}

#[derive(Debug, Clone, Error, Eq, PartialEq)]
pub enum StartUpdateError {
    #[error("no TUF repository available")]
    TufRepositoryUnavailable,
    #[error("targets are already being updated: {}", sps_to_string(.0))]
    UpdateInProgress(Vec<SpIdentifier>),
}

#[derive(Debug, Clone, Error, Eq, PartialEq)]
pub enum ClearUpdateStateError {
    #[error("target is currently being updated")]
    UpdateInProgress,
}

impl ClearUpdateStateError {
    pub(crate) fn to_http_error(&self) -> HttpError {
        let message = DisplayErrorChain::new(self).to_string();

        match self {
            ClearUpdateStateError::UpdateInProgress => {
                HttpError::for_bad_request(None, message)
            }
        }
    }
}

#[derive(Debug, Clone, Error, Eq, PartialEq)]
pub enum AbortUpdateError {
    #[error("update task not started")]
    UpdateNotStarted,

    #[error("update task already finished")]
    UpdateFinished,
}

impl AbortUpdateError {
    pub(crate) fn to_http_error(&self) -> HttpError {
        let message = DisplayErrorChain::new(self).to_string();

        match self {
            AbortUpdateError::UpdateNotStarted
            | AbortUpdateError::UpdateFinished => {
                HttpError::for_bad_request(None, message)
            }
        }
    }
}

#[derive(Debug)]
struct UpdateDriver {}

impl UpdateDriver {
    async fn run(
        self,
        plan: UpdatePlan,
        update_cx: UpdateContext,
        event_buffer: Arc<StdMutex<EventBuffer>>,
        ipr_start_receiver: IprStartReceiver,
        opts: StartUpdateOptions,
        abort_handle_sender: oneshot::Sender<AbortHandle>,
    ) {
        let update_cx = &update_cx;

        // TODO: We currently do updates in the order RoT -> SP -> host. This is
        // generally the correct order, but in some cases there might be a bug
        // which forces us to update components in the order SP -> RoT -> host.
        // How do we handle that?
        //
        // Broadly, there are two ways to do this:
        //
        // 1. Add metadata to artifacts.json indicating the order in which
        //    components should be updated. There are a lot of options in the
        //    design space here, from a simple boolean to a list or DAG
        //    expressing the order, or something even more dynamic than that.
        //
        // 2. Skip updating components that match the same version. This would
        //    let us ship two separate archives in case there's a bug: one with
        //    the newest components for the SP and RoT, and one without.

        // Build the update executor.
        let (sender, mut receiver) = mpsc::channel(128);
        let mut engine = UpdateEngine::new(&update_cx.log, sender);
        let abort_handle = engine.abort_handle();
        _ = abort_handle_sender.send(abort_handle);

        if let Some(secs) = opts.test_step_seconds {
            define_test_steps(&engine, secs);
        }

        let (rot_a, rot_b, sp_artifacts) = match update_cx.sp.type_ {
            SpType::Sled => (
                plan.gimlet_rot_a.clone(),
                plan.gimlet_rot_b.clone(),
                &plan.gimlet_sp,
            ),
            SpType::Power => {
                (plan.psc_rot_a.clone(), plan.psc_rot_b.clone(), &plan.psc_sp)
            }
            SpType::Switch => (
                plan.sidecar_rot_a.clone(),
                plan.sidecar_rot_b.clone(),
                &plan.sidecar_sp,
            ),
        };

        let rot_registrar = engine.for_component(UpdateComponent::Rot);
        let sp_registrar = engine.for_component(UpdateComponent::Sp);

        // To update the RoT, we have to know which slot (A or B) it is
        // currently executing; we must update the _other_ slot. We also want to
        // know its current version (so we can skip updating if we only need to
        // update the SP and/or host).
        let rot_interrogation =
            rot_registrar
                .new_step(
                    UpdateStepId::InterrogateRot,
                    "Checking current RoT version and active slot",
                    |_cx| async move {
                        update_cx.interrogate_rot(rot_a, rot_b).await
                    },
                )
                .register();

        // The SP only has one updateable firmware slot ("the inactive bank").
        // We want to ask about slot 0 (the active slot)'s current version, and
        // we are supposed to always pass 0 when updating.
        let sp_firmware_slot = 0;

        // To update the SP, we want to know both its version and its board (so
        // we can map to the correct artifact from our update plan).
        let sp_artifact_and_version = sp_registrar
            .new_step(
                UpdateStepId::InterrogateSp,
                "Checking SP board and current version",
                move |_cx| async move {
                    let caboose = update_cx
                        .mgs_client
                        .sp_component_caboose_get(
                            update_cx.sp.type_,
                            update_cx.sp.slot,
                            SpComponent::SP_ITSELF.const_as_str(),
                            sp_firmware_slot,
                        )
                        .await
                        .map_err(|error| {
                            UpdateTerminalError::GetSpCabooseFailed { error }
                        })?
                        .into_inner();

                    let Some(sp_artifact) = sp_artifacts.get(&caboose.board)
                    else {
                        return Err(
                            UpdateTerminalError::MissingSpImageForBoard {
                                board: caboose.board,
                            },
                        );
                    };
                    let sp_artifact = sp_artifact.clone();

                    let message = format!(
                        "SP board {}, version {} (git commit {})",
                        caboose.board,
                        caboose.version.as_deref().unwrap_or("unknown"),
                        caboose.git_commit
                    );
                    match caboose.version.map(|v| v.parse::<SemverVersion>()) {
                        Some(Ok(version)) => {
                            StepSuccess::new((sp_artifact, Some(version)))
                                .with_message(message)
                                .into()
                        }
                        Some(Err(err)) => StepWarning::new(
                            (sp_artifact, None),
                            format!(
                                "{message} (failed to parse SP version: {err})"
                            ),
                        )
                        .into(),
                        None => StepWarning::new((sp_artifact, None), message)
                            .into(),
                    }
                },
            )
            .register();
        // Send the update to the RoT.
        let inner_cx =
            SpComponentUpdateContext::new(update_cx, UpdateComponent::Rot);
        rot_registrar
            .new_step(
                UpdateStepId::SpComponentUpdate,
                "Updating RoT",
                move |cx| async move {
                    if let Some(result) = opts.test_simulate_rot_result {
                        return simulate_result(result);
                    }

                    let rot_interrogation =
                        rot_interrogation.into_value(cx.token()).await;

                    let rot_has_this_version = rot_interrogation
                        .active_version_matches_artifact_to_apply();

                    // If this RoT already has this version, skip the rest of
                    // this step, UNLESS we've been told to skip this version
                    // check.
                    if rot_has_this_version && !opts.skip_rot_version_check {
                        return StepSkipped::new(
                            (),
                            format!(
                                "RoT active slot already at version {}",
                                rot_interrogation.artifact_to_apply.id.version
                            ),
                        )
                        .into();
                    }

                    cx.with_nested_engine(|engine| {
                        inner_cx.register_steps(
                            engine,
                            rot_interrogation.slot_to_update,
                            &rot_interrogation.artifact_to_apply,
                        );
                        Ok(())
                    })
                    .await?;

                    // If we updated despite the RoT already having the version
                    // we updated to, make this step return a warning with that
                    // message; otherwise, this is a normal success.
                    if rot_has_this_version {
                        StepWarning::new(
                            (),
                            format!(
                                "RoT updated despite already having version {}",
                                rot_interrogation.artifact_to_apply.id.version
                            ),
                        )
                        .into()
                    } else {
                        StepSuccess::new(()).into()
                    }
                },
            )
            .register();

        let inner_cx =
            SpComponentUpdateContext::new(update_cx, UpdateComponent::Sp);
        sp_registrar
            .new_step(
                UpdateStepId::SpComponentUpdate,
                "Updating SP",
                move |cx| async move {
                    if let Some(result) = opts.test_simulate_sp_result {
                        return simulate_result(result);
                    }

                    let (sp_artifact, sp_version) =
                        sp_artifact_and_version.into_value(cx.token()).await;

                    let sp_has_this_version =
                        Some(&sp_artifact.id.version) == sp_version.as_ref();

                    // If this SP already has this version, skip the rest of
                    // this step, UNLESS we've been told to skip this version
                    // check.
                    if sp_has_this_version && !opts.skip_sp_version_check {
                        return StepSkipped::new(
                            (),
                            format!(
                                "SP already at version {}",
                                sp_artifact.id.version
                            ),
                        )
                        .into();
                    }

                    cx.with_nested_engine(|engine| {
                        inner_cx.register_steps(
                            engine,
                            sp_firmware_slot,
                            &sp_artifact,
                        );
                        Ok(())
                    })
                    .await?;

                    // If we updated despite the SP already having the version
                    // we updated to, make this step return a warning with that
                    // message; otherwise, this is a normal success.
                    if sp_has_this_version {
                        StepWarning::new(
                            (),
                            format!(
                                "SP updated despite already having version {}",
                                sp_artifact.id.version
                            ),
                        )
                        .into()
                    } else {
                        StepSuccess::new(()).into()
                    }
                },
            )
            .register();

        if update_cx.sp.type_ == SpType::Sled {
            self.register_sled_steps(
                update_cx,
                &mut engine,
                &plan,
                ipr_start_receiver,
            );
        }

        // Spawn a task to accept all events from the executing engine.
        let event_receiving_task = tokio::spawn(async move {
            while let Some(event) = receiver.recv().await {
                event_buffer.lock().unwrap().add_event(event);
            }
        });

        // Execute the update engine.
        match engine.execute().await {
            Ok(_cx) => (),
            Err(err) => {
                error!(update_cx.log, "update failed"; "err" => %err);
            }
        }

        // Wait for all events to be received and written to the update log.
        event_receiving_task.await.expect("event receiving task panicked");
    }

    fn register_sled_steps<'a>(
        &self,
        update_cx: &'a UpdateContext,
        engine: &mut UpdateEngine<'a>,
        plan: &'a UpdatePlan,
        ipr_start_receiver: IprStartReceiver,
    ) {
        let mut host_registrar = engine.for_component(UpdateComponent::Host);
        let image_id_handle = self.register_trampoline_phase1_steps(
            update_cx,
            &mut host_registrar,
            plan,
        );

        let start_handle = host_registrar
            .new_step(
                UpdateStepId::DownloadingInstallinator,
                "Downloading installinator, waiting for it to start",
                move |cx| async move {
                    let image_id = image_id_handle.into_value(cx.token()).await;
                    // The previous step should send this value in.
                    let report_receiver = update_cx
                        .wait_for_first_installinator_progress(
                            &cx,
                            ipr_start_receiver,
                            image_id,
                        )
                        .await
                        .map_err(|error| {
                            UpdateTerminalError::DownloadingInstallinatorFailed { error }
                        })?;

                        StepSuccess::new(report_receiver).into()
                    },
            )
            .register();

        let slots_to_update = host_registrar
            .new_step(
                UpdateStepId::RunningInstallinator,
                "Running installinator",
                move |cx| async move {
                    let report_receiver =
                        start_handle.into_value(cx.token()).await;
                    let write_output = update_cx
                        .process_installinator_reports(&cx, report_receiver)
                        .await
                        .map_err(|error| {
                            UpdateTerminalError::RunningInstallinatorFailed {
                                error,
                            }
                        })?;

                    let slots_to_update = write_output
                        .slots_written
                        .into_iter()
                        .map(|slot| match slot {
                            M2Slot::A => 0,
                            M2Slot::B => 1,
                        })
                        .collect::<BTreeSet<u16>>();

                    StepSuccess::new(slots_to_update).into()
                },
            )
            .register();

        // Installinator is done: install the host phase 1 that matches the host
        // phase 2 it installed, and boot our newly-recovered sled.
        self.register_install_host_phase1_and_boot_steps(
            update_cx,
            &mut host_registrar,
            plan,
            slots_to_update,
        );
    }

    // Installs the trampoline phase 1 and configures the host to fetch phase
    // 2 from MGS on boot, returning the image ID of that phase 2 image for use
    // when querying MGS for progress on its delivery to the SP.
    fn register_trampoline_phase1_steps<'a>(
        &self,
        update_cx: &'a UpdateContext,
        registrar: &mut ComponentRegistrar<'_, 'a>,
        plan: &'a UpdatePlan,
    ) -> StepHandle<HostPhase2RecoveryImageId> {
        // We arbitrarily choose to store the trampoline phase 1 in host boot
        // slot 0. We put this in a set for compatibility with the later step
        // that updates both slots.
        const TRAMPOLINE_PHASE_1_BOOT_SLOT: u16 = 0;
        let mut trampoline_phase_1_boot_slots = BTreeSet::new();
        trampoline_phase_1_boot_slots.insert(TRAMPOLINE_PHASE_1_BOOT_SLOT);

        self.register_deliver_host_phase1_steps(
            update_cx,
            registrar,
            &plan.trampoline_phase_1,
            "trampoline",
            StepHandle::ready(trampoline_phase_1_boot_slots).into_shared(),
        );

        // Wait (if necessary) for the trampoline phase 2 upload to MGS to
        // complete. We started a task to do this the first time a sled update
        // was started with this plan.
        let mut upload_trampoline_phase_2_to_mgs =
            update_cx.upload_trampoline_phase_2_to_mgs.clone();

        let image_id_step_handle = registrar.new_step(
            UpdateStepId::WaitingForTrampolinePhase2Upload,
            "Waiting for trampoline phase 2 upload to MGS",
            move |_cx| async move {
                // We expect this loop to run just once, but iterate just in
                // case the image ID doesn't get populated the first time.
                loop {
                    upload_trampoline_phase_2_to_mgs.changed().await.map_err(
                        |_recv_err| {
                            UpdateTerminalError::TrampolinePhase2UploadFailed
                        }
                    )?;

                    if let Some(image_id) = upload_trampoline_phase_2_to_mgs
                        .borrow()
                        .uploaded_image_id
                        .as_ref()
                    {
                        return StepSuccess::new(image_id.clone()).into();
                    }
                }
            },
        ).register();

        registrar
            .new_step(
                UpdateStepId::SettingInstallinatorImageId,
                "Setting installinator image ID",
                move |_cx| async move {
                    let installinator_image_id = InstallinatorImageId {
                        control_plane: plan.control_plane_hash.to_string(),
                        host_phase_2: plan.host_phase_2_hash.to_string(),
                        update_id: update_cx.update_id,
                    };
                    update_cx
                        .mgs_client
                        .sp_installinator_image_id_set(
                            update_cx.sp.type_,
                            update_cx.sp.slot,
                            &installinator_image_id,
                        )
                        .await
                        .map_err(|error| {
                            // HTTP-ERROR-FULL-CAUSE-CHAIN
                            UpdateTerminalError::SetInstallinatorImageIdFailed {
                                error,
                            }
                        })?;

                    StepSuccess::new(()).into()
                },
            )
            .register();

        registrar
            .new_step(
                UpdateStepId::SettingHostStartupOptions,
                "Setting host startup options",
                move |_cx| async move {
                    update_cx
                        .set_component_active_slot(
                            SpComponent::HOST_CPU_BOOT_FLASH.const_as_str(),
                            TRAMPOLINE_PHASE_1_BOOT_SLOT,
                            false,
                        )
                        .await
                        .map_err(|error| {
                            UpdateTerminalError::SetHostBootFlashSlotFailed {
                                error,
                            }
                        })?;

                    update_cx
                        .mgs_client
                        .sp_startup_options_set(
                            update_cx.sp.type_,
                            update_cx.sp.slot,
                            &HostStartupOptions {
                                boot_net: false,
                                boot_ramdisk: false,
                                bootrd: false,
                                kbm: false,
                                kmdb: false,
                                kmdb_boot: false,
                                phase2_recovery_mode: true,
                                prom: false,
                                verbose: false,
                            },
                        )
                        .await
                        .map_err(|error| {
                            UpdateTerminalError::SetHostStartupOptionsFailed {
                                description: "recovery mode",
                                error,
                            }
                        })?;

                    StepSuccess::new(()).into()
                },
            )
            .register();

        // All set - boot the host and let installinator do its thing!
        registrar
            .new_step(
                UpdateStepId::SetHostPowerState { state: PowerState::A0 },
                "Setting host power state to A0",
                move |_cx| async move {
                    update_cx.set_host_power_state(PowerState::A0).await
                },
            )
            .register();

        image_id_step_handle
    }

    fn register_install_host_phase1_and_boot_steps<'engine, 'a: 'engine>(
        &self,
        update_cx: &'a UpdateContext,
        registrar: &mut ComponentRegistrar<'engine, 'a>,
        plan: &'a UpdatePlan,
        slots_to_update: StepHandle<BTreeSet<u16>>,
    ) {
        // Installinator is done - set the stage for the real host to boot.

        // Deliver the real host phase 1 image to whichever slots installinator
        // wrote.
        let slots_to_update = slots_to_update.into_shared();
        self.register_deliver_host_phase1_steps(
            update_cx,
            registrar,
            &plan.host_phase_1,
            "host",
            slots_to_update.clone(),
        );

        // Clear the installinator image ID; failing to do this is _not_ fatal,
        // because any future update will set its own installinator ID anyway;
        // this is for cleanliness more than anything.
        registrar.new_step(
            UpdateStepId::ClearingInstallinatorImageId,
            "Clearing installinator image ID",
            move |_cx| async move {
                if let Err(err) = update_cx
                    .mgs_client
                    .sp_installinator_image_id_delete(
                        update_cx.sp.type_,
                        update_cx.sp.slot,
                    )
                    .await
                {
                    warn!(
                        update_cx.log,
                        "failed to clear installinator image ID (proceeding anyway)";
                        "err" => %err,
                    );
                }

                StepSuccess::new(()).into()
            }).register();

        registrar
            .new_step(
                UpdateStepId::SettingHostStartupOptions,
                "Setting startup options for standard boot",
                move |cx| async move {
                    // Persistently set to boot off of the first disk
                    // installinator successfully updated (usually 0, unless it
                    // only updated 1).
                    let mut slots_to_update =
                        slots_to_update.into_value(cx.token()).await;
                    let slot_to_boot =
                        slots_to_update.pop_first().ok_or_else(|| {
                            UpdateTerminalError::SetHostBootFlashSlotFailed {
                                error: anyhow!(
                                    "installinator reported 0 disks written"
                                ),
                            }
                        })?;
                    update_cx
                        .set_component_active_slot(
                            SpComponent::HOST_CPU_BOOT_FLASH.const_as_str(),
                            slot_to_boot,
                            true,
                        )
                        .await
                        .map_err(|error| {
                            UpdateTerminalError::SetHostBootFlashSlotFailed {
                                error,
                            }
                        })?;

                    // Set "standard boot".
                    update_cx
                        .mgs_client
                        .sp_startup_options_set(
                            update_cx.sp.type_,
                            update_cx.sp.slot,
                            &HostStartupOptions {
                                boot_net: false,
                                boot_ramdisk: false,
                                bootrd: false,
                                kbm: false,
                                kmdb: false,
                                kmdb_boot: false,
                                phase2_recovery_mode: false,
                                prom: false,
                                verbose: false,
                            },
                        )
                        .await
                        .map_err(|error| {
                            // HTTP-ERROR-FULL-CAUSE-CHAIN
                            UpdateTerminalError::SetHostStartupOptionsFailed {
                                description: "standard boot",
                                error,
                            }
                        })?;

                    StepSuccess::new(()).into()
                },
            )
            .register();

        // Boot the host.
        registrar
            .new_step(
                UpdateStepId::SetHostPowerState { state: PowerState::A0 },
                "Booting the host",
                |_cx| async {
                    update_cx.set_host_power_state(PowerState::A0).await
                },
            )
            .register();
    }

    fn register_deliver_host_phase1_steps<'a>(
        &self,
        update_cx: &'a UpdateContext,
        registrar: &mut ComponentRegistrar<'_, 'a>,
        artifact: &'a ArtifactIdData,
        kind: &str, // "host" or "trampoline"
        slots_to_update: SharedStepHandle<BTreeSet<u16>>,
    ) {
        registrar
            .new_step(
                UpdateStepId::SetHostPowerState { state: PowerState::A2 },
                "Setting host power state to A2",
                move |_cx| async move {
                    update_cx.set_host_power_state(PowerState::A2).await
                },
            )
            .register();

        let inner_cx =
            SpComponentUpdateContext::new(update_cx, UpdateComponent::Host);
        registrar
            .new_step(
                UpdateStepId::SpComponentUpdate,
                format!("Updating {kind} phase 1"),
                move |cx| async move {
                    let slots_to_update =
                        slots_to_update.into_value(cx.token()).await;

                    for boot_slot in slots_to_update {
                        cx.with_nested_engine(|engine| {
                            inner_cx
                                .register_steps(engine, boot_slot, artifact);
                            Ok(())
                        })
                        .await?;
                    }
                    StepSuccess::new(()).into()
                },
            )
            .register();
    }
}

fn define_test_steps(engine: &UpdateEngine, secs: u64) {
    engine
        .new_step(
            UpdateComponent::Rot,
            UpdateStepId::TestStep,
            "Test step",
            move |cx| async move {
                cx.with_nested_engine(
                    |engine: &mut UpdateEngine<TestStepSpec>| {
                        engine
                            .new_step(
                                TestStepComponent::Test,
                                TestStepId::Delay,
                                format!("Delay step ({secs} secs)"),
                                |cx| async move {
                                    for sec in 0..secs {
                                        cx.send_progress(
                                        StepProgress::with_current_and_total(
                                            sec,
                                            secs,
                                            "seconds",
                                            serde_json::Value::Null,
                                        ),
                                    )
                                    .await;
                                        tokio::time::sleep(
                                            Duration::from_secs(1),
                                        )
                                        .await;
                                    }

                                    StepSuccess::new(())
                                        .with_message(format!(
                                        "Step completed after {secs} seconds"
                                    ))
                                        .into()
                                },
                            )
                            .register();

                        engine
                        .new_step(
                            TestStepComponent::Test,
                            TestStepId::Delay,
                            "Nested stub step",
                            |_cx| async move { StepSuccess::new(()).into() },
                        )
                        .register();

                        Ok(())
                    },
                )
                .await?;

                StepSuccess::new(()).into()
            },
        )
        .register();
}

#[derive(Debug)]
struct RotInterrogation {
    slot_to_update: u16,
    artifact_to_apply: ArtifactIdData,
    active_version: Option<SemverVersion>,
}

impl RotInterrogation {
    fn active_version_matches_artifact_to_apply(&self) -> bool {
        Some(&self.artifact_to_apply.id.version) == self.active_version.as_ref()
    }
}

fn simulate_result(
    result: UpdateSimulatedResult,
) -> Result<StepResult<()>, UpdateTerminalError> {
    match result {
        UpdateSimulatedResult::Success => {
            StepSuccess::new(()).with_message("Simulated success result").into()
        }
        UpdateSimulatedResult::Warning => {
            StepWarning::new((), "Simulated warning result").into()
        }
        UpdateSimulatedResult::Skipped => {
            StepSkipped::new((), "Simulated skipped result").into()
        }
        UpdateSimulatedResult::Failure => {
            Err(UpdateTerminalError::SimulatedFailure)
        }
    }
}

struct UpdateContext {
    update_id: Uuid,
    sp: SpIdentifier,
    mgs_client: gateway_client::Client,
    upload_trampoline_phase_2_to_mgs:
        watch::Receiver<UploadTrampolinePhase2ToMgsStatus>,
    log: slog::Logger,
}

impl UpdateContext {
    async fn process_installinator_reports<'engine>(
        &self,
        cx: &StepContext,
        mut ipr_receiver: watch::Receiver<EventReport<InstallinatorSpec>>,
    ) -> anyhow::Result<WriteOutput> {
        let mut write_output = None;

        // Note: watch receivers must be used via this pattern, *not* via
        // `while ipr_receiver.changed().await.is_ok()`.
        loop {
            let report = ipr_receiver.borrow_and_update().clone();

            // Prior to processing the report, check for the completion metadata
            // that indicates which disks installinator attempt to /
            // successfully wrote. We only need to do this if we haven't already
            // seen the metadata we care about in a previous report; we should
            // never get multiple completion events that differ in this
            // metadata.
            if write_output.is_none() {
                for event in &report.step_events {
                    // We only care about the outcome of completion events.
                    let Some(outcome) = event.kind.step_outcome() else {
                        continue;
                    };

                    // We only care about successful (including "success with
                    // warning") outcomes.
                    let Some(metadata) = outcome.completion_metadata() else {
                        continue;
                    };

                    match metadata {
                        InstallinatorCompletionMetadata::Write { output } => {
                            write_output = Some(output.clone());
                        }
                        InstallinatorCompletionMetadata::HardwareScan { .. }
                        | InstallinatorCompletionMetadata::ControlPlaneZones { .. }
                        | InstallinatorCompletionMetadata::Download { .. }
                        | InstallinatorCompletionMetadata::Unknown => (),
                    }
                }
            }

            cx.send_nested_report(report).await?;
            if ipr_receiver.changed().await.is_err() {
                break;
            }
        }

        // The receiver being closed means that the installinator has completed.

        write_output.ok_or_else(|| {
            anyhow!("installinator completed without reporting disks written")
        })
    }

    async fn interrogate_rot(
        &self,
        rot_a: ArtifactIdData,
        rot_b: ArtifactIdData,
    ) -> Result<StepResult<RotInterrogation>, UpdateTerminalError> {
        let rot_active_slot = self
            .get_component_active_slot(SpComponent::ROT.const_as_str())
            .await
            .map_err(|error| UpdateTerminalError::GetRotActiveSlotFailed {
                error,
            })?;

        // Flip these around: if 0 (A) is active, we want to
        // update 1 (B), and vice versa.
        let (active_slot_name, slot_to_update, artifact_to_apply) =
            match rot_active_slot {
                0 => ('A', 1, rot_b),
                1 => ('B', 0, rot_a),
                _ => {
                    return Err(UpdateTerminalError::GetRotActiveSlotFailed {
                        error: anyhow!(
                            "unexpected RoT active slot {rot_active_slot}"
                        ),
                    })
                }
            };

        // Read the caboose of the currently-active slot.
        let caboose = self
            .mgs_client
            .sp_component_caboose_get(
                self.sp.type_,
                self.sp.slot,
                SpComponent::ROT.const_as_str(),
                rot_active_slot,
            )
            .await
            .map_err(|error| UpdateTerminalError::GetRotCabooseFailed {
                error,
            })?
            .into_inner();

        let message = format!(
            "RoT slot {active_slot_name} version {} (git commit {})",
            caboose.version.as_deref().unwrap_or("unknown"),
            caboose.git_commit
        );

        let make_result = |active_version| RotInterrogation {
            slot_to_update,
            artifact_to_apply,
            active_version,
        };

        match caboose.version.map(|v| v.parse::<SemverVersion>()) {
            Some(Ok(version)) => StepSuccess::new(make_result(Some(version)))
                .with_message(message)
                .into(),
            Some(Err(err)) => StepWarning::new(
                make_result(None),
                format!("{message} (failed to parse RoT version: {err})"),
            )
            .into(),
            None => StepWarning::new(make_result(None), message).into(),
        }
    }

    /// Poll the RoT asking for its currently active slot, allowing failures up
    /// to a fixed timeout to give time for it to boot.
    ///
    /// Intended to be called after the RoT has been reset.
    async fn wait_for_rot_reboot(
        &self,
        timeout: Duration,
    ) -> anyhow::Result<u16> {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));

        let start = Instant::now();
        loop {
            ticker.tick().await;
            match self
                .get_component_active_slot(SpComponent::ROT.const_as_str())
                .await
            {
                Ok(slot) => return Ok(slot),
                Err(error) => {
                    if start.elapsed() < timeout {
                        warn!(
                            self.log,
                            "failed getting RoT active slot (will retry)";
                            "error" => %error,
                        );
                    } else {
                        return Err(error);
                    }
                }
            }
        }
    }

    async fn wait_for_first_installinator_progress(
        &self,
        cx: &StepContext,
        mut ipr_start_receiver: IprStartReceiver,
        image_id: HostPhase2RecoveryImageId,
    ) -> anyhow::Result<watch::Receiver<EventReport<InstallinatorSpec>>> {
        const MGS_PROGRESS_POLL_INTERVAL: Duration = Duration::from_secs(3);

        // Waiting for the installinator to start is a little strange. It can't
        // start until the host boots, which requires all the normal boot things
        // (DRAM training, etc.), but also fetching the trampoline phase 2 image
        // over the management network -> SP -> uart path, which runs at about
        // 167 KiB/sec. This is a _long_ time to wait with no visible progress,
        // so we'll query MGS for progress of that phase 2 trampoline delivery.
        // However, this query is "best effort" - MGS is observing progress
        // indirectly (i.e., "what was the last request for a phase 2 image I
        // got from this SP"), and it isn't definitive. We'll still report it as
        // long as it matches the trampoline image we're expecting the SP to be
        // pulling, but it's possible we could be seeing stale status from a
        // previous update attempt with the same image.
        //
        // To start, _clear out_ the most recent status that MGS may have
        // cached, so we don't see any stale progress from a previous update
        // through this SP. If somehow we've lost the race and our SP is already
        // actively requesting host blocks, this will discard a real progress
        // message, but that's fine - in that case we expect to see another real
        // one imminently. It's possible (but hopefully unlikely?) that the SP
        // is getting its phase two image from the _other_ scrimlet's MGS
        // instance, in which case we will get no progress info at all until
        // installinator starts reporting in.
        //
        // Throughout this function, we do not fail if a request to MGS fails -
        // these are all "best effort" progress; our real failure mode is if
        // installinator tells us it has failed.
        if let Err(err) = self
            .mgs_client
            .sp_host_phase2_progress_delete(self.sp.type_, self.sp.slot)
            .await
        {
            warn!(
                self.log, "failed to clear SP host phase2 progress";
                "err" => %err,
            );
        }

        let mut interval = tokio::time::interval(MGS_PROGRESS_POLL_INTERVAL);
        interval
            .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                receiver = &mut ipr_start_receiver => {
                    // Received the first progress from the installinator.
                    break receiver.context("start sender died");
                }
                _ = interval.tick() => {
                    self.poll_trampoline_phase2_progress(cx, &image_id).await;
                }
            }
        }
    }

    /// Polls MGS for the latest trampoline phase 2 progress.
    ///
    /// The naming is somewhat confusing here: the code to fetch the respective
    /// phase 2 is present within all phase 1 ROMs, both host and trampoline.
    /// This is why the API has the name "host phase 2" in it. However, for this
    /// update flow it is only activated for trampoline images.
    async fn poll_trampoline_phase2_progress(
        &self,
        cx: &StepContext,
        uploaded_trampoline_phase2_id: &HostPhase2RecoveryImageId,
    ) {
        match self
            .mgs_client
            .sp_host_phase2_progress_get(self.sp.type_, self.sp.slot)
            .await
            .map(|response| response.into_inner())
        {
            Ok(HostPhase2Progress::Available {
                image_id,
                offset,
                total_size,
                ..
            }) => {
                // Does this image ID match the one we uploaded? If so,
                // record our current progress; if not, this is probably
                // stale data from a past update, and we have no progress
                // information.
                if &image_id == uploaded_trampoline_phase2_id {
                    cx.send_progress(StepProgress::with_current_and_total(
                        offset,
                        total_size,
                        ProgressUnits::BYTES,
                        Default::default(),
                    ))
                    .await;
                }
            }
            Ok(HostPhase2Progress::None) => {
                // No progress available -- don't send an update.
                // XXX should we reset the StepProgress to running?
            }
            Err(err) => {
                warn!(
                    self.log, "failed to get SP host phase2 progress";
                    "err" => %err,
                );
            }
        }
    }

    async fn set_host_power_state(
        &self,
        power_state: PowerState,
    ) -> Result<StepResult<()>, UpdateTerminalError> {
        info!(self.log, "moving host to {power_state:?}");
        self.mgs_client
            .sp_power_state_set(self.sp.type_, self.sp.slot, power_state)
            .await
            .map(|response| response.into_inner())
            .map_err(|error| UpdateTerminalError::UpdatePowerStateFailed {
                error,
            })?;
        StepSuccess::new(()).into()
    }

    async fn get_component_active_slot(
        &self,
        component: &str,
    ) -> anyhow::Result<u16> {
        self.mgs_client
            .sp_component_active_slot_get(
                self.sp.type_,
                self.sp.slot,
                component,
            )
            .await
            .context("failed to get component active slot")
            .map(|res| res.into_inner().slot)
    }

    async fn set_component_active_slot(
        &self,
        component: &str,
        slot: u16,
        persist: bool,
    ) -> anyhow::Result<()> {
        self.mgs_client
            .sp_component_active_slot_set(
                self.sp.type_,
                self.sp.slot,
                component,
                persist,
                &SpComponentFirmwareSlot { slot },
            )
            .await
            .context("failed to set component active slot")
            .map(|res| res.into_inner())
    }

    async fn reset_sp_component(&self, component: &str) -> anyhow::Result<()> {
        self.mgs_client
            .sp_component_reset(self.sp.type_, self.sp.slot, component)
            .await
            .context("failed to reset SP")
            .map(|res| res.into_inner())
    }

    async fn poll_component_update<S: StepSpec>(
        &self,
        cx: StepContext<S>,
        stage: ComponentUpdateStage,
        update_id: Uuid,
        component: &str,
    ) -> anyhow::Result<()>
    where
        S::ProgressMetadata: Default,
    {
        // How often we poll MGS for the progress of an update once it starts.
        const STATUS_POLL_FREQ: Duration = Duration::from_millis(300);

        loop {
            let status = self
                .mgs_client
                .sp_component_update_status(
                    self.sp.type_,
                    self.sp.slot,
                    component,
                )
                .await?
                .into_inner();

            match status {
                SpUpdateStatus::None => {
                    bail!("SP no longer processing update (did it reset?)")
                }
                SpUpdateStatus::Preparing { id, progress } => {
                    ensure!(id == update_id, "SP processing different update");
                    if stage == ComponentUpdateStage::Preparing {
                        if let Some(progress) = progress {
                            cx.send_progress(
                                StepProgress::with_current_and_total(
                                    progress.current as u64,
                                    progress.total as u64,
                                    // The actual units here depend on the
                                    // component being updated and are a bit
                                    // hard to explain succinctly:
                                    // https://github.com/oxidecomputer/omicron/pull/3267#discussion_r1229700370
                                    ProgressUnits::new("preparation steps"),
                                    Default::default(),
                                ),
                            )
                            .await;
                        }
                    } else {
                        warn!(
                            self.log,
                            "component update moved backwards \
                             from {stage:?} to preparing"
                        );
                    }
                }
                SpUpdateStatus::InProgress {
                    bytes_received,
                    id,
                    total_bytes,
                } => {
                    ensure!(id == update_id, "SP processing different update");
                    match stage {
                        ComponentUpdateStage::Preparing => {
                            // The prepare step is done -- exit this loop and move
                            // to the next stage.
                            return Ok(());
                        }
                        ComponentUpdateStage::InProgress => {
                            cx.send_progress(
                                StepProgress::with_current_and_total(
                                    bytes_received as u64,
                                    total_bytes as u64,
                                    ProgressUnits::BYTES,
                                    Default::default(),
                                ),
                            )
                            .await;
                        }
                    }
                }
                SpUpdateStatus::Complete { id } => {
                    ensure!(id == update_id, "SP processing different update");
                    return Ok(());
                }
                SpUpdateStatus::Aborted { id } => {
                    ensure!(id == update_id, "SP processing different update");
                    bail!("update aborted");
                }
                SpUpdateStatus::Failed { code, id } => {
                    ensure!(id == update_id, "SP processing different update");
                    bail!("update failed (error code {code})");
                }
                SpUpdateStatus::RotError { message, id } => {
                    ensure!(id == update_id, "SP processing different update");
                    bail!("update failed (rot error message {message})");
                }
            }

            tokio::time::sleep(STATUS_POLL_FREQ).await;
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ComponentUpdateStage {
    Preparing,
    InProgress,
}

async fn upload_trampoline_phase_2_to_mgs(
    mgs_client: gateway_client::Client,
    artifact: ArtifactIdData,
    status: watch::Sender<UploadTrampolinePhase2ToMgsStatus>,
    log: Logger,
) {
    let data = artifact.data;
    let hash = data.hash();
    let upload_task = move || {
        let mgs_client = mgs_client.clone();
        let data = data.clone();

        async move {
            let image_stream = data.reader_stream().await.map_err(|e| {
                // TODO-correctness If we get an I/O error opening the file
                // associated with `data`, is it actually a transient error? If
                // we change this to `permanent` we'll have to do some different
                // error handling below and at our call site to retry. We
                // _shouldn't_ get errors from `reader_stream()` in general, so
                // it's probably okay either way?
                backoff::BackoffError::transient(format!("{e:#}"))
            })?;
            mgs_client
                .recovery_host_phase2_upload(reqwest::Body::wrap_stream(
                    image_stream,
                ))
                .await
                .map_err(|e| backoff::BackoffError::transient(e.to_string()))
        }
    };

    let log_failure = move |err, delay| {
        warn!(
            log,
            "failed to upload trampoline phase 2 to MGS, will retry in {:?}",
            delay;
            "err" => %err,
        );
    };

    // retry_policy_internal_service_aggressive() retries forever, so we can
    // unwrap this call to retry_notify
    let uploaded_image_id = backoff::retry_notify(
        backoff::retry_policy_internal_service_aggressive(),
        upload_task,
        log_failure,
    )
    .await
    .unwrap()
    .into_inner();

    // Notify all receivers that we've uploaded the image.
    _ = status.send(UploadTrampolinePhase2ToMgsStatus {
        hash,
        uploaded_image_id: Some(uploaded_image_id),
    });

    // Wait for all receivers to be gone before we exit, so they don't get recv
    // errors unless we're cancelled.
    status.closed().await;
}

struct SpComponentUpdateContext<'a> {
    update_cx: &'a UpdateContext,
    component: UpdateComponent,
}

impl<'a> SpComponentUpdateContext<'a> {
    fn new(update_cx: &'a UpdateContext, component: UpdateComponent) -> Self {
        Self { update_cx, component }
    }

    fn register_steps(
        &self,
        engine: &UpdateEngine<'a, SpComponentUpdateSpec>,
        firmware_slot: u16,
        artifact: &'a ArtifactIdData,
    ) {
        let update_id = Uuid::new_v4();
        let component = self.component;
        let update_cx = self.update_cx;

        let component_name = match self.component {
            UpdateComponent::Rot => SpComponent::ROT.const_as_str(),
            UpdateComponent::Sp => SpComponent::SP_ITSELF.const_as_str(),
            UpdateComponent::Host => {
                SpComponent::HOST_CPU_BOOT_FLASH.const_as_str()
            }
        };

        let registrar = engine.for_component(component);

        registrar
            .new_step(
                SpComponentUpdateStepId::Sending,
                format!("Sending data to MGS (slot {firmware_slot})"),
                move |_cx| async move {
                    let data_stream = artifact
                        .data
                        .reader_stream()
                        .await
                        .map_err(|error| {
                            SpComponentUpdateTerminalError::SpComponentUpdateFailed {
                                stage: SpComponentUpdateStage::Sending,
                                artifact: artifact.id.clone(),
                                error,
                            }
                        })?;

                    // TODO: we should be able to report some sort of progress
                    // here for the file upload.
                    update_cx
                        .mgs_client
                        .sp_component_update(
                            update_cx.sp.type_,
                            update_cx.sp.slot,
                            component_name,
                            firmware_slot,
                            &update_id,
                            reqwest::Body::wrap_stream(data_stream),
                        )
                        .await
                        .map_err(|error| {
                            SpComponentUpdateTerminalError::SpComponentUpdateFailed {
                                stage: SpComponentUpdateStage::Sending,
                                artifact: artifact.id.clone(),
                                error: anyhow!(error),
                            }
                        })?;

                    StepSuccess::new(()).into()
                },
            )
            .register();

        registrar
            .new_step(
                SpComponentUpdateStepId::Preparing,
                format!("Preparing for update (slot {firmware_slot})"),
                move |cx| async move {
                    update_cx
                        .poll_component_update(
                            cx,
                            ComponentUpdateStage::Preparing,
                            update_id,
                            component_name,
                        )
                        .await
                        .map_err(|error| {
                            SpComponentUpdateTerminalError::SpComponentUpdateFailed {
                                stage: SpComponentUpdateStage::Preparing,
                                artifact: artifact.id.clone(),
                                error,
                            }
                        })?;

                    StepSuccess::new(()).into()
                },
            )
            .register();

        registrar
            .new_step(
                SpComponentUpdateStepId::Writing,
                format!("Writing update (slot {firmware_slot})"),
                move |cx| async move {
                    update_cx
                        .poll_component_update(
                            cx,
                            ComponentUpdateStage::InProgress,
                            update_id,
                            component_name,
                        )
                        .await
                        .map_err(|error| {
                            SpComponentUpdateTerminalError::SpComponentUpdateFailed {
                                stage: SpComponentUpdateStage::Writing,
                                artifact: artifact.id.clone(),
                                error,
                            }
                        })?;

                    StepSuccess::new(()).into()
                },
            )
            .register();

        // If we just updated the RoT or SP, immediately reboot it into the new
        // update. (One can imagine an update process _not_ wanting to do this,
        // to stage updates for example, but for wicketd-driven recovery it's
        // fine to do this immediately.)
        match component {
            UpdateComponent::Rot => {
                // Prior to rebooting the RoT, we have to tell it to boot into
                // the firmware slot we just updated.
                registrar
                    .new_step(
                        SpComponentUpdateStepId::SettingActiveBootSlot,
                        format!("Setting RoT active slot to {firmware_slot}"),
                        move |_cx| async move {
                            update_cx
                                .set_component_active_slot(
                                    component_name,
                                    firmware_slot,
                                    true,
                                )
                                .await
                                .map_err(|error| {
                                    SpComponentUpdateTerminalError::SetRotActiveSlotFailed {
                                        error,
                                    }
                                })?;
                            StepSuccess::new(()).into()
                        },
                    )
                    .register();

                // Reset the RoT.
                registrar
                    .new_step(
                        SpComponentUpdateStepId::Resetting,
                        "Resetting RoT",
                        move |_cx| async move {
                            update_cx
                                .reset_sp_component(component_name)
                                .await
                                .map_err(|error| {
                                    SpComponentUpdateTerminalError::RotResetFailed {
                                        error,
                                    }
                                })?;
                            StepSuccess::new(()).into()
                        },
                    )
                    .register();

                // Ensure the RoT has actually booted into the slot we just
                // wrote. This can fail for a variety of reasons; the two big
                // categories are:
                //
                // 1. The image is corrupt or signed with incorrect keys (in
                //    which case the RoT will boot back into the previous image)
                // 2. The RoT gets wedged in a state that requires an
                //    ignition-level power cycle to rectify (e.g.,
                //    https://github.com/oxidecomputer/hubris/issues/1451).
                //
                // We will not attempt to work around either of these
                // automatically: we will just poll the RoT for a fixed amount
                // of time (30 seconds should be _more_ than enough), and fail
                // if we either (a) get a successful response with an unexpected
                // active slot (error category 1) or (b) fail to get a
                // successful response at all (error category 2).
                registrar
                    .new_step(
                        SpComponentUpdateStepId::Resetting,
                        format!("Waiting for RoT to boot slot {firmware_slot}"),
                        move |_cx| async move {
                            const WAIT_FOR_BOOT_TIMEOUT: Duration =
                                Duration::from_secs(30);
                            let active_slot = update_cx
                                .wait_for_rot_reboot(WAIT_FOR_BOOT_TIMEOUT)
                                .await
                                .map_err(|error| {
                                    SpComponentUpdateTerminalError::GetRotActiveSlotFailed { error }
                                })?;
                            if active_slot == firmware_slot {
                                StepSuccess::new(()).into()
                            } else {
                                Err(SpComponentUpdateTerminalError::RotUnexpectedActiveSlot { active_slot })
                            }
                        },
                    )
                    .register();
            }
            UpdateComponent::Sp => {
                // Nothing special to do on the SP - just reset it.
                registrar
                    .new_step(
                        SpComponentUpdateStepId::Resetting,
                        "Resetting SP",
                        move |_cx| async move {
                            update_cx
                                .reset_sp_component(component_name)
                                .await
                                .map_err(|error| {
                                    SpComponentUpdateTerminalError::SpResetFailed { error }
                                })?;
                            StepSuccess::new(()).into()
                        },
                    )
                    .register();
            }
            UpdateComponent::Host => (),
        }
    }
}
