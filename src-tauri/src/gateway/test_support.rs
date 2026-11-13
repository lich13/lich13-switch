//! Entry points on the gateway that only tests call.
use super::*;
use hyper::HeaderMap;
impl Gateway {
    pub fn set_listener_port(&self, port: u16, expected: &str) -> Result<ListenerView> {
        let _lifecycle = self
            .0
            .registry
            .lifecycle
            .try_lock()
            .map_err(|_| AppError::new(code::GATEWAY_ACTIVE, "网关正在启停，请稍后再试"))?;
        let _mutation = self.0.registry.mutation.lock();
        for peer in self.0.registry.gateways() {
            let state = peer.inner.lock();
            for provider in &state.store.providers {
                for url in Self::endpoint_urls(&state.store, provider) {
                    model::base_url(&url, port)?;
                }
            }
        }
        self.0.registry.listener.set_port(port, expected)?;
        self.0.registry.update_ports()?;
        self.0.registry.changed();
        self.try_listener_view()
    }
    pub fn view(&self) -> View {
        self.try_view().expect("healthy test gateway")
    }
    pub fn listener_view(&self) -> ListenerView {
        self.try_listener_view().expect("healthy test listener")
    }
    pub async fn stop(&self) -> Result<View> {
        self.stop_checked(None).await
    }
    pub(crate) fn successful_user_agent(&self) -> Result<hyper::header::HeaderValue> {
        self.request_state()
            .store
            .successful_user_agent
            .as_deref()
            .and_then(|raw| hyper::header::HeaderValue::from_bytes(raw).ok())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                AppError::new(
                    code::STATE,
                    "请先通过该客户端完成一次正常请求，再运行供应商测试",
                )
            })
    }
    pub(super) fn remember(&self, id: &str, provider: &str) {
        self.remember_model(id, provider, None);
    }
    pub(crate) fn test_usage(&self) -> crate::usage::Service {
        self.0.usage.lock().clone().unwrap()
    }
    pub(crate) fn remember_test_user_agent(&self, raw: &[u8]) {
        self.remember_business_identity(testing::BusinessIdentity {
            user_agent: raw.to_vec(),
            codex: None,
        });
    }
    pub(crate) fn remember_test_identity(&self, headers: &HeaderMap) {
        if let Some(identity) = self.business_identity(headers) {
            self.remember_business_identity(identity);
        }
    }
}
