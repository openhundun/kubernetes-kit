pub const REQUEUE: std::time::Duration = std::time::Duration::from_secs(300);
pub const RETRY: std::time::Duration = std::time::Duration::from_secs(30);

pub struct Ctx {
    pub client: kube::Client,
    pub recorder: kube::runtime::events::Recorder,
}

const REGEX_META: &str = "\\^$.*+?()[]{}|";

const ROUTE_API_VERSION: &str = "gateway.networking.k8s.io/v1";
const ROUTE_KIND: &str = "HTTPRoute";

#[derive(Clone, Debug)]
struct BranchPlan {
    branch: String,
    deployment: k8s_openapi::api::apps::v1::Deployment,
    service: k8s_openapi::api::core::v1::Service,
    httproute: serde_json::Value,
}

#[derive(Clone, Debug, Default)]
struct Plan {
    branches: Vec<BranchPlan>,
    skipped: Vec<(String, crate::api::Condition)>,
}

fn plan(forkset: &crate::api::ForkSet) -> Plan {
    use kube::Resource;
    use kube::ResourceExt;
    let namespace = forkset.namespace().unwrap_or_default();
    let spec = &forkset.spec;
    let owners = forkset.controller_owner_ref(&()).map(|owner| vec![owner]).unwrap_or_default();
    let planned = spec.branches.iter().map(|entry| (entry, branch_plan(spec, entry, &namespace, &owners))).collect::<Vec<_>>();
    Plan {
        branches: planned.iter().filter_map(|(_, branch)| branch.clone()).collect(),
        skipped: planned
            .iter()
            .filter(|(_, branch)| branch.is_none())
            .map(|(entry, _)| {
                (
                    entry.branch.clone(),
                    crate::api::Condition {
                        type_: crate::api::CONDITION_SOURCE_CONTAINER.to_string(),
                        status: "False".to_string(),
                        reason: "MissingSourceContainer".to_string(),
                        message: format!("pod spec has no container named {}", spec.source),
                    },
                )
            })
            .collect(),
    }
}

fn orphan_names<K: kube::Resource + kube::ResourceExt>(items: &[K], uid: &str, desired: &std::collections::BTreeSet<&str>) -> Vec<String> {
    let mut names = items
        .iter()
        .filter(|item| {
            let owned = item.meta().owner_references.as_ref().is_some_and(|owners| owners.iter().any(|owner| owner.uid == uid));
            let branch = item.meta().labels.as_ref().and_then(|labels| labels.get(crate::api::LABEL_BRANCH));
            owned && branch.is_some_and(|branch| !desired.contains(branch.as_str()))
        })
        .map(kube::ResourceExt::name_any)
        .collect::<Vec<String>>();
    names.sort();
    names
}

fn child_name(source: &str, branch: &str) -> String {
    format!("{source}-{branch}")
}

fn escape_regex(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| REGEX_META.contains(character).then_some('\\').into_iter().chain([character]))
        .collect()
}

fn image_tag(image: &str) -> Option<&str> {
    let (_, tag) = image.rsplit_once(':')?;
    (!image.contains('@') && !tag.is_empty() && !tag.contains('/')).then_some(tag)
}

fn branch_plan(spec: &crate::api::ForkSetSpec, entry: &crate::api::BranchSpec, namespace: &str, owners: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference]) -> Option<BranchPlan> {
    spec.pod_spec.containers.iter().any(|container| container.name == spec.source).then(|| {
        let name = child_name(&spec.source, &entry.branch);
        let version = image_tag(&entry.image).unwrap_or_default().to_string();
        let labels = std::collections::BTreeMap::from([
            (crate::api::LABEL_SOURCE.to_string(), spec.source.clone()),
            (crate::api::LABEL_BRANCH.to_string(), entry.branch.clone()),
            (crate::api::LABEL_VERSION.to_string(), version),
        ]);
        let replicas = entry.replicas.unwrap_or(1);
        let deployment = build_deployment(&name, namespace, spec, entry, &labels, owners, replicas);
        let service = build_service(&name, namespace, spec, &labels, owners);
        let httproute = build_httproute(&name, namespace, spec, &labels, owners, &entry.branch);
        BranchPlan { branch: entry.branch.clone(), deployment, service, httproute }
    })
}

fn identity_selector(labels: &std::collections::BTreeMap<String, String>) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        (crate::api::LABEL_SOURCE.to_string(), labels[crate::api::LABEL_SOURCE].clone()),
        (crate::api::LABEL_BRANCH.to_string(), labels[crate::api::LABEL_BRANCH].clone()),
    ])
}

fn object_meta(
    name: &str,
    namespace: &str,
    labels: &std::collections::BTreeMap<String, String>,
    owners: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference],
) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
    k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
        name: Some(name.to_string()),
        namespace: Some(namespace.to_string()),
        labels: Some(labels.clone()),
        owner_references: Some(owners.to_vec()),
        ..Default::default()
    }
}

fn build_deployment(
    name: &str,
    namespace: &str,
    spec: &crate::api::ForkSetSpec,
    entry: &crate::api::BranchSpec,
    labels: &std::collections::BTreeMap<String, String>,
    owners: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference],
    replicas: i32,
) -> k8s_openapi::api::apps::v1::Deployment {
    let version = image_tag(&entry.image).unwrap_or_default();
    let mut pod_spec = spec.pod_spec.clone();
    pod_spec.containers.iter_mut().filter(|container| container.name == spec.source).for_each(|container| {
        container.image = Some(entry.image.clone());
        upsert_env(container, "SOURCE", &spec.source);
        upsert_env(container, "BRANCH", &entry.branch);
        upsert_env(container, "VERSION", version);
    });
    let template = k8s_openapi::api::core::v1::PodTemplateSpec {
        metadata: Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta { labels: Some(labels.clone()), ..Default::default() }),
        spec: Some(pod_spec),
    };
    k8s_openapi::api::apps::v1::Deployment {
        metadata: object_meta(name, namespace, labels, owners),
        spec: Some(k8s_openapi::api::apps::v1::DeploymentSpec {
            replicas: Some(replicas),
            selector: k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector { match_labels: Some(identity_selector(labels)), match_expressions: None },
            strategy: Some(k8s_openapi::api::apps::v1::DeploymentStrategy { type_: Some("Recreate".to_string()), rolling_update: None }),
            template,
            ..Default::default()
        }),
        status: None,
    }
}

fn build_service(
    name: &str,
    namespace: &str,
    spec: &crate::api::ForkSetSpec,
    labels: &std::collections::BTreeMap<String, String>,
    owners: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference],
) -> k8s_openapi::api::core::v1::Service {
    let mut service_spec: k8s_openapi::api::core::v1::ServiceSpec = spec.service_spec.clone();
    service_spec.selector = Some(identity_selector(labels));
    k8s_openapi::api::core::v1::Service { metadata: object_meta(name, namespace, labels, owners), spec: Some(service_spec), status: None }
}

fn build_httproute(
    name: &str,
    namespace: &str,
    spec: &crate::api::ForkSetSpec,
    labels: &std::collections::BTreeMap<String, String>,
    owners: &[k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference],
    branch: &str,
) -> serde_json::Value {
    let port = spec.service_spec.ports.as_ref().and_then(|ports| ports.first()).map(|port| port.port);
    serde_json::json!({
        "apiVersion": ROUTE_API_VERSION,
        "kind": ROUTE_KIND,
        "metadata": {
            "name": name,
            "namespace": namespace,
            "labels": labels,
            "ownerReferences": owners,
        },
        "spec": branch_route_spec(&spec.http_route_spec, name, port, &spec.source, branch),
    })
}

fn branch_route_spec(route_spec: &serde_json::Value, backend_name: &str, port: Option<i32>, source: &str, branch: &str) -> serde_json::Value {
    let header = branch_header(source, branch);
    let backend = backend_ref(backend_name, port);
    let mut result = route_spec.clone();
    match result.get_mut("rules").and_then(serde_json::Value::as_array_mut) {
        Some(rules) if !rules.is_empty() => rules.iter_mut().for_each(|rule| {
            if !rule.is_object() {
                *rule = serde_json::json!({});
            }
            rule["backendRefs"] = serde_json::json!([backend.clone()]);
            rule["matches"] = inject_branch_match(rule.get("matches"), &header);
        }),
        _ => {
            result["rules"] = serde_json::json!([{
                "matches": inject_branch_match(None, &header),
                "backendRefs": [backend],
            }]);
        }
    }
    result
}

fn inject_branch_match(matches: Option<&serde_json::Value>, header: &serde_json::Value) -> serde_json::Value {
    match matches.and_then(serde_json::Value::as_array).filter(|entries| !entries.is_empty()) {
        Some(entries) => serde_json::Value::Array(
            entries
                .iter()
                .map(|entry| {
                    let mut entry = if entry.is_object() { entry.clone() } else { serde_json::json!({}) };
                    entry["headers"] = inject_headers(entry.get("headers"), header);
                    entry
                })
                .collect(),
        ),
        None => serde_json::json!([{ "headers": [header.clone()] }]),
    }
}

fn inject_headers(headers: Option<&serde_json::Value>, header: &serde_json::Value) -> serde_json::Value {
    let kept = headers
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| !item.get("name").and_then(serde_json::Value::as_str).is_some_and(|name| name.eq_ignore_ascii_case(crate::api::HEADER)))
                .cloned()
                .collect::<Vec<serde_json::Value>>()
        })
        .unwrap_or_default()
        .into_iter()
        .chain([header.clone()])
        .collect();
    serde_json::Value::Array(kept)
}

fn branch_header(source: &str, branch: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "RegularExpression",
        "name": crate::api::HEADER,
        "value": format!("\"{}\":\"{}\"", escape_regex(source), escape_regex(branch)),
    })
}

fn backend_ref(name: &str, port: Option<i32>) -> serde_json::Value {
    match port {
        Some(port) => serde_json::json!({ "name": name, "port": port }),
        None => serde_json::json!({ "name": name }),
    }
}

fn upsert_env(container: &mut k8s_openapi::api::core::v1::Container, name: &str, value: &str) {
    let env = container.env.get_or_insert_with(Vec::new);
    match env.iter_mut().find(|entry| entry.name == name) {
        Some(entry) => {
            entry.value = Some(value.to_string());
            entry.value_from = None;
        }
        None => env.push(k8s_openapi::api::core::v1::EnvVar { name: name.to_string(), value: Some(value.to_string()), value_from: None }),
    }
}

pub fn route_resource() -> kube::api::ApiResource {
    kube::api::ApiResource::from_gvk(&kube::api::GroupVersionKind::gvk("gateway.networking.k8s.io", "v1", "HTTPRoute"))
}

pub async fn reconcile(forkset: std::sync::Arc<crate::api::ForkSet>, ctx: std::sync::Arc<Ctx>) -> Result<kube::runtime::controller::Action, kube::Error> {
    use futures::StreamExt;
    use futures::TryStreamExt;
    use kube::ResourceExt;
    let namespace = forkset.namespace().unwrap_or_default();
    let name = forkset.name_any();
    if forkset.metadata.deletion_timestamp.is_some() {
        delete_children(&ctx, &namespace, &forkset.spec.source, &forkset.spec.branches).await;
        return Ok(kube::runtime::controller::Action::await_change());
    }
    let forksets = kube::Api::<crate::api::ForkSet>::namespaced(ctx.client.clone(), &namespace);
    if name != forkset.spec.source {
        let condition = crate::api::Condition {
            type_: crate::api::CONDITION_IMMUTABLE.to_string(),
            status: "False".to_string(),
            reason: "NameSourceMismatch".to_string(),
            message: format!("metadata.name {name:?} does not match spec.source {:?}", forkset.spec.source),
        };
        let status = broken_status(&forkset, vec![condition]);
        if set_status(&forksets, &forkset, &status).await? {
            record(
                &ctx,
                &forkset,
                kube::runtime::events::EventType::Warning,
                "NameSourceMismatch",
                format!("{name} != {}", forkset.spec.source),
            )
            .await;
        }
        return Ok(kube::runtime::controller::Action::requeue(REQUEUE));
    }
    let deployments = kube::Api::<k8s_openapi::api::apps::v1::Deployment>::namespaced(ctx.client.clone(), &namespace);
    let services = kube::Api::<k8s_openapi::api::core::v1::Service>::namespaced(ctx.client.clone(), &namespace);
    let routes = routes_api(&ctx.client, &namespace);
    let pods = kube::Api::<k8s_openapi::api::core::v1::Pod>::namespaced(ctx.client.clone(), &namespace);
    let desired = plan(&forkset);
    let params = kube::api::PatchParams::apply(crate::api::FIELD_MANAGER).force();
    futures::stream::iter(&desired.branches)
        .then(|entry| async {
            let child = child_name(&forkset.spec.source, &entry.branch);
            deployments.patch(&child, &params, &kube::api::Patch::Apply(&entry.deployment)).await?;
            services.patch(&child, &params, &kube::api::Patch::Apply(&entry.service)).await?;
            routes.patch(&child, &params, &kube::api::Patch::Apply(&entry.httproute)).await?;
            Ok::<(), kube::Error>(())
        })
        .try_collect::<Vec<()>>()
        .await?;
    let uid = forkset.uid().unwrap_or_default();
    let selector = format!("{}={}", crate::api::LABEL_SOURCE, forkset.spec.source);
    let wanted = wanted_branches(&desired);
    reap_orphans(&deployments, &owned_children(&deployments, &selector).await, &uid, &wanted).await;
    reap_orphans(&services, &owned_children(&services, &selector).await, &uid, &wanted).await;
    reap_orphans(&routes, &owned_children(&routes, &selector).await, &uid, &wanted).await;
    let branches = futures::stream::iter(&forkset.spec.branches)
        .then(|entry| async {
            let child = child_name(&forkset.spec.source, &entry.branch);
            let condition = desired.skipped.iter().find(|(branch, _)| branch == &entry.branch).map(|(_, condition)| condition.clone());
            let (deployment, route, pod_list) = if condition.is_some() {
                (None, None, Vec::new())
            } else {
                (
                    deployments.get_opt(&child).await?,
                    routes.get_opt(&child).await?,
                    pods.list(&kube::api::ListParams::default().labels(&pod_selector(&forkset.spec.source, &entry.branch))).await?.items,
                )
            };
            Ok::<crate::api::BranchStatus, kube::Error>(branch_status(entry, &child, deployment.as_ref(), route.as_ref(), &pod_list, condition))
        })
        .try_collect::<Vec<crate::api::BranchStatus>>()
        .await?;
    let status = aggregate(&forkset, branches);
    if set_status(&forksets, &forkset, &status).await? {
        let condition = status.conditions.first().cloned().unwrap_or_default();
        let type_ = if condition.status == "True" {
            kube::runtime::events::EventType::Normal
        } else {
            kube::runtime::events::EventType::Warning
        };
        record(&ctx, &forkset, type_, &condition.reason, format!("{} branches", forkset.spec.branches.len())).await;
    }
    Ok(kube::runtime::controller::Action::requeue(REQUEUE))
}

pub fn error_policy(forkset: std::sync::Arc<crate::api::ForkSet>, error: &kube::Error, _ctx: std::sync::Arc<Ctx>) -> kube::runtime::controller::Action {
    use kube::ResourceExt;
    if matches!(error, kube::Error::Api(status) if status.is_not_found()) {
        return kube::runtime::controller::Action::await_change();
    }
    tracing::warn!(%error, forkset = %forkset.name_any(), "reconcile failed");
    kube::runtime::controller::Action::requeue(RETRY)
}

fn wanted_branches(desired: &Plan) -> std::collections::BTreeSet<&str> {
    desired
        .branches
        .iter()
        .map(|entry| entry.branch.as_str())
        .chain(desired.skipped.iter().map(|(branch, _)| branch.as_str()))
        .collect()
}

fn broken_status(forkset: &crate::api::ForkSet, conditions: Vec<crate::api::Condition>) -> crate::api::ForkSetStatus {
    let branches = forkset
        .spec
        .branches
        .iter()
        .map(|entry| branch_status(entry, &child_name(&forkset.spec.source, &entry.branch), None, None, &[], None))
        .collect();
    crate::api::ForkSetStatus { conditions, branches, observed_generation: forkset.metadata.generation.unwrap_or_default() }
}

fn branch_status(
    entry: &crate::api::BranchSpec,
    child: &str,
    deployment: Option<&k8s_openapi::api::apps::v1::Deployment>,
    route: Option<&kube::api::DynamicObject>,
    pods: &[k8s_openapi::api::core::v1::Pod],
    condition: Option<crate::api::Condition>,
) -> crate::api::BranchStatus {
    let desired = entry.replicas.unwrap_or(1);
    let ready = deployment.and_then(|deployment| deployment.status.as_ref()).and_then(|status| status.ready_replicas).unwrap_or(0);
    let version = image_tag(&entry.image).unwrap_or_default().to_string();
    let phase = if condition.as_ref().is_some_and(|condition| condition.status == "False") {
        crate::api::Phase::Failed
    } else if desired > 0 && ready >= desired {
        crate::api::Phase::Ready
    } else {
        crate::api::Phase::Pending
    };
    let mut pod_list = pods.iter().map(pod_status).collect::<Vec<crate::api::PodStatus>>();
    pod_list.sort_by(|left, right| left.name.cmp(&right.name));
    crate::api::BranchStatus {
        branch: entry.branch.clone(),
        phase,
        deployment: child.to_string(),
        service: child.to_string(),
        http_route: child.to_string(),
        version,
        replicas: crate::api::ReplicaStatus { desired, ready },
        route: crate::api::RouteStatus {
            name: child.to_string(),
            parent: route_parent_name(route),
            rule_index: 0,
            attached: route.is_some_and(route_accepted),
        },
        pods: pod_list,
        conditions: condition.into_iter().collect(),
    }
}

fn aggregate(forkset: &crate::api::ForkSet, branches: Vec<crate::api::BranchStatus>) -> crate::api::ForkSetStatus {
    let false_reason = branches
        .iter()
        .flat_map(|branch| branch.conditions.iter())
        .find(|condition| condition.status == "False")
        .map(|condition| condition.reason.clone());
    let condition = if branches.iter().all(|branch| branch.phase == crate::api::Phase::Ready) {
        crate::api::Condition {
            type_: "Ready".to_string(),
            status: "True".to_string(),
            reason: "AllBranchesReady".to_string(),
            message: format!("all {} branches ready", branches.len()),
        }
    } else {
        crate::api::Condition {
            type_: "Ready".to_string(),
            status: "False".to_string(),
            reason: false_reason.unwrap_or_else(|| "BranchesNotReady".to_string()),
            message: "not all branches ready".to_string(),
        }
    };
    crate::api::ForkSetStatus {
        conditions: vec![condition],
        branches,
        observed_generation: forkset.metadata.generation.unwrap_or_default(),
    }
}

fn route_parent_name(route: Option<&kube::api::DynamicObject>) -> String {
    route
        .and_then(|route| route.data.pointer("/spec/parentRefs/0/name"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn route_accepted(route: &kube::api::DynamicObject) -> bool {
    route.data.pointer("/status/parents").and_then(serde_json::Value::as_array).is_some_and(|parents| {
        parents.iter().any(|parent| {
            parent.pointer("/conditions").and_then(serde_json::Value::as_array).is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|condition| condition.get("type").and_then(serde_json::Value::as_str) == Some("Accepted") && condition.get("status").and_then(serde_json::Value::as_str) == Some("True"))
            })
        })
    })
}

async fn delete_children(ctx: &Ctx, namespace: &str, source: &str, branches: &[crate::api::BranchSpec]) {
    use futures::StreamExt;
    let params = kube::api::DeleteParams::default();
    futures::stream::iter(branches)
        .for_each(|entry| async {
            let name = child_name(source, &entry.branch);
            let results = [
                kube::Api::<k8s_openapi::api::apps::v1::Deployment>::namespaced(ctx.client.clone(), namespace)
                    .delete(&name, &params)
                    .await
                    .map(|_| ()),
                kube::Api::<k8s_openapi::api::core::v1::Service>::namespaced(ctx.client.clone(), namespace)
                    .delete(&name, &params)
                    .await
                    .map(|_| ()),
                routes_api(&ctx.client, namespace).delete(&name, &params).await.map(|_| ()),
            ];
            results.into_iter().for_each(|result| match result {
                Ok(()) => {}
                Err(kube::Error::Api(status)) if status.is_not_found() => {}
                Err(error) => tracing::warn!(%error, %name, "deleting child failed"),
            });
        })
        .await;
}

fn routes_api(client: &kube::Client, namespace: &str) -> kube::Api<kube::api::DynamicObject> {
    kube::Api::namespaced_with(client.clone(), namespace, &route_resource())
}

fn pod_status(pod: &k8s_openapi::api::core::v1::Pod) -> crate::api::PodStatus {
    use kube::ResourceExt;
    let conditions = pod.status.as_ref().and_then(|status| status.conditions.as_ref());
    let ready = conditions.is_some_and(|conditions| conditions.iter().any(|condition| condition.type_ == "Ready" && condition.status == "True"));
    crate::api::PodStatus {
        name: pod.name_any(),
        ready,
        phase: pod.status.as_ref().and_then(|status| status.phase.clone()).unwrap_or_default(),
    }
}

fn pod_selector(source: &str, branch: &str) -> String {
    format!("{}={},{}={}", crate::api::LABEL_SOURCE, source, crate::api::LABEL_BRANCH, branch)
}

async fn set_status(api: &kube::Api<crate::api::ForkSet>, forkset: &crate::api::ForkSet, status: &crate::api::ForkSetStatus) -> Result<bool, kube::Error> {
    use kube::ResourceExt;
    if forkset.status.as_ref() == Some(status) {
        return Ok(false);
    }
    api.patch_status(
        &forkset.name_any(),
        &kube::api::PatchParams::default(),
        &kube::api::Patch::Merge(serde_json::json!({ "status": status })),
    )
    .await?;
    Ok(true)
}

async fn record(ctx: &Ctx, forkset: &crate::api::ForkSet, type_: kube::runtime::events::EventType, reason: &str, note: String) {
    use kube::Resource;
    let event = kube::runtime::events::Event {
        type_,
        reason: reason.to_string(),
        note: Some(note),
        action: "Reconcile".to_string(),
        secondary: None,
    };
    if let Err(error) = ctx.recorder.publish(&event, &forkset.object_ref(&())).await {
        tracing::warn!(%error, "publishing event failed");
    }
}

async fn owned_children<K: Clone + std::fmt::Debug + serde::de::DeserializeOwned>(api: &kube::Api<K>, selector: &str) -> Vec<K> {
    api.list(&kube::api::ListParams::default().labels(selector)).await.map(|list| list.items).unwrap_or_else(|error| {
        tracing::warn!(%error, "listing children failed");
        Vec::new()
    })
}

async fn reap_orphans<K: kube::Resource + kube::ResourceExt + Clone + std::fmt::Debug + serde::de::DeserializeOwned>(
    api: &kube::Api<K>,
    items: &[K],
    uid: &str,
    wanted: &std::collections::BTreeSet<&str>,
) {
    use futures::StreamExt;
    futures::stream::iter(orphan_names(items, uid, wanted))
        .for_each(|name| async move {
            match api.delete(&name, &kube::api::DeleteParams::default()).await {
                Ok(_) => {}
                Err(kube::Error::Api(status)) if status.is_not_found() => {}
                Err(error) => tracing::warn!(%error, %name, "deleting orphan failed"),
            }
        })
        .await;
}
