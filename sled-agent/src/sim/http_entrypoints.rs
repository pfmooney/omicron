// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! HTTP entrypoint functions for the sled agent's exposed API

use crate::params::{
    DiskEnsureBody, InstanceEnsureBody, InstancePutMigrationIdsBody,
    InstancePutStateBody, InstancePutStateResponse, InstanceUnregisterResponse,
    VpcFirewallRulesEnsureBody,
};
use dropshot::endpoint;
use dropshot::ApiDescription;
use dropshot::HttpError;
use dropshot::HttpResponseOk;
use dropshot::HttpResponseUpdatedNoContent;
use dropshot::Path;
use dropshot::RequestContext;
use dropshot::TypedBody;
use illumos_utils::opte::params::DeleteVirtualNetworkInterfaceHost;
use illumos_utils::opte::params::SetVirtualNetworkInterfaceHost;
use omicron_common::api::internal::nexus::DiskRuntimeState;
use omicron_common::api::internal::nexus::InstanceRuntimeState;
use omicron_common::api::internal::nexus::UpdateArtifactId;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

use super::sled_agent::SledAgent;

type SledApiDescription = ApiDescription<Arc<SledAgent>>;

/// Returns a description of the sled agent API
pub fn api() -> SledApiDescription {
    fn register_endpoints(api: &mut SledApiDescription) -> Result<(), String> {
        api.register(instance_put_migration_ids)?;
        api.register(instance_put_state)?;
        api.register(instance_register)?;
        api.register(instance_unregister)?;
        api.register(instance_poke_post)?;
        api.register(disk_put)?;
        api.register(disk_poke_post)?;
        api.register(update_artifact)?;
        api.register(instance_issue_disk_snapshot_request)?;
        api.register(vpc_firewall_rules_put)?;
        api.register(set_v2p)?;
        api.register(del_v2p)?;

        Ok(())
    }

    let mut api = SledApiDescription::new();
    if let Err(err) = register_endpoints(&mut api) {
        panic!("failed to register entrypoints: {}", err);
    }
    api
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
    rqctx: RequestContext<Arc<SledAgent>>,
    path_params: Path<InstancePathParam>,
    body: TypedBody<InstanceEnsureBody>,
) -> Result<HttpResponseOk<InstanceRuntimeState>, HttpError> {
    let sa = rqctx.context();
    let instance_id = path_params.into_inner().instance_id;
    let body_args = body.into_inner();
    Ok(HttpResponseOk(
        sa.instance_register(instance_id, body_args.initial).await?,
    ))
}

#[endpoint {
    method = DELETE,
    path = "/instances/{instance_id}",
}]
async fn instance_unregister(
    rqctx: RequestContext<Arc<SledAgent>>,
    path_params: Path<InstancePathParam>,
) -> Result<HttpResponseOk<InstanceUnregisterResponse>, HttpError> {
    let sa = rqctx.context();
    let instance_id = path_params.into_inner().instance_id;
    Ok(HttpResponseOk(sa.instance_unregister(instance_id).await?))
}

#[endpoint {
    method = PUT,
    path = "/instances/{instance_id}/state",
}]
async fn instance_put_state(
    rqctx: RequestContext<Arc<SledAgent>>,
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
    rqctx: RequestContext<Arc<SledAgent>>,
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

#[endpoint {
    method = POST,
    path = "/instances/{instance_id}/poke",
}]
async fn instance_poke_post(
    rqctx: RequestContext<Arc<SledAgent>>,
    path_params: Path<InstancePathParam>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sa = rqctx.context();
    let instance_id = path_params.into_inner().instance_id;
    sa.instance_poke(instance_id).await;
    Ok(HttpResponseUpdatedNoContent())
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
    rqctx: RequestContext<Arc<SledAgent>>,
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
        .await?,
    ))
}

#[endpoint {
    method = POST,
    path = "/disks/{disk_id}/poke",
}]
async fn disk_poke_post(
    rqctx: RequestContext<Arc<SledAgent>>,
    path_params: Path<DiskPathParam>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sa = rqctx.context();
    let disk_id = path_params.into_inner().disk_id;
    sa.disk_poke(disk_id).await;
    Ok(HttpResponseUpdatedNoContent())
}

#[endpoint {
    method = POST,
    path = "/update"
}]
async fn update_artifact(
    rqctx: RequestContext<Arc<SledAgent>>,
    artifact: TypedBody<UpdateArtifactId>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sa = rqctx.context();
    sa.updates()
        .download_artifact(
            artifact.into_inner(),
            rqctx.context().nexus_client.as_ref(),
        )
        .await
        .map_err(|e| HttpError::for_internal_error(e.to_string()))?;
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
    rqctx: RequestContext<Arc<SledAgent>>,
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
    .await
    .map_err(|e| HttpError::for_internal_error(e.to_string()))?;

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
    rqctx: RequestContext<Arc<SledAgent>>,
    path_params: Path<VpcPathParam>,
    body: TypedBody<VpcFirewallRulesEnsureBody>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let _sa = rqctx.context();
    let _vpc_id = path_params.into_inner().vpc_id;
    let _body_args = body.into_inner();

    Ok(HttpResponseUpdatedNoContent())
}

/// Path parameters for V2P mapping related requests (sled agent API)
#[derive(Deserialize, JsonSchema)]
struct V2pPathParam {
    interface_id: Uuid,
}

/// Create a mapping from a virtual NIC to a physical host
#[endpoint {
    method = PUT,
    path = "/v2p/{interface_id}",
}]
async fn set_v2p(
    rqctx: RequestContext<Arc<SledAgent>>,
    path_params: Path<V2pPathParam>,
    body: TypedBody<SetVirtualNetworkInterfaceHost>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sa = rqctx.context();
    let interface_id = path_params.into_inner().interface_id;
    let body_args = body.into_inner();

    sa.set_virtual_nic_host(interface_id, &body_args)
        .await
        .map_err(|e| HttpError::for_internal_error(e.to_string()))?;

    Ok(HttpResponseUpdatedNoContent())
}

/// Delete a mapping from a virtual NIC to a physical host
#[endpoint {
    method = DELETE,
    path = "/v2p/{interface_id}",
}]
async fn del_v2p(
    rqctx: RequestContext<Arc<SledAgent>>,
    path_params: Path<V2pPathParam>,
    body: TypedBody<DeleteVirtualNetworkInterfaceHost>,
) -> Result<HttpResponseUpdatedNoContent, HttpError> {
    let sa = rqctx.context();
    let interface_id = path_params.into_inner().interface_id;
    let body_args = body.into_inner();

    sa.unset_virtual_nic_host(interface_id, &body_args)
        .await
        .map_err(|e| HttpError::for_internal_error(e.to_string()))?;

    Ok(HttpResponseUpdatedNoContent())
}
