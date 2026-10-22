use super::*;

type ProfileInput = (PathBuf, Option<String>, Vec<(PathBuf, String)>);
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileResult {
    pub profile: String,
    pub status: &'static str,
    pub error: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn a_conflict_after_preflight_keeps_success_and_marks_remaining_profiles() {
        let (home, _parent, gateway) = super::super::tests_v02516::fixture();
        let profiles: Vec<String> = ["desktop", "headless", "web"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let inputs = profiles
            .iter()
            .map(|p| {
                let input = Input::read(home.path(), p).unwrap();
                (input.dir.join(PATCH), input.patch.before, input.layers)
            })
            .collect();
        std::fs::write(
            home.path().join("profiles/headless").join(PATCH),
            "# external edit\n[]\n",
        )
        .unwrap();
        let result = gateway
            .apply_dsh_profiles(
                home.path(),
                super::super::tests_v02516::options(),
                &gateway.try_view().unwrap().revision,
                None,
                profiles,
                inputs,
            )
            .await
            .unwrap();
        assert_eq!(
            result.results.iter().map(|r| r.status).collect::<Vec<_>>(),
            ["success", "failed", "pending"]
        );
        assert!(
            std::fs::read_to_string(home.path().join("profiles/desktop").join(PATCH))
                .unwrap()
                .contains(ROUTE)
        );
        assert!(
            !std::fs::read_to_string(home.path().join("profiles/web").join(PATCH))
                .unwrap()
                .contains(ROUTE)
        );
        gateway.stop_dsh(false).await.unwrap();
    }
}
#[derive(Serialize)]
pub struct Batch {
    pub state: View,
    pub results: Vec<ProfileResult>,
}
impl Gateway {
    pub(in crate::gateway) async fn restart_dsh(
        &self,
        home: &Path,
        expected: &str,
    ) -> Result<View> {
        let _listener = self.0.registry.lifecycle.lock().await;
        let _lifecycle = self.0.lifecycle.lock().await;
        self.recover_dsh_state()?;
        let profiles = self
            .0
            .inner
            .lock()
            .unwrap()
            .store
            .dsh
            .as_ref()
            .map(Snapshot::entries)
            .ok_or_else(|| error("请先在 DSH 接入面板选择 profile"))?;
        let mut revision = expected.to_owned();
        for (index, profile) in profiles.values().enumerate() {
            revision = self
                .attach_dsh_locked(home, profile.options.clone(), &revision, None, index == 0)
                .await?
                .revision;
        }
        self.try_view()
    }
    pub async fn attach_dsh_profiles(
        &self,
        home: &Path,
        options: Options,
        all: bool,
        expected: &str,
        preview: Option<&str>,
    ) -> Result<Batch> {
        let _listener = self.0.registry.lifecycle.lock().await;
        let _lifecycle = self.0.lifecycle.lock().await;
        self.recover_dsh_state()?;
        if self.try_view()?.revision != expected {
            return Err(conflict());
        }
        let mut profiles = vec![options.profile.clone()];
        if all {
            profiles.extend(
                selection::profiles(home)?
                    .into_iter()
                    .filter(|p| p != &options.profile),
            );
        }
        // Validate every existing profile before changing the first participant.
        let port = self.0.registry.listener.view()?.port;
        let mut store = self.0.inner.lock().unwrap().store.clone();
        import_selection(home, &options, &mut store, port)?;
        let models = models::catalog();
        let mut inputs = Vec::new();
        for (index, profile) in profiles.iter().enumerate() {
            let mut input = Input::read(home, profile)?;
            if index == 0 && preview.is_some_and(|v| v != input.revision()) {
                return Err(conflict());
            }
            for path in [
                input.dir.join(PATCH),
                home.join(".env"),
                home.join(".credentials.yaml"),
                home.join(PATCH),
                home.join("settings.yaml"),
            ] {
                editor::no_symlinks(home, &path)?;
                if std::fs::metadata(path).is_ok_and(|m| m.permissions().readonly()) {
                    return Err(error("DSH 目标文件只读，尚未修改任何 profile"));
                }
            }
            let previous = store.dsh.as_ref().and_then(|s| s.for_profile(profile));
            prepare_shared(
                &mut input,
                Options {
                    profile: profile.clone(),
                    ..options.clone()
                },
                &store.local_token,
                port,
                &models,
                previous.as_ref(),
                store.dsh.as_ref(),
            )?;
            inputs.push((input.dir.join(PATCH), input.patch.before, input.layers));
        }
        self.apply_dsh_profiles(home, options, expected, preview, profiles, inputs)
            .await
    }
    #[allow(clippy::too_many_arguments)]
    async fn apply_dsh_profiles(
        &self,
        home: &Path,
        options: Options,
        expected: &str,
        preview: Option<&str>,
        profiles: Vec<String>,
        inputs: Vec<ProfileInput>,
    ) -> Result<Batch> {
        let mut results = vec![];
        let mut failed = false;
        let mut revision = expected.to_owned();
        for (index, profile) in profiles.into_iter().enumerate() {
            if failed {
                results.push(ProfileResult {
                    profile,
                    status: "pending",
                    error: None,
                });
                continue;
            }
            let current = Options {
                profile: profile.clone(),
                ..options.clone()
            };
            let unchanged = || -> Result<()> {
                let input = &inputs[index];
                if text(&input.0)? != input.1 {
                    return Err(conflict());
                }
                for (path, raw) in &input.2 {
                    if path != &home.join(PATCH)
                        && path != &home.join("settings.yaml")
                        && text(path)?.as_deref() != Some(raw)
                    {
                        return Err(conflict());
                    }
                }
                Ok(())
            };
            let result = match unchanged() {
                Err(error) => Err(error),
                Ok(()) => {
                    self.attach_dsh_locked(
                        home,
                        current,
                        &revision,
                        if index == 0 { preview } else { None },
                        index == 0,
                    )
                    .await
                }
            };
            match result {
                Ok(state) => {
                    revision = state.revision;
                    results.push(ProfileResult {
                        profile,
                        status: "success",
                        error: None,
                    });
                }
                Err(error) => {
                    failed = true;
                    results.push(ProfileResult {
                        profile,
                        status: "failed",
                        error: Some(error.message),
                    });
                }
            }
        }
        Ok(Batch {
            state: self.try_view()?,
            results,
        })
    }
}
