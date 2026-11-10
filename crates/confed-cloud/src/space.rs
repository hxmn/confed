//! Creating a space on Cloud: `POST api/v2/spaces`, with `createPrivateSpace`
//! for one only its creator sees. Cloud answers 404 to an account that may not
//! create spaces, as its API description says.
//!
//! Whether it may is asked of `rest/api/user/current?expand=operations`, whose
//! list holds `create` on `space` (or `create_space`) when it may.

use crate::{v2, CloudClient};
use confed_api::error::{ApiError, ApiResult};
use confed_api::paginate::CursorPage;
use confed_api::types::*;
use confed_api::wire::{server_message, space_refusal};
use serde_json::json;

impl CloudClient {
    /// The space with this key, current or archived.
    pub(crate) async fn cloud_find_space(&self, key: &str) -> ApiResult<Option<Space>> {
        for status in [None, Some("archived")] {
            let mut query = vec![("keys", key.to_string()), ("limit", "1".to_string())];
            if let Some(status) = status {
                query.push(("status", status.to_string()));
            }
            let page: CursorPage<v2::Space> = self.http.get_json("api/v2/spaces", &query).await?;
            if let Some(found) = page.results.into_iter().find(|s| s.key.eq_ignore_ascii_case(key))
            {
                let space = found.into_domain();
                if let Some(numeric) = &space.id.numeric {
                    self.remember_space(&space.id.key, numeric);
                }
                return Ok(Some(space));
            }
        }
        Ok(None)
    }

    pub(crate) async fn cloud_create_space(&self, new: &NewSpace) -> ApiResult<Space> {
        let mut body = json!({ "key": new.key, "name": new.name });
        if let Some(text) = new.description.as_deref().filter(|d| !d.is_empty()) {
            body["description"] = json!({ "value": text, "representation": "plain" });
        }
        if new.private {
            body["createPrivateSpace"] = json!(true);
        }
        let path = "api/v2/spaces";
        let answer = self.http.post_json_answer(path, &body).await?;
        if !(200..300).contains(&answer.status) {
            return Err(refused(answer.status, &answer.body, &new.key));
        }
        let created: v2::Space = serde_json::from_str(&answer.body)
            .map_err(|e| ApiError::Decode { context: format!("POST {path}"), source: e })?;
        let mut space = created.into_domain();
        if let Some(numeric) = &space.id.numeric {
            self.remember_space(&space.id.key, numeric);
        }
        if space.homepage_id.is_none() {
            if let Some(read) = self.cloud_find_space(&new.key).await? {
                space = read;
            }
        }
        Ok(space)
    }

    pub(crate) async fn cloud_probe_create_space(&self) -> ApiResult<Probe> {
        let user: serde_json::Value = self
            .http
            .get_json("rest/api/user/current", &[("expand", "operations".to_string())])
            .await?;
        let Some(operations) = user["operations"].as_array() else {
            return Ok(Probe::Unknown);
        };
        let may = operations.iter().any(|op| {
            let operation = op["operation"].as_str().unwrap_or_default();
            let target = op["targetType"].as_str().unwrap_or_default();
            operation == "create_space" || (operation == "create" && target == "space")
        });
        Ok(if may { Probe::Available } else { Probe::Unavailable })
    }
}

fn refused(status: u16, body: &str, key: &str) -> ApiError {
    let said = server_message(body);
    // Cloud's "you may not create spaces" is a 404.
    let reason =
        if status == 404 { Some(SpaceRefusal::NoPermission) } else { space_refusal(status, body) };
    match reason {
        Some(reason) => ApiError::SpaceRefused {
            reason,
            message: match reason {
                SpaceRefusal::KeyTaken => format!("space key {key} is taken: {said}"),
                SpaceRefusal::InvalidKey => format!("Cloud refused the key {key}: {said}"),
                SpaceRefusal::NoPermission => format!(
                    "this account may not create spaces: it lacks the site's \"Create space\" \
                     permission ({said})"
                ),
            },
        },
        None => ApiError::from_status(status, body.to_string(), "POST api/v2/spaces"),
    }
}
