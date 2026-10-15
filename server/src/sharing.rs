//! HTTP API for creating shares, issuing tickets and mounting them.

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::ids::valid_name;
use crate::storage::{ShareInfo, User};
use crate::ticket::Ticket;
use crate::ws::authenticate;
use crate::SharedState;

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

fn internal(e: anyhow::Error) -> Response {
    tracing::error!("sharing api: {e:#}");
    err(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Normalise a user-typed path to a folder prefix ("a/b/") or a file key ("a/b.md").
fn clean_path(p: &str) -> String {
    p.trim().trim_matches('/').replace('\\', "/")
}

fn overlaps(a: &str, b: &str) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

fn user_vault(state: &SharedState, user: &User, vault: &str) -> Result<(crate::storage::VaultInfo, std::sync::Arc<crate::vault::LiveDoc>), Response> {
    if !valid_name(vault) {
        return Err(err(StatusCode::BAD_REQUEST, "invalid vault name"));
    }
    let doc = state.docs.get_or_load(user, vault).map_err(internal)?;
    let info = doc.vault_info().cloned().ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "not a vault"))?;
    Ok((info, doc))
}

fn taken_prefixes(state: &SharedState, vault_id: &str) -> anyhow::Result<Vec<String>> {
    let reg = state.storage.registry();
    let mut out: Vec<String> = reg.shares_for_vault(vault_id)?.into_iter().map(|s| match s.only_file {
        Some(f) => format!("{}{}", s.prefix, f),
        None => s.prefix,
    }).collect();
    out.extend(reg.mounts_for_vault(vault_id)?.into_iter().filter(|m| m.revoked_at.is_none()).map(|m| m.mount_prefix));
    Ok(out)
}

// ------------------------------------------------------------------ GET /api/vaults/{vault}/sharing

pub async fn get_sharing(
    Path(vault): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let (info, _) = match user_vault(&state, &user, &vault) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let reg = state.storage.registry();
    let shares = match reg.shares_for_vault(&info.id) {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    let mut shares_json = Vec::new();
    for s in shares {
        let grants = reg.grants_for_share(&s.id).unwrap_or_default();
        let all_mounts = reg.mounts_for_share(&s.id).unwrap_or_default();
        let mounts = all_mounts.iter().filter(|m| m.revoked_at.is_none()).count();
        // Local mounts carry the ticket secret, so they can be attributed to a ticket.
        let grants: Vec<serde_json::Value> = grants
            .iter()
            .map(|g| {
                let hash = reg.grant_secret_hash(&s.id, &g.label).ok().flatten();
                let mounted_by = all_mounts
                    .iter()
                    .filter(|m| m.revoked_at.is_none() && hash.as_deref() == Some(crate::ids::token_hash(&m.secret).as_str()))
                    .count();
                serde_json::json!({ "label": g.label, "access": g.access, "created_at": g.created_at, "revoked_at": g.revoked_at, "mounted_by": mounted_by })
            })
            .collect();
        shares_json.push(serde_json::json!({
            "id": s.id, "name": s.name, "prefix": s.prefix, "only_file": s.only_file,
            "path": match &s.only_file { Some(f) => format!("{}{}", s.prefix, f), None => s.prefix.trim_end_matches('/').to_string() },
            "created_at": s.created_at, "grants": grants, "mounted_by": mounts,
        }));
    }
    let mounts = reg.mounts_for_vault(&info.id).unwrap_or_default();
    let mounts_json: Vec<serde_json::Value> = mounts
        .iter()
        .map(|m| {
            let status = if m.revoked_at.is_some() {
                "revoked".to_string()
            } else if m.home_node != state.docs.node_id {
                format!("remote: {}", state.docs.link_status(&m.id).unwrap_or_else(|| "starting".into()))
            } else {
                "active".to_string()
            };
            serde_json::json!({
                "id": m.id, "share_id": m.share_id, "name": m.share_name, "mount_path": m.mount_prefix.trim_end_matches('/'),
                "access": m.access, "home_node": m.home_node, "status": status, "created_at": m.created_at,
            })
        })
        .collect();
    Json(serde_json::json!({ "node_id": state.docs.node_id, "shares": shares_json, "mounts": mounts_json })).into_response()
}

// ------------------------------------------------------------------ POST /api/vaults/{vault}/shares

#[derive(Deserialize)]
pub struct CreateShare {
    pub path: String,
    pub name: Option<String>,
    pub access: Option<String>,
}

fn make_ticket(state: &SharedState, share: &ShareInfo, secret: String, access: &str) -> anyhow::Result<String> {
    Ticket {
        version: 1,
        share_id: share.id.clone(),
        name: share.name.clone(),
        node_id: state.docs.node_id.clone(),
        secret,
        access: access.to_string(),
        addrs: state.docs.endpoint.as_ref().map(crate::remote::addr_strings).unwrap_or_default(),
    }
    .encode()
}

pub async fn create_share(
    Path(vault): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    Json(body): Json<CreateShare>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let (info, doc) = match user_vault(&state, &user, &vault) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let path = clean_path(&body.path);
    if path.is_empty() {
        return err(StatusCode::BAD_REQUEST, "path is required (a folder or a note)");
    }
    let access = body.access.unwrap_or_else(|| "rw".into());
    if access != "rw" && access != "ro" {
        return err(StatusCode::BAD_REQUEST, "access must be rw or ro");
    }

    // Folder or note? A note is an existing key; a folder has keys below it.
    let keys = doc.keys();
    let (prefix, only_file) = if keys.iter().any(|k| k == &path) {
        match path.rsplit_once('/') {
            Some((dir, file)) => (format!("{dir}/"), Some(file.to_string())),
            None => (String::new(), Some(path.clone())),
        }
    } else {
        let p = format!("{path}/");
        if !keys.iter().any(|k| k.starts_with(&p)) {
            return err(StatusCode::NOT_FOUND, format!("'{path}' is neither a synced note nor a folder with synced files"));
        }
        (p, None)
    };
    let full = match &only_file {
        Some(f) => format!("{prefix}{f}"),
        None => prefix.clone(),
    };
    // Check + insert + attach under one lock, otherwise two concurrent creates could both pass.
    let _admin = state.docs.share_admin.lock().unwrap();
    match taken_prefixes(&state, &info.id) {
        Ok(taken) => {
            if taken.iter().any(|t| overlaps(t, &full)) {
                return err(StatusCode::CONFLICT, "this path overlaps an existing share or mount");
            }
        }
        Err(e) => return internal(e),
    }
    let name = body
        .name
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(&path).to_string());

    let reg = state.storage.registry();
    let share = match reg.create_share(&user.id, &info.id, &name, &prefix, only_file.as_deref()) {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    let secret = match reg.create_grant(&share.id, "ticket-1", &access) {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    if let Err(e) = state.docs.add_owner_bridge(&share) {
        let _ = reg.delete_share(&share.id);
        return internal(e);
    }
    let ticket = match make_ticket(&state, &share, secret, &access) {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    tracing::info!(user = %user.name, share = %share.name, "share created for {full}");
    Json(serde_json::json!({ "share": share, "ticket": ticket, "label": "ticket-1" })).into_response()
}

// ------------------------------------------------------------------ POST /api/shares/{id}/tickets

#[derive(Deserialize, Default)]
pub struct NewTicket {
    pub label: Option<String>,
    pub access: Option<String>,
}

fn owned_share(state: &SharedState, user: &User, id: &str) -> Result<ShareInfo, Response> {
    match state.storage.registry().share_by_id(id) {
        Ok(Some(s)) if s.user_id == user.id => Ok(s),
        Ok(_) => Err(err(StatusCode::NOT_FOUND, "no such share")),
        Err(e) => Err(internal(e)),
    }
}

pub async fn new_ticket(
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    body: Option<Json<NewTicket>>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let share = match owned_share(&state, &user, &id) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let body = body.map(|b| b.0).unwrap_or_default();
    let access = body.access.unwrap_or_else(|| "rw".into());
    if access != "rw" && access != "ro" {
        return err(StatusCode::BAD_REQUEST, "access must be rw or ro");
    }
    let reg = state.storage.registry();
    let label = body.label.filter(|l| !l.trim().is_empty()).unwrap_or_else(|| {
        let n = reg.grants_for_share(&share.id).map(|g| g.len()).unwrap_or(0) + 1;
        format!("ticket-{n}")
    });
    let secret = match reg.create_grant(&share.id, &label, &access) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::CONFLICT, e.to_string()),
    };
    match make_ticket(&state, &share, secret, &access) {
        Ok(t) => Json(serde_json::json!({ "ticket": t, "label": label, "access": access })).into_response(),
        Err(e) => internal(e),
    }
}

// ------------------------------------------------------------------ DELETE /api/shares/{id}/grants/{label}

pub async fn revoke_ticket(
    Path((id, label)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let share = match owned_share(&state, &user, &id) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let reg = state.storage.registry();
    let hash = match reg.grant_secret_hash(&share.id, &label) {
        Ok(Some(h)) => h,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such ticket"),
        Err(e) => return internal(e),
    };
    if let Err(e) = reg.revoke_grant(&share.id, &label) {
        return internal(e);
    }
    match reg.revoke_mounts_with_secret(&share.id, &hash) {
        Ok(ids) => state.docs.remove_bridges_for_mounts(&ids),
        Err(e) => return internal(e),
    }
    StatusCode::NO_CONTENT.into_response()
}

// ------------------------------------------------------------------ DELETE /api/shares/{id}

pub async fn delete_share(
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let share = match owned_share(&state, &user, &id) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let reg = state.storage.registry();
    let mount_ids: Vec<String> = reg.mounts_for_share(&share.id).unwrap_or_default().into_iter().map(|m| m.id).collect();
    state.docs.remove_bridges_for_mounts(&mount_ids);
    state.docs.remove_bridges_for_share(&share.id);
    for m in &mount_ids {
        let _ = reg.delete_mount(m);
    }
    if let Err(e) = reg.delete_share(&share.id) {
        return internal(e);
    }
    let dir = state.storage.share_dir(&share.user_id, &share.id);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        tracing::warn!("removing share dir {} failed: {e}", dir.display());
    }
    tracing::info!(user = %user.name, share = %share.name, "share deleted");
    StatusCode::NO_CONTENT.into_response()
}

// ------------------------------------------------------------------ POST /api/vaults/{vault}/mounts

#[derive(Deserialize)]
pub struct CreateMount {
    pub ticket: String,
    pub mount_path: Option<String>,
}

pub async fn create_mount(
    Path(vault): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    Json(body): Json<CreateMount>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let (info, doc) = match user_vault(&state, &user, &vault) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let ticket = match Ticket::decode(&body.ticket) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("bad ticket: {e}")),
    };
    let reg = state.storage.registry();
    let remote = ticket.node_id != state.docs.node_id;
    // What the owner granted: (label for the log, access, share name, single-note flag).
    let (label, grant_access, share_name, only_file) = if remote {
        let Some(ep) = state.docs.endpoint.as_ref() else {
            return err(StatusCode::NOT_IMPLEMENTED, "this ticket is for a share on another server and p2p is disabled on this one (--no-p2p)");
        };
        match crate::remote::probe(ep, &ticket).await {
            Ok((access, name, only_file)) => ("p2p".to_string(), access, name, only_file),
            Err(e) => return err(StatusCode::BAD_GATEWAY, e.to_string()),
        }
    } else {
        let share = match reg.share_by_id(&ticket.share_id) {
            Ok(Some(s)) => s,
            Ok(None) => return err(StatusCode::NOT_FOUND, "the share in this ticket no longer exists"),
            Err(e) => return internal(e),
        };
        if share.vault_id == info.id {
            return err(StatusCode::BAD_REQUEST, "you cannot mount your own share into the same vault");
        }
        match reg.check_grant(&share.id, &ticket.secret) {
            Ok(Some((label, access))) => (label, access, share.name.clone(), share.only_file.clone()),
            Ok(None) => return err(StatusCode::FORBIDDEN, "ticket is invalid or has been revoked"),
            Err(e) => return internal(e),
        }
    };
    let access = if grant_access == "ro" || ticket.access == "ro" { "ro" } else { "rw" };

    let mount_path = clean_path(&body.mount_path.unwrap_or_default());
    let mount_path = if mount_path.is_empty() { format!("Shared/{share_name}") } else { mount_path };
    let prefix = format!("{mount_path}/");
    let _admin = state.docs.share_admin.lock().unwrap();
    match taken_prefixes(&state, &info.id) {
        Ok(taken) => {
            if taken.iter().any(|t| overlaps(t, &prefix)) {
                return err(StatusCode::CONFLICT, "mount path overlaps an existing share or mount");
            }
        }
        Err(e) => return internal(e),
    }
    if doc.keys().iter().any(|k| k.starts_with(&prefix)) {
        return err(StatusCode::CONFLICT, format!("'{mount_path}' already contains files; choose an empty or new folder"));
    }
    let mount = match reg.create_mount(
        &user.id,
        &info.id,
        &ticket.share_id,
        &share_name,
        &prefix,
        access,
        &ticket.node_id,
        &ticket.addrs,
        only_file.as_deref(),
        &ticket.secret,
    ) {
        Ok(m) => m,
        Err(e) => return internal(e),
    };
    if let Err(e) = state.docs.add_mount_bridge(&mount) {
        let _ = reg.delete_mount(&mount.id);
        return internal(e);
    }
    tracing::info!(user = %user.name, share = %share_name, remote, "mounted at {prefix} via {label} ({access})");
    Json(serde_json::json!({ "mount": mount, "mount_path": mount_path, "access": access })).into_response()
}

// ------------------------------------------------------------------ POST /api/tickets/inspect

#[derive(Deserialize)]
pub struct InspectTicket {
    pub ticket: String,
}

/// Tell the UI what a pasted ticket is before mounting it: name, access, which server it
/// belongs to and, for shares on this server, whether it is still valid.
pub async fn inspect_ticket(
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    Json(body): Json<InspectTicket>,
) -> Response {
    if let Err(r) = authenticate(&state, &headers, &query) {
        return r;
    }
    let ticket = match Ticket::decode(&body.ticket) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("bad ticket: {e}")),
    };
    let same_server = ticket.node_id == state.docs.node_id;
    let (valid, access, own) = if same_server {
        let reg = state.storage.registry();
        match reg.share_by_id(&ticket.share_id) {
            Ok(Some(s)) => match reg.check_grant(&s.id, &ticket.secret) {
                Ok(Some((_, grant_access))) => {
                    let a = if grant_access == "ro" || ticket.access == "ro" { "ro" } else { "rw" };
                    (Some(true), a.to_string(), Some(s.vault_id))
                }
                _ => (Some(false), ticket.access.clone(), None),
            },
            _ => (Some(false), ticket.access.clone(), None),
        }
    } else {
        (None, ticket.access.clone(), None)
    };
    let own_vault = own
        .and_then(|vid| state.storage.registry().vault_by_id(&vid).ok().flatten())
        .map(|v| v.name);
    Json(serde_json::json!({
        "share_id": ticket.share_id,
        "name": ticket.name,
        "access": access,
        "node_id": ticket.node_id,
        "same_server": same_server,
        "p2p_available": state.docs.endpoint.is_some(),
        "valid": valid,
        "from_own_vault": own_vault,
    }))
    .into_response()
}

// ------------------------------------------------------------------ DELETE /api/mounts/{id}

pub async fn delete_mount(
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let reg = state.storage.registry();
    let mount = match reg.mount_by_id(&id) {
        Ok(Some(m)) if m.user_id == user.id => m,
        Ok(_) => return err(StatusCode::NOT_FOUND, "no such mount"),
        Err(e) => return internal(e),
    };
    state.docs.remove_bridges_for_mounts(&[mount.id.clone()]);
    if let Err(e) = reg.delete_mount(&mount.id) {
        return internal(e);
    }
    if mount.home_node != state.docs.node_id {
        let dir = state.storage.mount_dir(&mount.user_id, &mount.id);
        if dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::warn!("removing replica dir {} failed: {e}", dir.display());
            }
        }
    }
    tracing::info!(user = %user.name, "unmounted {}", mount.mount_prefix);
    StatusCode::NO_CONTENT.into_response()
}
