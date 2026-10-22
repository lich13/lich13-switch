use super::*;
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileResult {
    pub profile: String,
    pub status: &'static str,
    pub error: Option<String>,
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
        for profile in profiles.values() {
            revision = self
                .attach_dsh_locked(home, profile.options.clone(), &revision, None)
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
            match self
                .attach_dsh_locked(
                    home,
                    current,
                    &revision,
                    if index == 0 { preview } else { None },
                )
                .await
            {
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
