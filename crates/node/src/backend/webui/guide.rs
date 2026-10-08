//! The first run guide: which hint the interface shows a machine that has
//! just created its local network.
//!
//! The node only keeps the progress (`Config::first_run_guide`), so the hints
//! follow the machine and not the browser. The interface decides where each
//! hint points and reports a step as done or skipped; when the last one is
//! done, or the operator hides the guide, the progress is dropped and the
//! guide never comes back.

use super::*;

/// The steps, in the order the interface walks them.
pub(crate) const GUIDE_STEPS: [&str; 6] = [
    "network_name",
    "machine_name",
    "download_model",
    "deploy_model",
    "chat",
    "join",
];

#[derive(Debug, Deserialize)]
pub struct GuideUpdate {
    /// Id of a step that was done or skipped.
    #[serde(default)]
    pub done: Option<String>,
    /// Ends the guide whatever is left of it.
    #[serde(default)]
    pub dismiss: bool,
}

fn guide_view(progress: Option<&Vec<String>>) -> serde_json::Value {
    json!({
        "active": progress.is_some(),
        "steps": GUIDE_STEPS,
        "done": progress.cloned().unwrap_or_default(),
    })
}

/// Applies one update to the stored progress. `None` is a guide that ended.
pub(crate) fn advance_guide(
    progress: Option<Vec<String>>,
    update: &GuideUpdate,
) -> Result<Option<Vec<String>>, String> {
    let Some(mut done) = progress else {
        return Ok(None);
    };
    if update.dismiss {
        return Ok(None);
    }
    if let Some(step) = update.done.as_deref() {
        if !GUIDE_STEPS.contains(&step) {
            return Err(format!("unknown guide step: {step}"));
        }
        if !done.iter().any(|known| known == step) {
            done.push(step.to_string());
        }
    }
    if GUIDE_STEPS.iter().all(|step| done.iter().any(|known| known == step)) {
        return Ok(None);
    }
    Ok(Some(done))
}

pub(super) async fn get_guide(_auth: ConnectionAuth, State(state): State<GatewayState>) -> Response {
    Json(guide_view(state.auth.read().await.first_run_guide.as_ref())).into_response()
}

pub(super) async fn update_guide(
    _auth: ConnectionAuth,
    State(state): State<GatewayState>,
    Json(update): Json<GuideUpdate>,
) -> Response {
    let mut config = state.auth.write().await;
    let next = match advance_guide(config.first_run_guide.clone(), &update) {
        Ok(next) => next,
        Err(error) => return bad_request(error),
    };
    if next != config.first_run_guide {
        let previous = std::mem::replace(&mut config.first_run_guide, next);
        if let Err(error) = config.save() {
            config.first_run_guide = previous;
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("could not save the guide progress: {error:#}") })),
            )
                .into_response();
        }
    }
    Json(guide_view(config.first_run_guide.as_ref())).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done(step: &str) -> GuideUpdate {
        GuideUpdate {
            done: Some(step.to_string()),
            dismiss: false,
        }
    }

    #[test]
    fn the_guide_ends_with_its_last_step_and_never_restarts() {
        let mut progress = Some(Vec::new());
        let (last, before) = GUIDE_STEPS.split_last().unwrap();
        for step in before {
            progress = advance_guide(progress, &done(step)).unwrap();
            assert!(progress.is_some());
        }
        // Repeating a step neither duplicates it nor ends the guide.
        progress = advance_guide(progress, &done(GUIDE_STEPS[0])).unwrap();
        assert_eq!(progress.as_ref().map(Vec::len), Some(before.len()));
        progress = advance_guide(progress, &done(last)).unwrap();
        assert_eq!(progress, None);
        assert_eq!(advance_guide(None, &done(GUIDE_STEPS[0])).unwrap(), None);
    }

    #[test]
    fn hiding_the_guide_ends_it_and_an_unknown_step_is_refused() {
        let hide = GuideUpdate {
            done: None,
            dismiss: true,
        };
        assert_eq!(advance_guide(Some(Vec::new()), &hide).unwrap(), None);
        assert!(advance_guide(Some(Vec::new()), &done("wizard")).is_err());
    }
}
