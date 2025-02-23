//! Nexus integration tests
//!
//! See the driver in the parent directory for how and why this is structured
//! the way it is.

mod address_lots;
mod authn_http;
mod authz;
mod basic;
mod certificates;
mod commands;
mod console_api;
mod device_auth;
mod disks;
mod images;
mod initialization;
mod instances;
mod ip_pools;
mod loopback_address;
mod metrics;
mod oximeter;
mod pantry;
mod password_login;
mod projects;
mod rack;
mod role_assignments;
mod roles_builtin;
mod router_routes;
mod saml;
mod schema;
mod silo_users;
mod silos;
mod sleds;
mod snapshots;
mod ssh_keys;
mod subnet_allocation;
mod switch_port;
mod system_updates;
mod unauthorized;
mod unauthorized_coverage;
mod updates;
mod users_builtin;
mod volume_management;
mod vpc_firewall;
mod vpc_routers;
mod vpc_subnets;
mod vpcs;
mod zpools;

// This module is used only for shared data, not test cases.
mod endpoints;
