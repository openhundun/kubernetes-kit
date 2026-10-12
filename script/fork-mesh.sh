#!/usr/bin/env bash
set -euo pipefail

readonly ARG_JOB="${1:-}"
readonly ARG_STEP="${2:-}"

readonly GIT_REPOSITORY_URL="https://github.com/openhundun/kubernetes-kit"
readonly GIT_REPOSITORY_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

function help() {
    echo "USAGE: ${0} <job__check | job__image | job__chart | job__e2e> [step__xxx]"
}

function job__check() {
    function step__fact() {
        local app_name="$(yq -p toml -o yaml '.package.name' source/fork-mesh/Cargo.toml)"
        local app_version="$(yq -p toml -o toml '.package.version' source/fork-mesh/Cargo.toml)"
        local image_repository="$(yq -p yaml -o yaml '.image.repository' deploy/fork-mesh-chart/values.yaml)"
        local image_tag="$(yq -p yaml -o yaml '.image.tag' deploy/fork-mesh-chart/values.yaml)"
        local chart_version="$(yq -p yaml -o yaml '.version' deploy/fork-mesh-chart/Chart.yaml)"
        if [[ "${app_name}" != "$(basename "${image_repository}")" || "${app_version}" != "${image_tag}" || "${app_version}" != "${chart_version}" ]]; then
            echo "[check][fact]: FAIL" >&2
            return 1
        fi
    }

    function step__fmt() {
        cargo fmt --check
    }

    function step__clippy() {
        cargo clippy --workspace --all-targets -- -D warnings
    }

    function step__crd_diff() {
        local tmp_dir="${GIT_REPOSITORY_DIR}/out/check"
        rm -rf "${tmp_dir}"
        mkdir -p "${tmp_dir}"
        cargo run -q -p fork-mesh -- crd | yq -P '.metadata.annotations."helm.sh/resource-policy" = "keep"' > "${tmp_dir}/crd.yaml"
        diff -u deploy/fork-mesh-chart/templates/crds.yaml "${tmp_dir}/crd.yaml"
        rm -rf "${tmp_dir}"
    }

    function step__helm_lint() {
        helm lint deploy/fork-mesh-chart
        helm template fork-mesh deploy/fork-mesh-chart > /dev/null
    }

    function all() {
        step__fact
        step__fmt
        step__clippy
        step__crd_diff
        step__helm_lint
    }

    case "${ARG_STEP}" in
    step__fact) step__fact ;;
    step__fmt) step__fmt ;;
    step__clippy) step__clippy ;;
    step__crd_diff) step__crd_diff ;;
    step__helm_lint) step__helm_lint ;;
    *) all ;;
    esac
}

function job__image() {
    readonly IMAGE_REGISTRY="${IMAGE_REGISTRY:-ghcr.io}"
    readonly IMAGE_REPOSITORY_OWNER="${IMAGE_REPOSITORY_OWNER:-openhundun}"

    function step__build() {
        local image_repository="$(yq -p yaml -o yaml '.image.repository' deploy/fork-mesh-chart/values.yaml)"
        local image_tag="$(yq -p toml -o toml '.package.version' source/fork-mesh/Cargo.toml)"
        docker buildx create --name openhundun --driver docker-container --use > /dev/null 2>&1 || docker buildx use openhundun
        docker buildx build --file deploy/fork-mesh-image/Dockerfile --tag "${IMAGE_REGISTRY}/${IMAGE_REPOSITORY_OWNER}/$(basename "${image_repository}"):${image_tag}" --build-arg IMAGE_REGISTRY="${IMAGE_REGISTRY}" --platform linux/amd64,linux/arm64 --provenance false --sbom false --annotation "index:org.opencontainers.image.source=${GIT_REPOSITORY_URL}" --push "${GIT_REPOSITORY_DIR}"
    }

    function all() {
        step__build
    }

    case "${ARG_STEP}" in
    step__build) step__build ;;
    *) all ;;
    esac
}

function job__chart() {
    readonly CHART_REGISTRY="${CHART_REGISTRY:-ghcr.io}"
    readonly CHART_REPOSITORY_OWNER="${CHART_REPOSITORY_OWNER:-openhundun}"
    readonly TMP_DIR="${GIT_REPOSITORY_DIR}/out/chart"

    function step__package() {
        rm -rf "${TMP_DIR}"
        mkdir -p "${TMP_DIR}"
        helm package deploy/fork-mesh-chart --destination "${TMP_DIR}"
    }

    function step__push() {
        local chart_name="$(yq -p yaml -o yaml '.name' deploy/fork-mesh-chart/Chart.yaml)"
        local chart_version="$(yq -p yaml -o yaml '.version' deploy/fork-mesh-chart/Chart.yaml)"
        helm push "${TMP_DIR}/${chart_name}-${chart_version}.tgz" "oci://${CHART_REGISTRY}/${CHART_REPOSITORY_OWNER}/charts"
        rm -rf "${TMP_DIR}"
    }

    function all() {
        step__package
        step__push
    }

    case "${ARG_STEP}" in
    step__package) step__package ;;
    step__push) step__push ;;
    *) all ;;
    esac
}

function job__e2e() {
    readonly E2E_NAMESPACE="fork-mesh-e2e"
    readonly SOURCE_NAME="busybox-sleep"
    readonly BUSYBOX_IMAGE="docker.io/library/busybox:stable"

    function scenario() {
        local branches="${1}"
        kubectl delete namespace "${E2E_NAMESPACE}" --ignore-not-found > /dev/null
        kubectl create namespace "${E2E_NAMESPACE}" > /dev/null
        kubectl -n "${E2E_NAMESPACE}" apply -f - > /dev/null << EOF
apiVersion: fork-mesh.io/v1alpha1
kind: ForkSet
metadata: { name: ${SOURCE_NAME} }
spec:
  source: ${SOURCE_NAME}
  podSpec:
    containers:
      - name: ${SOURCE_NAME}
        image: ${BUSYBOX_IMAGE}
        command: ["sh", "-c", "sleep 3600"]
  serviceSpec:
    ports:
      - { port: 8080 }
  httpRouteSpec:
    parentRefs:
      - { name: fork-mesh }
    hostnames: [${SOURCE_NAME}.example.com]
  branches:
${branches}
EOF
    }

    function branches_two() {
        printf '    - { branch: alpha, image: %s }\n    - { branch: beta, image: %s }\n' "${BUSYBOX_IMAGE}" "${BUSYBOX_IMAGE}"
    }

    function branches_one() {
        printf '    - { branch: alpha, image: %s }\n' "${BUSYBOX_IMAGE}"
    }

    function wait_ready() {
        kubectl -n "${E2E_NAMESPACE}" wait deploy/"${SOURCE_NAME}-alpha" deploy/"${SOURCE_NAME}-beta" --for=condition=Available --timeout=120s > /dev/null
        kubectl -n "${E2E_NAMESPACE}" wait service/"${SOURCE_NAME}-alpha" service/"${SOURCE_NAME}-beta" httproute/"${SOURCE_NAME}-alpha" httproute/"${SOURCE_NAME}-beta" --for=create --timeout=120s > /dev/null
        kubectl -n "${E2E_NAMESPACE}" wait forkset/"${SOURCE_NAME}" --for=jsonpath='{.status.branches[0].phase}'=Ready --for=jsonpath='{.status.branches[1].phase}'=Ready --timeout=120s > /dev/null
    }

    function child_gone() {
        local kind="${1}"
        local name="${2}"
        [[ "$(kubectl -n "${E2E_NAMESPACE}" get "${kind}/${name}" -o name 2> /dev/null)" == "" ]]
    }

    function step__install() {
        kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.4.1/experimental-install.yaml > /dev/null
        helm upgrade --install fork-mesh deploy/fork-mesh-chart --namespace fork-mesh-system --create-namespace
        kubectl -n fork-mesh-system rollout status deploy/fork-mesh --timeout=120s
    }

    function step__test__branch_ready() {
        scenario "$(branches_two)"
        local found=""
        for _ in {1..120}; do
            if wait_ready > /dev/null 2>&1; then
                found="1"
                break
            fi
            sleep 1
        done
        if [[ -z "${found}" ]]; then
            echo "[e2e][branch_ready]: FAIL" >&2
            return 1
        fi
        if [[ "$(kubectl -n "${E2E_NAMESPACE}" get forkset/"${SOURCE_NAME}" -o jsonpath='{.status.branches[*].phase}')" != "Ready Ready" || "$(kubectl -n "${E2E_NAMESPACE}" get forkset/"${SOURCE_NAME}" -o jsonpath='{.status.branches[*].deployment}')" != "${SOURCE_NAME}-alpha ${SOURCE_NAME}-beta" ]]; then
            echo "[e2e][branch_ready]: FAIL" >&2
            return 1
        fi
        kubectl delete namespace "${E2E_NAMESPACE}" --ignore-not-found --wait=false > /dev/null
    }

    function step__test__branch_deleted() {
        scenario "$(branches_two)"
        local found=""
        for _ in {1..120}; do
            if wait_ready > /dev/null 2>&1; then
                found="1"
                break
            fi
            sleep 1
        done
        if [[ -z "${found}" ]]; then
            echo "[e2e][branch_deleted]: FAIL" >&2
            return 1
        fi
        scenario "$(branches_one)"
        found=""
        for _ in {1..120}; do
            if child_gone deploy "${SOURCE_NAME}-beta" && child_gone service "${SOURCE_NAME}-beta" && child_gone httproute "${SOURCE_NAME}-beta"; then
                found="1"
                break
            fi
            sleep 1
        done
        if [[ -z "${found}" ]]; then
            echo "[e2e][branch_deleted]: FAIL" >&2
            return 1
        fi
        found=""
        for _ in {1..120}; do
            if [[ "$(kubectl -n "${E2E_NAMESPACE}" get deploy/"${SOURCE_NAME}-alpha" -o jsonpath='{.status.readyReplicas}' 2> /dev/null)" == "1" && "$(kubectl -n "${E2E_NAMESPACE}" get forkset/"${SOURCE_NAME}" -o jsonpath='{.status.branches[*].branch}')" == "alpha" ]]; then
                found="1"
                break
            fi
            sleep 1
        done
        if [[ -z "${found}" ]]; then
            echo "[e2e][branch_deleted]: FAIL" >&2
            return 1
        fi
        kubectl delete namespace "${E2E_NAMESPACE}" --ignore-not-found --wait=false > /dev/null
    }

    function step__test__forkset_deleted() {
        scenario "$(branches_two)"
        local found=""
        for _ in {1..120}; do
            if wait_ready > /dev/null 2>&1; then
                found="1"
                break
            fi
            sleep 1
        done
        if [[ -z "${found}" ]]; then
            echo "[e2e][forkset_deleted]: FAIL" >&2
            return 1
        fi
        kubectl -n "${E2E_NAMESPACE}" delete forkset "${SOURCE_NAME}" > /dev/null
        found=""
        for _ in {1..120}; do
            if child_gone deploy "${SOURCE_NAME}-alpha" && child_gone service "${SOURCE_NAME}-alpha" && child_gone httproute "${SOURCE_NAME}-alpha" && child_gone deploy "${SOURCE_NAME}-beta" && child_gone service "${SOURCE_NAME}-beta" && child_gone httproute "${SOURCE_NAME}-beta"; then
                found="1"
                break
            fi
            sleep 1
        done
        if [[ -z "${found}" ]]; then
            echo "[e2e][forkset_deleted]: FAIL" >&2
            return 1
        fi
        kubectl delete namespace "${E2E_NAMESPACE}" --ignore-not-found --wait=false > /dev/null
    }

    function step__cleanup() {
        kubectl -n "${E2E_NAMESPACE}" delete forkset --all --ignore-not-found || true
        kubectl delete namespace "${E2E_NAMESPACE}" --ignore-not-found --wait=false || true
        helm uninstall fork-mesh --namespace fork-mesh-system || true
        kubectl delete namespace fork-mesh-system --ignore-not-found --wait=false || true
    }

    function all() {
        step__install
        step__test__branch_ready
        step__test__branch_deleted
        step__test__forkset_deleted
        step__cleanup
    }

    case "${ARG_STEP}" in
    step__install) step__install ;;
    step__cleanup) step__cleanup ;;
    step__test__branch_ready) step__test__branch_ready ;;
    step__test__branch_deleted) step__test__branch_deleted ;;
    step__test__forkset_deleted) step__test__forkset_deleted ;;
    *) all ;;
    esac
}

case "${ARG_JOB}" in
job__check) job__check ;;
job__image) job__image ;;
job__chart) job__chart ;;
job__e2e) job__e2e ;;
*) help ;;
esac
