use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::Json;
use htycommons::common::{HtyErr, HtyErrCode, HtyResponse};
use htycommons::web::{wrap_ok_resp, HtyToken};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;

use crate::config;

type LabelMap = HashMap<String, i64>;

/// Simple URL percent-encoding for query parameters.
fn urlencoding(s: &str) -> String {
    let mut result = String::new();
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            b' ' => result.push_str("%20"),
            _ => result.push_str(&format!("%{:02X}", byte)),
        }
    }
    result
}

fn gitea_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("gitea client")
}

async fn gitea_label_values(
    client: &reqwest::Client,
) -> Result<Vec<serde_json::Value>, StatusCode> {
    let auth = config::gitea_auth_header();
    let resp = client
        .get(format!(
            "{}/repos/weli/tickets/labels",
            config::gitea_api_base()
        ))
        .header("Authorization", auth.as_str())
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("[gitea_labels] reqwest error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let body = resp.text().await.map_err(|e| {
        tracing::warn!("[gitea_labels] read body: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let labels: Vec<serde_json::Value> = serde_json::from_str(&body).map_err(|e| {
        tracing::warn!("[gitea_labels] parse: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(labels)
}

async fn gitea_labels(client: &reqwest::Client) -> Result<LabelMap, StatusCode> {
    let labels = gitea_label_values(client).await?;
    let mut map = LabelMap::new();
    for l in labels {
        if let (Some(name), Some(id)) = (l["name"].as_str(), l["id"].as_i64()) {
            map.insert(name.to_string(), id);
        }
    }
    Ok(map)
}

fn label_names_to_ids(names: &[String], map: &LabelMap) -> Vec<i64> {
    names.iter().filter_map(|n| map.get(n).copied()).collect()
}

fn is_system_admin(token: &HtyToken) -> bool {
    if token
        .roles
        .as_ref()
        .map(|roles| {
            roles.iter().any(|role| {
                matches!(
                    role.role_key.as_deref(),
                    Some("ADMIN" | "ROOT" | "SYS_ADMIN")
                )
            })
        })
        .unwrap_or(false)
    {
        return true;
    }
    token
        .tags
        .as_ref()
        .map(|tags| {
            tags.iter().any(|tag| {
                matches!(
                    tag.tag_name.as_deref(),
                    Some("SYS_ROOT" | "SYS_ADMIN")
                )
            })
        })
        .unwrap_or(false)
}

fn is_tester(token: &HtyToken) -> bool {
    token
        .roles
        .as_ref()
        .map(|roles| {
            roles.iter().any(|role| {
                matches!(role.role_key.as_deref(), Some("TESTER"))
            })
        })
        .unwrap_or(false)
}

fn is_admin_or_tester(token: &HtyToken) -> bool {
    is_system_admin(token) || is_tester(token)
}

fn forbidden_resp(reason: &str) -> HtyResponse<serde_json::Value> {
    HtyResponse {
        r: false,
        d: None,
        e: Some(reason.to_string()),
        hty_err: Some(HtyErr {
            code: HtyErrCode::AuthenticationFailed,
            reason: Some(reason.to_string()),
        }),
    }
}

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct CreateTicketReq {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub labels: Vec<String>,
    pub submitter_name: Option<String>,
}

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct UpdateTicketReq {
    pub state: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub unlabels: Vec<String>,
}

#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct AddCommentReq {
    pub body: String,
    pub submitter_name: Option<String>,
}

/// GET /api/v2/upctl/api/tickets — list issues (proxy to Gitea)
pub async fn gitea_list_tickets(
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<HtyResponse<serde_json::Value>>, StatusCode> {
    let client = gitea_client();
    let auth = config::gitea_auth_header();

    let state = params.get("state").map(|s| s.as_str()).unwrap_or("open");
    let limit = params.get("limit").map(|s| s.as_str()).unwrap_or("50");
    let page = params.get("page").map(|s| s.as_str()).unwrap_or("1");
    let mut url = format!(
        "{}/repos/weli/tickets/issues?state={state}&limit={limit}&page={page}",
        config::gitea_api_base()
    );

    if let Some(labels) = params.get("labels") {
        url = format!("{url}&labels={labels}");
    }

    if let Some(q) = params.get("q") {
        url = format!("{url}&q={}", urlencoding(&q));
    }

    if state == "closed" {
        url = format!("{url}&sort=updated&order=desc");
    } else {
        url = format!("{url}&sort=created&order=desc");
    }

    let resp = client
        .get(&url)
        .header("Authorization", auth.as_str())
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("[gitea_list_tickets] reqwest error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let status = resp.status();
    let body = resp.text().await.map_err(|e| {
        tracing::warn!("[gitea_list_tickets] read body: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if !status.is_success() {
        tracing::warn!("[gitea_list_tickets] non-success status={status} body={body}");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let all_tickets: Vec<serde_json::Value> = serde_json::from_str(&body).map_err(|e| {
        tracing::warn!("[gitea_list_tickets] parse: {e} body={body}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    // Filter out E2E test tickets when hide_e2e=true
    let hide_e2e = params.get("hide_e2e").map(|s| s.as_str() == "true").unwrap_or(false);
    let mut tickets: Vec<serde_json::Value> = if hide_e2e {
        all_tickets
            .into_iter()
            .filter(|t| {
                t.get("title")
                    .and_then(|v| v.as_str())
                    .map(|title| !title.starts_with("E2E"))
                    .unwrap_or(true)
            })
            .collect()
    } else {
        all_tickets
    };

    for ticket in &mut tickets {
        let submitter: Option<String> = ticket
            .get("body")
            .and_then(|b| b.as_str())
            .and_then(|body| body.strip_prefix("> 提交者: "))
            .and_then(|rest| rest.split('\n').next())
            .filter(|n| !n.is_empty())
            .map(|n| n.to_string());
        if let Some(name) = submitter {
            if let Some(user) = ticket.get_mut("user") {
                if let Some(obj) = user.as_object_mut() {
                    obj.insert("login".to_string(), serde_json::Value::String(name));
                }
            }
            if let Some(body) = ticket.get_mut("body") {
                if let Some(s) = body.as_str() {
                    if let Some(rest) = s.strip_prefix("> 提交者: ") {
                        if let Some(idx) = rest.find('\n') {
                            let after = &rest[idx..];
                            *body = serde_json::Value::String(after.trim_start().to_string());
                        }
                    }
                }
            }
        }
    }

    if state == "open" {
        tickets.sort_by(|a, b| {
            let a_urgent = a
                .get("labels")
                .and_then(|l| l.as_array())
                .map(|arr| {
                    arr.iter()
                        .any(|label| label.get("name").and_then(|n| n.as_str()) == Some("urgent"))
                })
                .unwrap_or(false);
            let b_urgent = b
                .get("labels")
                .and_then(|l| l.as_array())
                .map(|arr| {
                    arr.iter()
                        .any(|label| label.get("name").and_then(|n| n.as_str()) == Some("urgent"))
                })
                .unwrap_or(false);
            b_urgent.cmp(&a_urgent)
        });
    }

    Ok(Json(wrap_ok_resp(serde_json::json!({
        "tickets": tickets,
        "claude_prompt_prefix": config::claude_prompt_prefix(),
    }))))
}

/// GET /api/v2/upctl/api/tickets/labels — list Gitea labels with colors
pub async fn gitea_list_labels() -> Result<Json<HtyResponse<Vec<serde_json::Value>>>, StatusCode> {
    let client = gitea_client();
    let labels = gitea_label_values(&client).await?;
    Ok(Json(wrap_ok_resp(labels)))
}

/// POST /api/v2/upctl/api/tickets/{id}/labels — add labels to an issue
#[derive(serde::Deserialize)]
pub struct AddLabelsReq {
    pub labels: Vec<i64>,
}

pub async fn gitea_add_label(
    _token: HtyToken,
    Path(id): Path<String>,
    Json(req): Json<AddLabelsReq>,
) -> Result<Json<HtyResponse<serde_json::Value>>, StatusCode> {
    let client = gitea_client();
    let auth = config::gitea_auth_header();

    let payload = serde_json::json!({
        "labels": req.labels,
    });

    let resp = client
        .post(format!(
            "{}/repos/weli/tickets/issues/{id}/labels",
            config::gitea_api_base()
        ))
        .header("Authorization", auth.as_str())
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("[gitea_add_label] reqwest error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let status = resp.status();
    let resp_body = resp.text().await.map_err(|e| {
        tracing::warn!("[gitea_add_label] read body: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if !status.is_success() {
        tracing::warn!("[gitea_add_label] Gitea returned {status}: {resp_body}");
        return Ok(Json(HtyResponse {
            r: false,
            d: None,
            e: Some(format!("Gitea error {status}: {resp_body}")),
            hty_err: None,
        }));
    }

    let val: serde_json::Value = serde_json::from_str(&resp_body).unwrap_or_default();
    Ok(Json(wrap_ok_resp(val)))
}

/// DELETE /api/v2/upctl/api/tickets/{id}/labels/{label_id}
pub async fn gitea_remove_label(
    _token: HtyToken,
    Path((id, label_id)): Path<(String, String)>,
) -> Result<Json<HtyResponse<serde_json::Value>>, StatusCode> {
    let client = gitea_client();
    let auth = config::gitea_auth_header();

    let resp = client
        .delete(format!(
            "{}/repos/weli/tickets/issues/{id}/labels/{label_id}",
            config::gitea_api_base()
        ))
        .header("Authorization", auth.as_str())
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("[gitea_remove_label] reqwest error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let status = resp.status();
    if !status.is_success() {
        let resp_body = resp.text().await.unwrap_or_default();
        tracing::warn!("[gitea_remove_label] Gitea returned {status}: {resp_body}");
        return Ok(Json(HtyResponse {
            r: false,
            d: None,
            e: Some(format!("Gitea error {status}: {resp_body}")),
            hty_err: None,
        }));
    }

    Ok(Json(HtyResponse {
        r: true,
        d: Some(serde_json::json!({"removed": true})),
        e: None,
        hty_err: None,
    }))
}

/// POST /api/v2/upctl/api/tickets — create new issue
pub async fn gitea_create_ticket(
    token: HtyToken,
    Json(req): Json<CreateTicketReq>,
) -> Result<Json<HtyResponse<serde_json::Value>>, StatusCode> {
    let client = gitea_client();
    let auth = config::gitea_auth_header();

    let body = if let Some(ref name) = req.submitter_name {
        format!("> 提交者: {}\n\n{}", name, req.body)
    } else {
        req.body.clone()
    };

    let label_map = gitea_labels(&client).await.unwrap_or_default();
    let requested_labels = if is_admin_or_tester(&token) {
        req.labels
    } else {
        Vec::new()
    };
    let label_ids = label_names_to_ids(&requested_labels, &label_map);

    let payload = serde_json::json!({
        "title": req.title,
        "body": body,
        "labels": label_ids,
    });

    let resp = client
        .post(format!(
            "{}/repos/weli/tickets/issues",
            config::gitea_api_base()
        ))
        .header("Authorization", auth.as_str())
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("[gitea_create_ticket] reqwest error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let status = resp.status();
    let resp_body = resp.text().await.map_err(|e| {
        tracing::warn!("[gitea_create_ticket] read body: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if !status.is_success() {
        tracing::warn!("[gitea_create_ticket] Gitea returned {status}: {resp_body}");
        return Ok(Json(HtyResponse {
            r: false,
            d: None,
            e: Some(format!("Gitea error {status}: {resp_body}")),
            hty_err: None,
        }));
    }

    let val: serde_json::Value = serde_json::from_str(&resp_body).unwrap_or_default();
    Ok(Json(wrap_ok_resp(val)))
}

/// GET /api/v2/upctl/api/tickets/{id} — get issue detail + comments
pub async fn gitea_get_ticket(
    Path(id): Path<String>,
) -> Result<Json<HtyResponse<serde_json::Value>>, StatusCode> {
    let client = gitea_client();
    let auth = config::gitea_auth_header();

    let (issue_resp, comments_resp) = tokio::join!(
        client
            .get(format!(
                "{}/repos/weli/tickets/issues/{id}",
                config::gitea_api_base()
            ))
            .header("Authorization", auth.as_str())
            .send(),
        client
            .get(format!(
                "{}/repos/weli/tickets/issues/{id}/comments",
                config::gitea_api_base()
            ))
            .header("Authorization", auth.as_str())
            .send(),
    );

    let issue_text = issue_resp
        .map_err(|e| {
            tracing::warn!("[gitea_get_ticket] issue fetch error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .text()
        .await
        .map_err(|e| {
            tracing::warn!("[gitea_get_ticket] issue body: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let comments_text = comments_resp
        .map_err(|e| {
            tracing::warn!("[gitea_get_ticket] comments fetch error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .text()
        .await
        .map_err(|e| {
            tracing::warn!("[gitea_get_ticket] comments body: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut issue_val: serde_json::Value = serde_json::from_str(&issue_text).map_err(|e| {
        tracing::warn!("[gitea_get_ticket] parse issue: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let submitter: Option<String> = issue_val
        .get("body")
        .and_then(|b| b.as_str())
        .and_then(|body| body.strip_prefix("> 提交者: "))
        .and_then(|rest| rest.split('\n').next())
        .filter(|n| !n.is_empty())
        .map(|n| n.to_string());
    if let Some(name) = submitter {
        if let Some(user) = issue_val.get_mut("user") {
            if let Some(obj) = user.as_object_mut() {
                obj.insert("login".to_string(), serde_json::Value::String(name));
            }
        }
        if let Some(body) = issue_val.get_mut("body") {
            if let Some(s) = body.as_str() {
                if let Some(rest) = s.strip_prefix("> 提交者: ") {
                    if let Some(idx) = rest.find('\n') {
                        let after = &rest[idx..];
                        *body = serde_json::Value::String(after.trim_start().to_string());
                    }
                }
            }
        }
    }

    let comments_val: Vec<serde_json::Value> =
        serde_json::from_str(&comments_text).map_err(|e| {
            tracing::warn!("[gitea_get_ticket] parse comments: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let combined = serde_json::json!({
        "issue": issue_val,
        "comments": comments_val,
        "claude_prompt_prefix": config::claude_prompt_prefix(),
    });

    Ok(Json(wrap_ok_resp(combined)))
}

/// POST /api/v2/upctl/api/tickets/{id}/comments — add comment
pub async fn gitea_add_comment(
    Path(id): Path<String>,
    Json(req): Json<AddCommentReq>,
) -> Result<Json<HtyResponse<serde_json::Value>>, StatusCode> {
    let client = gitea_client();
    let auth = config::gitea_auth_header();

    let issue_resp = client
        .get(format!(
            "{}/repos/weli/tickets/issues/{id}",
            config::gitea_api_base()
        ))
        .header("Authorization", auth.as_str())
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("[gitea_add_comment] issue fetch error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let issue_text = issue_resp.text().await.map_err(|e| {
        tracing::warn!("[gitea_add_comment] issue body: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let issue_val: serde_json::Value = serde_json::from_str(&issue_text).map_err(|e| {
        tracing::warn!("[gitea_add_comment] parse issue: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    if issue_val["state"].as_str() != Some("open") {
        return Ok(Json(HtyResponse {
            r: false,
            d: None,
            e: Some("Closed tickets do not accept comments".to_string()),
            hty_err: None,
        }));
    }

    // Comments are allowed on any open ticket — the ai-agent needs to post
    // processing results after adding the in_progress label, and users may
    // need to add context even after approval. Closing the ticket is what
    // prevents further comments (checked above).

    let body = if let Some(ref name) = req.submitter_name {
        format!("> {} \n\n{}", name, req.body)
    } else {
        req.body.clone()
    };

    let payload = serde_json::json!({ "body": body });

    let resp = client
        .post(format!(
            "{}/repos/weli/tickets/issues/{id}/comments",
            config::gitea_api_base()
        ))
        .header("Authorization", auth.as_str())
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("[gitea_add_comment] reqwest error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let status = resp.status();
    let resp_body = resp.text().await.map_err(|e| {
        tracing::warn!("[gitea_add_comment] read body: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    if !status.is_success() {
        return Ok(Json(HtyResponse {
            r: false,
            d: None,
            e: Some(format!("Gitea error {status}: {resp_body}")),
            hty_err: None,
        }));
    }

    let val: serde_json::Value = serde_json::from_str(&resp_body).unwrap_or_default();
    Ok(Json(wrap_ok_resp(val)))
}

/// PATCH /api/v2/upctl/api/tickets/{id} — update issue (labels, state)
pub async fn gitea_update_ticket(
    Path(id): Path<String>,
    token: HtyToken,
    Json(req): Json<UpdateTicketReq>,
) -> Result<(StatusCode, Json<HtyResponse<serde_json::Value>>), StatusCode> {
    if !is_admin_or_tester(&token) {
        tracing::warn!("[gitea_update_ticket] forbidden: user is not admin or tester, token roles={:?}", token.roles.as_ref().map(|r| r.iter().map(|x| x.role_key.as_deref().unwrap_or("?")).collect::<Vec<_>>()));
        return Ok((
            StatusCode::FORBIDDEN,
            Json(forbidden_resp(
                "Only system administrators can update tickets",
            )),
        ));
    }

    let client = gitea_client();
    let auth = config::gitea_auth_header();
    let label_map = gitea_labels(&client).await.unwrap_or_default();

    if let Some(ref new_state) = req.state {
        let payload = serde_json::json!({ "state": new_state });
        let resp = client
            .patch(format!(
                "{}/repos/weli/tickets/issues/{id}",
                config::gitea_api_base()
            ))
            .header("Authorization", auth.as_str())
            .header("Content-Type", "application/json")
            .json(&payload)
            .send()
            .await
            .map_err(|e| {
                tracing::warn!("[gitea_update_ticket] state change error: {e}");
                StatusCode::INTERNAL_SERVER_ERROR
            })?;
        let status = resp.status();
        let resp_body = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            tracing::warn!("[gitea_update_ticket] state change {status}: {resp_body}");
        }
    }

    if !req.labels.is_empty() {
        let label_ids = label_names_to_ids(&req.labels, &label_map);
        if label_ids.is_empty() {
            tracing::warn!("[gitea_update_ticket] add label: no IDs found for names={:?}, label_map keys={:?}", req.labels, label_map.keys().collect::<Vec<_>>());
        } else {
            let payload = serde_json::json!({ "labels": label_ids });
            let resp = client
                .post(format!(
                    "{}/repos/weli/tickets/issues/{id}/labels",
                    config::gitea_api_base()
                ))
                .header("Authorization", auth.as_str())
                .header("Content-Type", "application/json")
                .json(&payload)
                .send()
                .await
                .map_err(|e| {
                    tracing::warn!("[gitea_update_ticket] add label error: {e}");
                    StatusCode::INTERNAL_SERVER_ERROR
                })?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                tracing::warn!("[gitea_update_ticket] add label {status}: {body}");
            }
        }
    }

    if !req.unlabels.is_empty() {
        let label_ids = label_names_to_ids(&req.unlabels, &label_map);
        for lid in label_ids {
            let result = client
                .delete(format!(
                    "{}/repos/weli/tickets/issues/{id}/labels/{lid}",
                    config::gitea_api_base()
                ))
                .header("Authorization", auth.as_str())
                .send()
                .await;
            match result {
                Ok(r) => {
                    let status = r.status();
                    if !status.is_success() {
                        let body = r.text().await.unwrap_or_default();
                        tracing::warn!(
                            "[gitea_update_ticket] remove label non-success status={status} body={body}"
                        );
                    }
                }
                Err(e) => tracing::warn!("[gitea_update_ticket] remove label error: {e}"),
            }
        }
    }

    Ok((
        StatusCode::OK,
        Json(wrap_ok_resp(serde_json::json!({"ok": true}))),
    ))
}

/// POST /api/v2/upctl/api/tickets/{id}/close — close issue and remove in_progress label
pub async fn gitea_close_ticket(
    Path(id): Path<String>,
    token: HtyToken,
) -> Result<Json<HtyResponse<serde_json::Value>>, StatusCode> {
    if !is_admin_or_tester(&token) {
        tracing::warn!("[gitea_close_ticket] forbidden: user is not admin or tester, token roles={:?}", token.roles.as_ref().map(|r| r.iter().map(|x| x.role_key.as_deref().unwrap_or("?")).collect::<Vec<_>>()));
        return Ok(Json(forbidden_resp(
            "Only system administrators can close tickets",
        )));
    }

    let client = gitea_client();
    let auth = config::gitea_auth_header();
    let label_map = gitea_labels(&client).await.unwrap_or_default();

    let payload = serde_json::json!({ "state": "closed" });
    let resp = client
        .patch(format!(
            "{}/repos/weli/tickets/issues/{id}",
            config::gitea_api_base()
        ))
        .header("Authorization", auth.as_str())
        .header("Content-Type", "application/json")
        .json(&payload)
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("[gitea_close_ticket] state change error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        tracing::warn!("[gitea_close_ticket] state change {status}: {body}");
    }

    let in_progress_ids = label_names_to_ids(&["in_progress".to_string()], &label_map);
    for lid in in_progress_ids {
        let resp = client
            .delete(format!(
                "{}/repos/weli/tickets/issues/{id}/labels/{lid}",
                config::gitea_api_base()
            ))
            .header("Authorization", auth.as_str())
            .send()
            .await;
        if let Err(e) = resp {
            tracing::warn!("[gitea_close_ticket] remove in_progress error: {e}");
        }
    }

    Ok(Json(wrap_ok_resp(serde_json::json!({"ok": true}))))
}

fn sign_attachment_token(filename: &str) -> String {
    type HmacSha256 = Hmac<Sha256>;
    let mut mac = HmacSha256::new_from_slice(&config::jwt_key())
        .expect("HMAC key");
    mac.update(filename.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

fn verify_attachment_token(filename: &str, token: &str) -> bool {
    let expected = sign_attachment_token(filename);
    // Constant-time comparison
    expected == token
}

/// POST /api/v2/upctl/api/upload_attachment — upload image to local storage
///
/// Returns a URL with an HMAC-signed access token bound to the filename.
/// The token is generated server-side using JWT_KEY and does not expose
/// the user's JWT. This matches the UPYUN CDN model: file-specific access,
/// not user-specific auth.
pub async fn upload_attachment(
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<HtyResponse<serde_json::Value>>, StatusCode> {
    let mime_type = headers
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream");

    let ext = match mime_type {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "application/pdf" => "pdf",
        "application/msword" => "doc",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document" => "docx",
        "text/plain" => "txt",
        _ => "bin",
    };

    let uuid = uuid::Uuid::new_v4().to_string();
    let filename = format!("{uuid}.{ext}");

    let upload_dir = std::path::Path::new("./uploads");
    tokio::fs::create_dir_all(upload_dir).await.map_err(|e| {
        tracing::warn!("[upload_attachment] create dir: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let filepath = upload_dir.join(&filename);
    tokio::fs::write(&filepath, &body).await.map_err(|e| {
        tracing::warn!("[upload_attachment] write file: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    tracing::info!(
        "[upload_attachment] saved {filename} ({size} bytes)",
        size = body.len()
    );

    let token = sign_attachment_token(&filename);
    let url = format!("/api/v2/upctl/api/attachment/{filename}?token={token}");

    Ok(Json(wrap_ok_resp(serde_json::json!(
        {"url": url, "uuid": uuid}
    ))))
}

/// GET /api/v2/upctl/api/attachment/{filename} — serve uploaded file
///
/// Requires `?token=` query param — an HMAC-SHA256 signature of the filename
/// signed with JWT_KEY. Generated by upload_attachment at upload time.
/// Unlike JWT auth, this token is file-scoped, not user-scoped, and safe
/// to embed in <img> tags for markdown rendering.
pub async fn serve_attachment(
    Path(filename): Path<String>,
    Query(params): Query<AttachmentQuery>,
) -> Result<(HeaderMap, Vec<u8>), (StatusCode, &'static str)> {
    if !filename
        .chars()
        .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err((StatusCode::BAD_REQUEST, "invalid filename"));
    }

    let token = params.token.as_ref().ok_or((StatusCode::UNAUTHORIZED, "missing token"))?;
    if !verify_attachment_token(&filename, token) {
        return Err((StatusCode::FORBIDDEN, "invalid token"));
    }

    let upload_dir = std::path::Path::new("./uploads");
    let filepath = upload_dir.join(&filename);

    let data = tokio::fs::read(&filepath).await.map_err(|_| {
        (StatusCode::NOT_FOUND, "file not found")
    })?;

    let content_type = match filepath
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
    {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    };

    let mut headers = HeaderMap::new();
    headers.insert("Content-Type", HeaderValue::from_static(content_type));

    Ok((headers, data))
}

#[derive(Deserialize)]
pub struct AttachmentQuery {
    pub token: Option<String>,
}


// ── Agent / tmux handlers ─────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct TmuxSendReq {
    pub keys: String,
}

/// GET /api/v2/upctl/api/tmux/{session} — capture tmux pane output
pub async fn agent_capture(
    token: HtyToken,
    Path(session): Path<String>,
) -> Result<Json<HtyResponse<String>>, StatusCode> {
    if !is_admin_or_tester(&token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if !crate::agent::AgentBackend::validate_session(&session) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let backend = crate::agent::AgentBackend::from_env();
    match backend.capture_pane(&session).await {
        Ok(text) => Ok(Json(wrap_ok_resp(text))),
        Err(e) => {
            let msg = e.to_string();
            Ok(Json(HtyResponse {
                r: false,
                d: None,
                e: Some(msg),
                hty_err: None,
            }))
        }
    }
}

/// POST /api/v2/upctl/api/tmux/{session}/send — send keystrokes to tmux session
pub async fn agent_send_keys(
    token: HtyToken,
    Path(session): Path<String>,
    Json(req): Json<TmuxSendReq>,
) -> Result<Json<HtyResponse<String>>, StatusCode> {
    if !is_admin_or_tester(&token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if !crate::agent::AgentBackend::validate_session(&session) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let backend = crate::agent::AgentBackend::from_env();
    match backend.send_keys(&session, &req.keys, false).await {
        Ok(()) => Ok(Json(wrap_ok_resp("keys sent".to_string()))),
        Err(e) => {
            let msg = e.to_string();
            Ok(Json(HtyResponse {
                r: false,
                d: None,
                e: Some(msg),
                hty_err: None,
            }))
        }
    }
}

/// POST /api/v2/upctl/api/agent/prompt — send prompt to agent, wait, capture response
#[derive(serde::Deserialize)]
pub struct AgentPromptReq {
    pub prompt: String,
    pub session: Option<String>,
    pub start_cmd: Option<String>,
    /// Optional ticket number — when provided, ticket + project context is prepended to the prompt
    pub ticket_number: Option<i64>,
    #[serde(default = "default_wait_secs")]
    pub wait_secs: u64,
    /// Dry-run mode: assemble the prompt but don't send to agent. Returns the assembled prompt.
    #[serde(default)]
    pub dry_run: bool,
}

fn default_wait_secs() -> u64 {
    10
}

/// Build a context string from a Gitea ticket and linked projects.
async fn build_ticket_context(ticket_number: i64) -> Result<String, StatusCode> {
    let client = gitea_client();
    let auth = config::gitea_auth_header();

    // Fetch issue
    let issue_resp = client
        .get(format!(
            "{}/repos/weli/tickets/issues/{ticket_number}",
            config::gitea_api_base()
        ))
        .header("Authorization", auth.as_str())
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("[build_ticket_context] fetch error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let issue_val: serde_json::Value = issue_resp.json().await.map_err(|e| {
        tracing::warn!("[build_ticket_context] parse issue: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let title = issue_val["title"].as_str().unwrap_or("(no title)");
    let state = issue_val["state"].as_str().unwrap_or("unknown");
    let body = issue_val["body"].as_str().unwrap_or("");
    let labels: Vec<&str> = issue_val["labels"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|l| l["name"].as_str())
                .collect()
        })
        .unwrap_or_default();

    // Fetch comments
    let comments_val: Vec<serde_json::Value> = client
        .get(format!(
            "{}/repos/weli/tickets/issues/{ticket_number}/comments",
            config::gitea_api_base()
        ))
        .header("Authorization", auth.as_str())
        .send()
        .await
        .map_err(|e| {
            tracing::warn!("[build_ticket_context] comments fetch error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .json()
        .await
        .map_err(|e| {
            tracing::warn!("[build_ticket_context] parse comments: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let mut ctx = String::new();
    ctx.push_str(&format!("# 当前工单 #{ticket_number}\n"));
    ctx.push_str(&format!("标题: {title}\n"));
    ctx.push_str(&format!("状态: {state}\n"));
    if !labels.is_empty() {
        ctx.push_str(&format!("标签: {}\n", labels.join(", ")));
    }
    ctx.push_str(&format!("\n## 工单内容\n{body}\n"));

    // Include comments (non-bot, non-system)
    if !comments_val.is_empty() {
        ctx.push_str("\n## 工单评论\n");
        for comment in &comments_val {
            let user = comment["user"]["login"].as_str().unwrap_or("unknown");
            let cbody = comment["body"].as_str().unwrap_or("");
            let created = comment["created_at"].as_str().unwrap_or("");
            ctx.push_str(&format!("- **{user}** ({created}):\n{cbody}\n\n"));
        }
    }

    // Look for linked projects in the ticket body (## 关联项目 section)
    let projects = read_projects().await.unwrap_or_default();
    let linked: Vec<&Project> = projects
        .iter()
        .filter(|p| body.contains(&p.name))
        .collect();

    if !linked.is_empty() {
        ctx.push_str("\n## 关联项目\n");
        for p in &linked {
            ctx.push_str(&format!("- **{}**", p.name));
            if let Some(ref url) = p.repo_url {
                ctx.push_str(&format!(" ({url})"));
            }
            if p.is_open_source {
                ctx.push_str(" [开源]");
            } else {
                ctx.push_str(" [私有]");
            }
            ctx.push('\n');
            if let Some(ref doc) = p.memory_doc {
                ctx.push_str(&format!(
                    "  Memory: {}...\n",
                    if doc.len() > 200 {
                        format!("{}...", &doc[..200])
                    } else {
                        doc.clone()
                    }
                ));
            }
            // Open source warning
            if p.is_open_source {
                ctx.push_str("  ⚠️ 注意：这是一个开源项目。\n");
                ctx.push_str("  1. 不要有任何密文泄漏（API key、密码、token 等）\n");
                ctx.push_str("  2. 保持业务独立性，不要混入其他业务相关代码\n");
                ctx.push_str("  3. 确保不包含其他项目的业务逻辑\n");
            }
        }
    }

    Ok(ctx)
}

pub async fn agent_prompt(
    token: HtyToken,
    Json(req): Json<AgentPromptReq>,
) -> Result<Json<HtyResponse<String>>, StatusCode> {
    if !is_admin_or_tester(&token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let session = req
        .session
        .unwrap_or_else(|| std::env::var("AGENT_SESSION").or_else(|_| std::env::var("TMUX_DEFAULT_SESSION")).unwrap_or_else(|_| "deepseek".to_string()));
    // Note: TMUX_SESSION_NAME is deliberately NOT used here because tmux
    // automatically sets this variable inside tmux sessions, which can
    // accidentally override the intended agent session name (regression).

    if !crate::agent::AgentBackend::validate_session(&session) {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Build memory instruction — tells the agent where to find project memory files
    let memory_dir = config::agent_memory_dir();
    let memory_instruction = if memory_dir.is_empty() {
        tracing::warn!("[agent_prompt] memory_dir is not configured; memory instruction will be omitted from prompt");
        String::new()
    } else {
        format!(
            "## Memory 上下文\n先读取 memory 文件了解项目背景。执行:\ncat {dir}/MEMORY.md\n\n根据工单内容再有针对性地 cat 具体 memory 文件。\n",
            dir = memory_dir,
        )
    };

    // Build final prompt: if ticket_number is provided, prepend ticket context
    let final_prompt = if let Some(ticket_num) = req.ticket_number {
        match build_ticket_context(ticket_num).await {
            Ok(ctx) => format!(
                "{}\n\n{}\n\n{}\n\n{}",
                config::claude_prompt_prefix(),
                memory_instruction,
                ctx,
                req.prompt,
            ),
            Err(_) => {
                tracing::warn!("[agent_prompt] failed to fetch ticket context for #{}, proceeding without context", ticket_num);
                req.prompt
            }
        }
    } else {
        format!(
            "{}\n\n{}\n\n{}",
            config::claude_prompt_prefix(),
            memory_instruction,
            req.prompt,
        )
    };

    // Dry-run mode: return the assembled prompt without sending to agent
    if req.dry_run {
        return Ok(Json(HtyResponse {
            r: true,
            d: Some(serde_json::json!({
                "assembled_prompt": final_prompt,
                "session": session,
                "ticket_number": req.ticket_number,
            }).to_string()),
            e: None,
            hty_err: None,
        }));
    }

    let backend = crate::agent::AgentBackend::from_env();

    // Ensure session exists (creates if local mode and missing)
    if let Err(e) = backend.ensure_session(&session, req.start_cmd.as_deref()).await {
        let msg = format!("Agent session error: {e}");
        return Ok(Json(HtyResponse {
            r: false,
            d: None,
            e: Some(msg),
            hty_err: None,
        }));
    }

    // Send prompt (two-step: text first, then Enter — for DeepSeek TUI compatibility)
    if let Err(e) = backend.send_prompt(&session, &final_prompt).await {
        let msg = format!("Agent send error: {e}");
        return Ok(Json(HtyResponse {
            r: false,
            d: None,
            e: Some(msg),
            hty_err: None,
        }));
    }

    // Wait for processing
    tokio::time::sleep(std::time::Duration::from_secs(req.wait_secs)).await;

    // Capture response
    match backend.capture_pane(&session).await {
        Ok(text) => Ok(Json(wrap_ok_resp(text))),
        Err(e) => {
            let msg = e.to_string();
            Ok(Json(HtyResponse {
                r: false,
                d: None,
                e: Some(msg),
                hty_err: None,
            }))
        }
    }
}

// ── Config: prompt prefix + memory dir ────────────────────────

/// GET /api/v2/upctl/api/config/prompt-prefix — read current prompt prefix
pub async fn get_prompt_prefix() -> Json<HtyResponse<serde_json::Value>> {
    let prefix = config::claude_prompt_prefix();
    Json(wrap_ok_resp(serde_json::json!({
        "prefix": prefix,
    })))
}

/// PUT /api/v2/upctl/api/config/prompt-prefix — update prompt prefix (ADMIN only)
#[derive(serde::Deserialize)]
pub struct SetPromptPrefixReq {
    pub prefix: String,
}

pub async fn set_prompt_prefix(
    token: HtyToken,
    Json(req): Json<SetPromptPrefixReq>,
) -> Result<Json<HtyResponse<serde_json::Value>>, StatusCode> {
    if !is_admin_or_tester(&token) {
        return Ok(Json(forbidden_resp("Admin role required")));
    }
    match config::set_claude_prompt_prefix(&req.prefix) {
        Ok(actual) => Ok(Json(wrap_ok_resp(serde_json::json!({
            "prefix": actual,
        })))),
        Err(e) => Ok(Json(HtyResponse {
            r: false,
            d: None,
            e: Some(format!("Failed to save: {e}")),
            hty_err: None,
        })),
    }
}

/// GET /api/v2/upctl/api/config/memory-dir — read current memory directory
pub async fn get_memory_dir() -> Json<HtyResponse<serde_json::Value>> {
    let dir = config::agent_memory_dir();
    Json(wrap_ok_resp(serde_json::json!({
        "memory_dir": dir,
    })))
}

/// PUT /api/v2/upctl/api/config/memory-dir — update memory directory (ADMIN only)
#[derive(serde::Deserialize)]
pub struct SetMemoryDirReq {
    pub memory_dir: String,
}

pub async fn set_memory_dir(
    token: HtyToken,
    Json(req): Json<SetMemoryDirReq>,
) -> Result<Json<HtyResponse<serde_json::Value>>, StatusCode> {
    if !is_admin_or_tester(&token) {
        return Ok(Json(forbidden_resp("Admin role required")));
    }
    match config::set_agent_memory_dir(&req.memory_dir) {
        Ok(actual) => Ok(Json(wrap_ok_resp(serde_json::json!({
            "memory_dir": actual,
        })))),
        Err(e) => Ok(Json(HtyResponse {
            r: false,
            d: None,
            e: Some(format!("Failed to save: {e}")),
            hty_err: None,
        })),
    }
}

// ── Project management ─────────────────────────────────────────

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub repo_url: Option<String>,
    pub memory_doc: Option<String>,
    #[serde(default)]
    pub is_open_source: bool,
    #[serde(default)]
    pub is_archived: bool,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(serde::Deserialize)]
pub struct CreateProjectReq {
    pub name: String,
    pub repo_url: Option<String>,
    pub memory_doc: Option<String>,
    #[serde(default)]
    pub is_open_source: bool,
    #[serde(default)]
    pub is_archived: bool,
}

#[derive(serde::Deserialize)]
pub struct UpdateProjectReq {
    pub name: Option<String>,
    pub repo_url: Option<String>,
    pub memory_doc: Option<String>,
    pub is_open_source: Option<bool>,
    pub is_archived: Option<bool>,
}

fn projects_path() -> std::path::PathBuf {
    std::path::Path::new(&crate::config::data_dir()).join("projects.json")
}

async fn read_projects() -> Result<Vec<Project>, StatusCode> {
    let path = projects_path();
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = tokio::fs::read_to_string(&path).await.map_err(|e| {
        tracing::warn!("[projects] read error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    serde_json::from_str(&data).map_err(|e| {
        tracing::warn!("[projects] parse error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })
}

async fn write_projects(projects: &[Project]) -> Result<(), StatusCode> {
    let path = projects_path();
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| {
            tracing::warn!("[projects] create dir error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }
    let data = serde_json::to_string_pretty(projects).map_err(|e| {
        tracing::warn!("[projects] serialize error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    tokio::fs::write(&path, &data).await.map_err(|e| {
        tracing::warn!("[projects] write error: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    Ok(())
}

/// GET /api/v2/upctl/api/projects — list all projects
pub async fn list_projects() -> Json<HtyResponse<Vec<Project>>> {
    let projects = read_projects().await.unwrap_or_default();
    Json(wrap_ok_resp(projects))
}

/// POST /api/v2/upctl/api/projects — create a project (ADMIN only)
pub async fn create_project(
    token: HtyToken,
    Json(req): Json<CreateProjectReq>,
) -> Result<Json<HtyResponse<serde_json::Value>>, StatusCode> {
    if !is_admin_or_tester(&token) {
        return Ok(Json(forbidden_resp("Admin role required")));
    }
    let mut projects = read_projects().await?;
    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    let project = Project {
        id: uuid::Uuid::new_v4().to_string(),
        name: req.name,
        repo_url: req.repo_url,
        memory_doc: req.memory_doc,
        is_open_source: req.is_open_source,
        is_archived: req.is_archived,
        created_at: now.clone(),
        updated_at: now,
    };
    projects.push(project.clone());
    write_projects(&projects).await?;
    Ok(Json(wrap_ok_resp(serde_json::json!(project))))
}

/// PATCH /api/v2/upctl/api/projects/{id} — update a project (ADMIN only)
pub async fn update_project(
    token: HtyToken,
    Path(id): Path<String>,
    Json(req): Json<UpdateProjectReq>,
) -> Result<Json<HtyResponse<serde_json::Value>>, StatusCode> {
    if !is_admin_or_tester(&token) {
        return Ok(Json(forbidden_resp("Admin role required")));
    }
    let mut projects = read_projects().await?;
    let idx = projects.iter().position(|p| p.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string();
    if let Some(name) = req.name {
        projects[idx].name = name;
    }
    if req.repo_url.is_some() {
        projects[idx].repo_url = req.repo_url;
    }
    if req.memory_doc.is_some() {
        projects[idx].memory_doc = req.memory_doc;
    }
    if let Some(val) = req.is_open_source {
        projects[idx].is_open_source = val;
    }
    if let Some(val) = req.is_archived {
        projects[idx].is_archived = val;
    }
    projects[idx].updated_at = now.clone();
    write_projects(&projects).await?;
    Ok(Json(wrap_ok_resp(serde_json::json!(projects[idx]))))
}

/// DELETE /api/v2/upctl/api/projects/{id} — delete a project (ADMIN only)
pub async fn delete_project(
    token: HtyToken,
    Path(id): Path<String>,
) -> Result<Json<HtyResponse<serde_json::Value>>, StatusCode> {
    if !is_admin_or_tester(&token) {
        return Ok(Json(forbidden_resp("Admin role required")));
    }
    let mut projects = read_projects().await?;
    let idx = projects.iter().position(|p| p.id == id)
        .ok_or(StatusCode::NOT_FOUND)?;
    projects.remove(idx);
    write_projects(&projects).await?;
    Ok(Json(wrap_ok_resp(serde_json::json!({"ok": true}))))
}
