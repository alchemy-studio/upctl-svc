use std::collections::HashMap;

use axum::body::Bytes;
use axum::extract::{Path, Query};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::Json;
use htycommons::common::{HtyErr, HtyErrCode, HtyResponse};
use htycommons::jwt::jwt_decode_token;
use htycommons::web::{wrap_ok_resp, HtyToken};
use serde::Deserialize;

use crate::config;

type LabelMap = HashMap<String, i64>;

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

/// GET /api/v2/ts/tickets — list issues (proxy to Gitea)
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

    let mut tickets: Vec<serde_json::Value> = serde_json::from_str(&body).map_err(|e| {
        tracing::warn!("[gitea_list_tickets] parse: {e} body={body}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

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
        "claude_prompt_prefix": "不要进入plan mode，直接干活\n\n",
    }))))
}

/// GET /api/v2/ts/tickets/labels — list Gitea labels with colors
pub async fn gitea_list_labels() -> Result<Json<HtyResponse<Vec<serde_json::Value>>>, StatusCode> {
    let client = gitea_client();
    let labels = gitea_label_values(&client).await?;
    Ok(Json(wrap_ok_resp(labels)))
}

/// POST /api/v2/ts/tickets — create new issue
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

/// GET /api/v2/ts/tickets/{id} — get issue detail + comments
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
        "claude_prompt_prefix": "不要进入plan mode，直接干活\n\n",
    });

    Ok(Json(wrap_ok_resp(combined)))
}

/// POST /api/v2/ts/tickets/{id}/comments — add comment
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

/// PATCH /api/v2/ts/tickets/{id} — update issue (labels, state)
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

/// POST /api/v2/ts/tickets/{id}/close — close issue and remove in_progress label
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

/// POST /api/v2/ts/upload_attachment — upload image to local storage
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

    Ok(Json(wrap_ok_resp(serde_json::json!(
        {"url": format!("/api/v2/ts/attachment/{filename}"), "uuid": uuid}
    ))))
}

/// GET /api/v2/ts/attachment/{filename} — serve uploaded file (requires JWT query param)
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

    let jwt_str = params.jwt.as_ref().ok_or((StatusCode::UNAUTHORIZED, "missing jwt"))?;
    let token = jwt_decode_token(jwt_str).map_err(|_| (StatusCode::UNAUTHORIZED, "invalid jwt"))?;
    if !is_admin_or_tester(&token) {
        return Err((StatusCode::FORBIDDEN, "insufficient permissions"));
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
    pub jwt: Option<String>,
}
