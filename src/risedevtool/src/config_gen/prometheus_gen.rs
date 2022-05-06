// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use itertools::Itertools;

use crate::PrometheusConfig;

pub struct PrometheusGen;

impl PrometheusGen {
    pub fn gen_prometheus_yml(&self, config: &PrometheusConfig) -> String {
        let prometheus_host = &config.address;
        let prometheus_port = &config.port;
        let compute_node_targets = config
            .provide_compute_node
            .as_ref()
            .unwrap()
            .iter()
            .map(|node| format!("\"{}:{}\"", node.address, node.exporter_port))
            .join(",");

        let meta_node_targets = config
            .provide_meta_node
            .as_ref()
            .unwrap()
            .iter()
            .map(|node| format!("\"{}:{}\"", node.address, node.exporter_port))
            .join(",");

        let minio_targets = config
            .provide_minio
            .as_ref()
            .unwrap()
            .iter()
            .map(|node| format!("\"{}:{}\"", node.address, node.port))
            .join(",");

        let compactor_targets = config
            .provide_compactor
            .as_ref()
            .unwrap()
            .iter()
            .map(|node| format!("\"{}:{}\"", node.address, node.exporter_port))
            .join(",");

        format!(
            r#"# --- THIS FILE IS AUTO GENERATED BY RISEDEV ---
global:
  scrape_interval: 1s
  evaluation_interval: 5s

scrape_configs:
  - job_name: "prometheus"
    static_configs:
      - targets: ["{prometheus_host}:{prometheus_port}"]

  - job_name: compute-job
    static_configs:
      - targets: [{compute_node_targets}]

  - job_name: meta-job
    static_configs:
      - targets: [{meta_node_targets}]
  
  - job_name: minio-job
    metrics_path: /minio/v2/metrics/cluster
    static_configs:
    - targets: [{minio_targets}]

  - job_name: compactor-job
    static_configs:
      - targets: [{compactor_targets}]
"#,
        )
    }
}
