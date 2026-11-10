//! Creating a space on Data Center: `POST rest/api/space`, or
//! `rest/api/space/_private` for one only its creator sees. The server makes
//! the home page itself.
//!
//! Data Center has no REST call that says whether an account holds the global
//! "Create Space" permission, so [`DcClient::dc_probe_create_space`] asks the
//! create endpoint itself with a key no server accepts (`!`): it checks the
//! permission before the key, so a refusal of the permission (403) is told
//! apart from a refusal of the key (400), and nothing can be created.

use crate::{encode_path, DcClient};
use confed_api::error::{ApiError, ApiResult};
use confed_api::types::*;
use confed_api::wire::{server_message, space_refusal, v1};
use serde_json::json;

/// The key the permission probe sends: not alphanumeric, so never a space.
const PROBE_KEY: &str = "!";

impl DcClient {
    /// `GET rest/api/space/{key}`, which — unlike the listing — answers for an
    /// archived space as for a current one.
    pub(crate) async fn dc_find_space(&self, key: &str) -> ApiResult<Option<Space>> {
        // `~` is unreserved in a URL, and a personal key starts with it.
        let path = format!("rest/api/space/{}", encode_path(key).replace("%7E", "~"));
        match self.http.get_json::<v1::Space>(&path, &[("expand", "homepage".to_string())]).await {
            Ok(space) => Ok(Some(space.into_domain())),
            Err(ApiError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn dc_create_space(&self, new: &NewSpace) -> ApiResult<Space> {
        let mut body = json!({ "key": new.key, "name": new.name });
        if let Some(text) = new.description.as_deref().filter(|d| !d.is_empty()) {
            body["description"] = json!({ "plain": { "value": text, "representation": "plain" } });
        }
        let path = if new.private { "rest/api/space/_private" } else { "rest/api/space" };
        let answer = self.http.post_json_answer(path, &body).await?;
        if !(200..300).contains(&answer.status) {
            return Err(refused(answer.status, &answer.body, &new.key, path));
        }
        let created: v1::Space = serde_json::from_str(&answer.body)
            .map_err(|e| ApiError::Decode { context: format!("POST {path}"), source: e })?;
        let mut space = created.into_domain();
        // The answer names the home page only when asked to expand it, which
        // a create does not take: read the space back for it.
        if space.homepage_id.is_none() {
            if let Some(read) = self.dc_find_space(&new.key).await? {
                space = read;
            }
        }
        Ok(space)
    }

    pub(crate) async fn dc_probe_create_space(&self) -> ApiResult<Probe> {
        let body = json!({ "key": PROBE_KEY, "name": "confed permission check" });
        let answer = self.http.post_json_answer("rest/api/space", &body).await?;
        Ok(match answer.status {
            401 => {
                return Err(ApiError::Auth("POST rest/api/space: HTTP 401".into()));
            }
            400 => match space_refusal(400, &answer.body) {
                Some(SpaceRefusal::NoPermission) => Probe::Unavailable,
                _ => Probe::Available,
            },
            403 => Probe::Unavailable,
            _ => Probe::Unknown,
        })
    }
}

/// A refused create, as the error confed reports.
fn refused(status: u16, body: &str, key: &str, path: &str) -> ApiError {
    let said = server_message(body);
    match space_refusal(status, body) {
        Some(reason) => ApiError::SpaceRefused {
            reason,
            message: match reason {
                SpaceRefusal::KeyTaken => format!("space key {key} is taken: {said}"),
                SpaceRefusal::InvalidKey => format!("Data Center refused the key {key}: {said}"),
                SpaceRefusal::NoPermission => format!(
                    "this account may not create spaces: it lacks Data Center's global \
                     \"Create Space\" permission ({said})"
                ),
            },
        },
        None => ApiError::from_status(status, body.to_string(), &format!("POST {path}")),
    }
}
