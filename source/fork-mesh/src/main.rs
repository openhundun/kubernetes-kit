mod api;
mod controller;

#[tokio::main]
async fn main() -> Result<(), kube::Error> {
    use futures::StreamExt;
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,kube=warn")))
        .init();
    if std::env::args().nth(1).as_deref() == Some("crd") {
        println!("{}", serde_json::to_string_pretty(&crate::api::bare_crd()).unwrap());
        return Ok(());
    }
    let client = kube::Client::try_default().await?;
    require_crd(&client).await?;
    let recorder = kube::runtime::events::Recorder::new(client.clone(), "fork-mesh".into());
    tracing::info!("fork-mesh started");
    kube::runtime::controller::Controller::new(kube::Api::<crate::api::ForkSet>::all(client.clone()), kube::runtime::watcher::Config::default())
        .owns(kube::Api::<k8s_openapi::api::apps::v1::Deployment>::all(client.clone()), kube::runtime::watcher::Config::default())
        .owns(kube::Api::<k8s_openapi::api::core::v1::Service>::all(client.clone()), kube::runtime::watcher::Config::default())
        .owns_with(
            kube::Api::<kube::api::DynamicObject>::all_with(client.clone(), &crate::controller::route_resource()),
            crate::controller::route_resource(),
            kube::runtime::watcher::Config::default(),
        )
        .shutdown_on_signal()
        .run(
            crate::controller::reconcile,
            crate::controller::error_policy,
            std::sync::Arc::new(crate::controller::Ctx { client, recorder }),
        )
        .for_each(|result| async move {
            match result {
                Ok(_) => {}
                Err(kube::runtime::controller::Error::ObjectNotFound { .. }) => {}
                Err(kube::runtime::controller::Error::ReconcilerFailed(kube::Error::Api(status), _)) if status.is_not_found() => {}
                Err(error) => tracing::warn!(%error, "controller failed"),
            }
        })
        .await;
    Ok(())
}

async fn require_crd(client: &kube::Client) -> Result<(), kube::Error> {
    kube::Api::<crate::api::ForkSet>::all(client.clone()).list(&kube::api::ListParams::default().limit(1)).await.map(|_| ())
}
