pub const FIELD_MANAGER: &str = "fork-mesh";
pub const LABEL_SOURCE: &str = "fork-mesh.io/source";
pub const LABEL_BRANCH: &str = "fork-mesh.io/branch";
pub const LABEL_VERSION: &str = "fork-mesh.io/version";
pub const HEADER: &str = "x-fork-mesh";
pub const CONDITION_IMMUTABLE: &str = "ImmutableIdentity";
pub const CONDITION_SOURCE_CONTAINER: &str = "SourceContainer";

#[derive(kube::CustomResource, kube::KubeSchema, serde::Serialize, serde::Deserialize, Clone, Debug)]
#[kube(doc = "", group = "fork-mesh.io", version = "v1alpha1", kind = "ForkSet", namespaced, status = "ForkSetStatus")]
#[serde(rename_all = "camelCase")]
pub struct ForkSetSpec {
    #[x_kube(validation = "self == oldSelf")]
    pub source: String,
    pub pod_spec: k8s_openapi::api::core::v1::PodSpec,
    pub service_spec: k8s_openapi::api::core::v1::ServiceSpec,
    #[schemars(extend("x-kubernetes-preserve-unknown-fields" = true))]
    pub http_route_spec: serde_json::Value,
    #[schemars(extend("x-kubernetes-list-type" = "map", "x-kubernetes-list-map-keys" = serde_json::json!(["branch"])))]
    pub branches: Vec<BranchSpec>,
}

#[derive(Clone, Debug, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BranchSpec {
    #[schemars(extend("pattern" = "^[a-z0-9]([-a-z0-9]*[a-z0-9])?$", "maxLength" = 63))]
    pub branch: String,
    #[schemars(extend("minLength" = 1))]
    pub image: String,
    pub replicas: Option<i32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, schemars::JsonSchema, serde::Deserialize, serde::Serialize)]
pub enum Phase {
    #[default]
    Pending,
    Ready,
    Failed,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaStatus {
    pub desired: i32,
    pub ready: i32,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PodStatus {
    pub name: String,
    pub ready: bool,
    pub phase: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RouteStatus {
    pub name: String,
    pub parent: String,
    pub rule_index: i32,
    pub attached: bool,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq, schemars::JsonSchema)]
pub struct Condition {
    #[serde(rename = "type")]
    pub type_: String,
    pub status: String,
    pub reason: String,
    pub message: String,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct BranchStatus {
    pub branch: String,
    pub phase: Phase,
    pub deployment: String,
    pub service: String,
    pub http_route: String,
    pub version: String,
    pub replicas: ReplicaStatus,
    pub route: RouteStatus,
    pub pods: Vec<PodStatus>,
    pub conditions: Vec<Condition>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq, Eq, schemars::JsonSchema)]
#[serde(rename_all = "camelCase", default)]
pub struct ForkSetStatus {
    pub conditions: Vec<Condition>,
    pub branches: Vec<BranchStatus>,
    pub observed_generation: i64,
}

pub fn bare_crd() -> serde_json::Value {
    fn strip_descriptions(value: &mut serde_json::Value) {
        match value {
            serde_json::Value::Object(entries) => {
                entries.remove("description");
                entries.values_mut().for_each(strip_descriptions);
            }
            serde_json::Value::Array(items) => items.iter_mut().for_each(strip_descriptions),
            _ => {}
        }
    }
    use kube::CustomResourceExt;
    let mut value = serde_json::to_value(ForkSet::crd()).expect("CRD serializes");
    strip_descriptions(&mut value);
    value
}
