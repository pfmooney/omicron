// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTP entrypoint functions for the sled agent's exposed API

use super::sled_agent::SledAgent;
use crate::params::{
    CleanupContextUpdate, DiskEnsureBody, InstanceEnsureBody,
    InstancePutMigrationIdsBody, InstancePutStateBody,
    InstancePutStateResponse, InstanceUnregisterResponse, ServiceEnsureBody,
    SledRole, TimeSync, VpcFirewallRulesEnsureBody, ZoneBundleId,
    ZoneBundleMetadata, Zpool,
};
use crate::sled_agent::Error as SledAgentError;
use crate::zone_bundle;
use camino::Utf8PathBuf;
use dropshot::{
    endpoint, ApiDescription, FreeformBody, HttpError, HttpResponseCreated,
    HttpResponseDeleted, HttpResponseHeaders, HttpResponseOk,
    HttpResponseUpdatedNoContent, Path, Query, RequestContext, TypedBody,
};
use illumos_utils::opte::params::{
    DeleteVirtualNetworkInterfaceHost, SetVirtualNetworkInterfaceHost,
};
use omicron_common::api::external::Error;
use omicron_common::api::internal::nexus::DiskRuntimeState;
use omicron_common::api::internal::nexus::InstanceRuntimeState;
use omicron_common::api::internal::nexus::UpdateArtifactId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

type SledApiDescription = ApiDescription<SledAgent>;

/// Returns a description of the sled agent API
pub fn api() -> SledApiDescription {
    fn register_endpoints(api: &mut SledApiDescription) -> Result<(), String> {
        api.register(disk_put)?;
        api.register(cockroachdb_init)?;
        api.register(instance_issue_disk_snapshot_request)?;
        api.register(instance_put_migration_ids)?;
        api.register(instance_put_state)?;
        api.register(instance_register)?;
        api.register(instance_unregister)?;
        api.register(services_put)?;
        api.register(zones_list)?;
        api.register(zone_bundle_list)?;
        api.register(zone_bundle_list_all)?;
        api.register(zone_bundle_create)?;
        api.register(zone_bundle_get)?;
        api.register(zone_bundle_delete)?;
        api.register(zone_bundle_utilization)?;
        api.register(zone_bundle_cleanup_context)?;
        api.register(zone_bundle_cleanup_context_update)?;
        api.register(zone_bundle_cleanup)?;
        api.register(sled_role_get)?;
        api.register(set_v2p)?;
        api.register(del_v2p)?;
        api.register(timesync_get)?;
        api.register(update_artifact)?;
        api.register(vpc_firewall_rules_put)?;
        api.register(zpools_get)?;

        Ok(())
    }

    let mut api = SledApiDescription::new();
    if let Err(err) = register_endpoints(&mut api) {
        panic!("failed to register entrypoints: {}", err);
    }
    api
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
struct ZonePathParam {
    /// The name of the zone.
    zone_name: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
struct ZoneBundleFilter {
    /// An optional substring used to filter zone bundles.
    filter: Option<String>,
}

/// List all zone bundles that exist, even for now-deleted zones.
#[endpoint {
    method = GET,
    path = "/zones/bundles",
}]
async fn zone_bundle_list_all(
    rqctx: RequestContext<SledAgent>,
    query: Query<ZoneBundleFilter>,
) -> Result<HttpResponseOk<Vec<ZoneBundleMetadata>>, HttpError> {
    let sa = rqctx.context();
    let filter = query.into_inner().filter;
    sa.list_all_zone_bundles(filter.as_deref())
        .await
        .map(HttpResponseOk)
        .map_err(HttpError::from)
}

/// List the zone bundles that are available for a running zone.
#[endpoint {
    method = GET,
    path = "/zones/bundles/{zone_name}",
}]
async fn zone_bundle_list(
    rqctx: RequestContext<SledAgent>,
    params: Path<ZonePathParam>,
) -> Result<HttpResponseOk<Vec<ZoneBundleMetadata>>, HttpError> {
    let params = params.into_inner();
    let zone_name = params.zone_name;
    let sa = rqctx.context();
    sa.list_zone_bundles(&zone_name)
        .await
        .map(HttpResponseOk)
        .map_err(HttpError::from)
}

/// Ask the sled agent to create a zone bundle.
#[endpoint {
    method = POST,
    path = "/zones/bundles/{zone_name}",
}]
async fn zone_bundle_create(
    rqctx: RequestContext<SledAgent>,
    params: Path<ZonePathParam>,
) -> Result<HttpResponseCreated<ZoneBundleMetadata>, HttpError> {
    let params = params.into_inner();
    let zone_name = params.zone_name;
    let sa = rqctx.context();
    sa.create_zone_bundle(&zone_name)
        .await
        .map(HttpResponseCreated)
        .map_err(HttpError::from)
}

/// Fetch the binary content of a single zone bundle.
#[endpoint {
    method = GET,
    path = "/zones/bundles/{zone_name}/{bundle_id}",
}]
async fn zone_bundle_get(
    rqctx: RequestContext<SledAgent>,
    params: Path<ZoneBundleId>,
) -> Result<HttpResponseHeaders<HttpResponseOk<FreeformBody>>, HttpError> {
    let params = params.into_inner();
    let zone_name = params.zone_name;
    let bundle_id = params.bundle_id;
    let sa = rqctx.context();
    let Some(path) = sa
        .get_zone_bundle_paths(&zone_name, &bundle_id)
        .await
        .map_err(HttpError::from)?
        .into_iter()
        .next()
    else {
        return Err(HttpError::for_not_found(
            None,
            format!(
                "No zone bundle for zone '{}' with ID '{}'",
                zone_name, bundle_id
            ),
        ));
    };
    let f = tokio::fs::File::open(&path).await.map_err(|e| {
        HttpError::for_internal_error(format!(
            "failed to open zone bundle file at {}: {:?}",
            path, e,
        ))
    })?;
    let stream = hyper_staticfile::FileBytesStream::new(f);
    let body = FreeformBody(stream.into_body());
    let mut response = HttpResponseHeaders::new_unnamed(HttpResponseOk(body));
    response.headers_mut().append(
        http::header::CONTENT_TYPE,
        "application/gzip".try_into().unwrap(),
    );
    Ok(response)
}

/// Delete a zone bundle.
#[endpoint {
    method = DELETE,
    path = "/zones/bundles/{zone_name}/{bundle_id}",
}]
async fn zone_bundle_delete(
    rqctx: RequestContext<SledAgent>,
    params: Path<ZoneBundleId>,
) -> Result<HttpResponseDeleted, HttpError> {
    let params = params.into_inner();
    let zone_name = params.zone_name;
    let bundle_id = params.bundle_id;
    let sa = rqctx.context();
    let paths = sa
        .get_zone_bundle_paths(&zone_name, &bundle_id)
        .await
        .map_err(HttpError::from)?;
    if paths.is_empty() {
        return Err(HttpError::for_not_found(
            None,
            format!(
                "No zone bundle for zone '{}' with ID '{}'",
                zone_name, bundle_id
            ),
        ));
    };
    for path in paths.into_iter() {
        tokio::fs::remove_file(&path).await.map_err(|e| {
            HttpError::for_internal_error(format!(
                "Failed to delete zone bundle: {e}"
            ))
        })?;
    }
    Ok(HttpResponseDeleted())
}

/// Return utilization information about all zone bundles.
#[endpoint {
    method = GET,
    path = "/zones/bundle-cleanup/utilization",
}]
async fn zone_bundle_utilization(
    rqctx: RequestContext<SledAgent>,
) -> Result<
    HttpResponseOk<BTreeMap<Utf8PathBuf, zone_bundle::BundleUtilization>>,
    HttpError,
> {
    let sa = rqctx.context();
    sa.zone_bundle_utilization()
        .await
        .map(HttpResponseOk)
        .map_err(HttpError::from)
}

/// Return context used by the zone-bundle cleanup task.
#[endpoint {
    method = GET,
    path = "/zones/bundle-cleanup/context",
}]
async fn zone_bundle_cleanup_context(
    rqctx: RequestContext<SledAgent>,
) -> Result<HttpResponseOk<zone_bundle::CleanupContext>, HttpError> {
    let sa = rqctx.context();
    Ok(HttpResponseOk(sa.zone_bundle_cleanup_context().await))
}

/// Update context used by the zone-bundle cleanup task.
#[endpoint {
    method = PUT,
    path = "/zones/bundle-cleanup/context",
}]
async fn zone_bundle_cleanup_context_update(
    rqctx: RequestContext<SledAgent>,
    body: TypedBody<CleanupContextUpdate>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sa = rqctx.context();
    let params = body.into_inner();
    let new_period = params
        .period
        .map(zone_bundle::CleanupPeriod::new)
        .transpose()
        .map_err(|e| HttpError::from(SledAgentError::from(e)))?;
    let new_priority = params.priority;
    let new_limit = params
        .storage_limit
        .map(zone_bundle::StorageLimit::new)
        .transpose()
        .map_err(|e| HttpError::from(SledAgentError::from(e)))?;
    sa.update_zone_bundle_cleanup_context(new_period, new_limit, new_priority)
        .await
        .map(|_| HttpResponseUpdatedNoContent())
        .map_err(HttpError::from)
}

/// Trigger a zone bundle cleanup.
#[endpoint {
    method = POST,
    path = "/zones/bundle-cleanup",
}]
async fn zone_bundle_cleanup(
    rqctx: RequestContext<SledAgent>,
) -> Result<
    HttpResponseOk<BTreeMap<Utf8PathBuf, zone_bundle::CleanupCount>>,
    HttpError,
> {
    let sa = rqctx.context();
    sa.zone_bundle_cleanup().await.map(HttpResponseOk).map_err(HttpError::from)
}

/// List the zones that are currently managed by the sled agent.
#[endpoint {
    method = GET,
    path = "/zones",
}]
async fn zones_list(
    rqctx: RequestContext<SledAgent>,
) -> Result<HttpResponseOk<Vec<String>>, HttpError> {
    let sa = rqctx.context();
    sa.zones_list().await.map(HttpResponseOk).map_err(HttpError::from)
}

#[endpoint {
    method = PUT,
    path = "/services",
}]
async fn services_put(
    rqctx: RequestContext<SledAgent>,
    body: TypedBody<ServiceEnsureBody>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sa = rqctx.context().clone();
    let body_args = body.into_inner();

    // Spawn a separate task to run `services_ensure`: cancellation of this
    // endpoint's future (as might happen if the client abandons the request or
    // times out) could result in leaving zones partially configured and the
    // in-memory state of the service manager invalid. See:
    // oxidecomputer/omicron#3098.
    let handler = async move {
        match sa.services_ensure(body_args).await {
            Ok(()) => Ok(()),
            Err(e) => {
                // Log the error here to make things clear even if the client
                // has already disconnected.
                error!(sa.logger(), "failed to initialize services: {e}");
                Err(e)
            }
        }
    };
    match tokio::spawn(handler).await {
        Ok(result) => result.map_err(|e| Error::from(e))?,

        Err(e) => {
            return Err(HttpError::for_internal_error(format!(
                "unexpected failure awaiting \"services_ensure\": {:#}",
                e
            )));
        }
    }

    Ok(HttpResponseUpdatedNoContent())
}

#[endpoint {
    method = GET,
    path = "/zpools",
}]
async fn zpools_get(
    rqctx: RequestContext<SledAgent>,
) -> Result<HttpResponseOk<Vec<Zpool>>, HttpError> {
    let sa = rqctx.context();
    Ok(HttpResponseOk(sa.zpools_get().await.map_err(|e| Error::from(e))?))
}

#[endpoint {
    method = GET,
    path = "/sled-role",
}]
async fn sled_role_get(
    rqctx: RequestContext<SledAgent>,
) -> Result<HttpResponseOk<SledRole>, HttpError> {
    let sa = rqctx.context();
    Ok(HttpResponseOk(sa.get_role()))
}

/// Initializes a CockroachDB cluster
#[endpoint {
    method = POST,
    path = "/cockroachdb",
}]
async fn cockroachdb_init(
    rqctx: RequestContext<SledAgent>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sa = rqctx.context();
    sa.cockroachdb_initialize().await?;
    Ok(HttpResponseUpdatedNoContent())
}

/// Path parameters for Instance requests (sled agent API)
#[derive(Deserialize, JsonSchema)]
struct InstancePathParam {
    instance_id: Uuid,
}

#[endpoint {
    method = PUT,
    path = "/instances/{instance_id}",
}]
async fn instance_register(
    rqctx: RequestContext<SledAgent>,
    path_params: Path<InstancePathParam>,
    body: TypedBody<InstanceEnsureBody>,
) -> Result<HttpResponseOk<InstanceRuntimeState>, HttpError> {
    let sa = rqctx.context();
    let instance_id = path_params.into_inner().instance_id;
    let body_args = body.into_inner();
    Ok(HttpResponseOk(
        sa.instance_ensure_registered(instance_id, body_args.initial).await?,
    ))
}

#[endpoint {
    method = DELETE,
    path = "/instances/{instance_id}",
}]
async fn instance_unregister(
    rqctx: RequestContext<SledAgent>,
    path_params: Path<InstancePathParam>,
) -> Result<HttpResponseOk<InstanceUnregisterResponse>, HttpError> {
    let sa = rqctx.context();
    let instance_id = path_params.into_inner().instance_id;
    Ok(HttpResponseOk(sa.instance_ensure_unregistered(instance_id).await?))
}

#[endpoint {
    method = PUT,
    path = "/instances/{instance_id}/state",
}]
async fn instance_put_state(
    rqctx: RequestContext<SledAgent>,
    path_params: Path<InstancePathParam>,
    body: TypedBody<InstancePutStateBody>,
) -> Result<HttpResponseOk<InstancePutStateResponse>, HttpError> {
    let sa = rqctx.context();
    let instance_id = path_params.into_inner().instance_id;
    let body_args = body.into_inner();
    Ok(HttpResponseOk(
        sa.instance_ensure_state(instance_id, body_args.state).await?,
    ))
}

#[endpoint {
    method = PUT,
    path = "/instances/{instance_id}/migration-ids",
}]
async fn instance_put_migration_ids(
    rqctx: RequestContext<SledAgent>,
    path_params: Path<InstancePathParam>,
    body: TypedBody<InstancePutMigrationIdsBody>,
) -> Result<HttpResponseOk<InstanceRuntimeState>, HttpError> {
    let sa = rqctx.context();
    let instance_id = path_params.into_inner().instance_id;
    let body_args = body.into_inner();
    Ok(HttpResponseOk(
        sa.instance_put_migration_ids(
            instance_id,
            &body_args.old_runtime,
            &body_args.migration_params,
        )
        .await?,
    ))
}

/// Path parameters for Disk requests (sled agent API)
#[derive(Deserialize, JsonSchema)]
struct DiskPathParam {
    disk_id: Uuid,
}

#[endpoint {
    method = PUT,
    path = "/disks/{disk_id}",
}]
async fn disk_put(
    rqctx: RequestContext<SledAgent>,
    path_params: Path<DiskPathParam>,
    body: TypedBody<DiskEnsureBody>,
) -> Result<HttpResponseOk<DiskRuntimeState>, HttpError> {
    let sa = rqctx.context();
    let disk_id = path_params.into_inner().disk_id;
    let body_args = body.into_inner();
    Ok(HttpResponseOk(
        sa.disk_ensure(
            disk_id,
            body_args.initial_runtime.clone(),
            body_args.target.clone(),
        )
        .await
        .map_err(|e| Error::from(e))?,
    ))
}

#[endpoint {
    method = POST,
    path = "/update"
}]
async fn update_artifact(
    rqctx: RequestContext<SledAgent>,
    artifact: TypedBody<UpdateArtifactId>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sa = rqctx.context();
    sa.update_artifact(artifact.into_inner()).await.map_err(Error::from)?;
    Ok(HttpResponseUpdatedNoContent())
}

#[derive(Deserialize, JsonSchema)]
pub struct InstanceIssueDiskSnapshotRequestPathParam {
    instance_id: Uuid,
    disk_id: Uuid,
}

#[derive(Deserialize, JsonSchema)]
pub struct InstanceIssueDiskSnapshotRequestBody {
    snapshot_id: Uuid,
}

#[derive(Serialize, JsonSchema)]
pub struct InstanceIssueDiskSnapshotRequestResponse {
    snapshot_id: Uuid,
}

/// Take a snapshot of a disk that is attached to an instance
#[endpoint {
    method = POST,
    path = "/instances/{instance_id}/disks/{disk_id}/snapshot",
}]
async fn instance_issue_disk_snapshot_request(
    rqctx: RequestContext<SledAgent>,
    path_params: Path<InstanceIssueDiskSnapshotRequestPathParam>,
    body: TypedBody<InstanceIssueDiskSnapshotRequestBody>,
) -> Result<HttpResponseOk<InstanceIssueDiskSnapshotRequestResponse>, HttpError>
{
    let sa = rqctx.context();
    let path_params = path_params.into_inner();
    let body = body.into_inner();

    sa.instance_issue_disk_snapshot_request(
        path_params.instance_id,
        path_params.disk_id,
        body.snapshot_id,
    )
    .await?;

    Ok(HttpResponseOk(InstanceIssueDiskSnapshotRequestResponse {
        snapshot_id: body.snapshot_id,
    }))
}

/// Path parameters for VPC requests (sled agent API)
#[derive(Deserialize, JsonSchema)]
struct VpcPathParam {
    vpc_id: Uuid,
}

#[endpoint {
    method = PUT,
    path = "/vpc/{vpc_id}/firewall/rules",
}]
async fn vpc_firewall_rules_put(
    rqctx: RequestContext<SledAgent>,
    path_params: Path<VpcPathParam>,
    body: TypedBody<VpcFirewallRulesEnsureBody>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sa = rqctx.context();
    let _vpc_id = path_params.into_inner().vpc_id;
    let body_args = body.into_inner();

    sa.firewall_rules_ensure(body_args.vni, &body_args.rules[..])
        .await
        .map_err(Error::from)?;

    Ok(HttpResponseUpdatedNoContent())
}

/// Path parameters for V2P mapping related requests (sled agent API)
#[allow(dead_code)]
#[derive(Deserialize, JsonSchema)]
struct V2pPathParam {
    interface_id: Uuid,
}

/// Create a mapping from a virtual NIC to a physical host
// Keep interface_id to maintain parity with the simulated sled agent, which
// requires interface_id on the path.
#[endpoint {
    method = PUT,
    path = "/v2p/{interface_id}",
}]
async fn set_v2p(
    rqctx: RequestContext<SledAgent>,
    _path_params: Path<V2pPathParam>,
    body: TypedBody<SetVirtualNetworkInterfaceHost>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sa = rqctx.context();
    let body_args = body.into_inner();

    sa.set_virtual_nic_host(&body_args).await.map_err(Error::from)?;

    Ok(HttpResponseUpdatedNoContent())
}

/// Delete a mapping from a virtual NIC to a physical host
// Keep interface_id to maintain parity with the simulated sled agent, which
// requires interface_id on the path.
#[endpoint {
    method = DELETE,
    path = "/v2p/{interface_id}",
}]
async fn del_v2p(
    rqctx: RequestContext<SledAgent>,
    _path_params: Path<V2pPathParam>,
    body: TypedBody<DeleteVirtualNetworkInterfaceHost>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sa = rqctx.context();
    let body_args = body.into_inner();

    sa.unset_virtual_nic_host(&body_args).await.map_err(Error::from)?;

    Ok(HttpResponseUpdatedNoContent())
}

#[endpoint {
    method = GET,
    path = "/timesync",
}]
async fn timesync_get(
    rqctx: RequestContext<SledAgent>,
) -> Result<HttpResponseOk<TimeSync>, HttpError> {
    let sa = rqctx.context();
    Ok(HttpResponseOk(sa.timesync_get().await.map_err(|e| Error::from(e))?))
}
