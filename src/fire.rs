use crate::arm::ArmedPayload;
use crate::config::AppConfig;
use eyre::Result;
use reqwest::Client;

pub fn rpc_http_client(pool: usize) -> Result<Client> {
    Ok(Client::builder().pool_max_idle_per_host(pool.max(4)).tcp_nodelay(true).build()?)
}
pub async fn prewarm_rpcs(_client: &Client, _rpcs: &[String]) {}
pub async fn fire_armed(_p: &str, _d: bool, _e: i64, _a: Option<i64>) -> Result<()> { Ok(()) }
pub async fn fire_payload(_c: &AppConfig, _p: &ArmedPayload, _d: bool, _cl: Option<Client>, _w: bool) -> Result<()> { Ok(()) }
