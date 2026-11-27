//! 纯 Rust DNS 解析，避开 musl 静态二进制的 getaddrinfo 解析失败。
//!
//! reqwest 0.12 的 `hickory-dns` feature 绑定 hickory-resolver 0.25（有 RustSec 公告），
//! teloxide 又锁在 reqwest 0.12，所以直接用 hickory-resolver 0.26 实现 reqwest 的解析接口。

use std::net::SocketAddr;
use std::sync::Arc;

use hickory_resolver::config::LookupIpStrategy;
use hickory_resolver::TokioResolver;
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

pub struct HickoryResolver(TokioResolver);

/// 读取系统 DNS 配置（/etc/resolv.conf）构造解析器，交给 `ClientBuilder::dns_resolver`。
pub fn resolver() -> anyhow::Result<Arc<HickoryResolver>> {
    let mut builder = TokioResolver::builder_tokio()?;
    // 沿用 reqwest 0.11 trust-dns 时的策略：先查 A，没有再查 AAAA（hickory 0.26 默认改成了两者都查）。
    builder.options_mut().ip_strategy = LookupIpStrategy::Ipv4thenIpv6;
    Ok(Arc::new(HickoryResolver(builder.build()?)))
}

impl Resolve for HickoryResolver {
    fn resolve(&self, name: Name) -> Resolving {
        let resolver = self.0.clone();
        Box::pin(async move {
            let lookup = resolver.lookup_ip(name.as_str()).await?;
            let addrs: Addrs = Box::new(lookup.into_iter().map(|ip| SocketAddr::new(ip, 0)));
            Ok(addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolves_through_hickory() {
        let addrs: Vec<_> = resolver()
            .unwrap()
            .resolve("localhost".parse().unwrap())
            .await
            .unwrap()
            .collect();
        assert!(
            addrs.iter().any(|addr| addr.ip().is_loopback()),
            "{addrs:?}"
        );
    }
}
