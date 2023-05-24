// Copyright 2022 Zinc Labs Inc. and Contributors

//  Licensed under the Apache License, Version 2.0 (the "License");
//  you may not use this file except in compliance with the License.
//  You may obtain a copy of the License at

//      http:www.apache.org/licenses/LICENSE-2.0

//  Unless required by applicable law or agreed to in writing, software
//  distributed under the License is distributed on an "AS IS" BASIS,
//  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
//  See the License for the specific language governing permissions and
//  limitations under the License.

import config from "@/aws-exports";
import { routeGuardPendingSubscriptions } from "@/utils/zincutils";
import SysLog from "@/components/ingestion/logs/SysLog.vue";
import Ingestion from "@/views/Ingestion.vue";
import FluentBit from "@/components/ingestion/logs/FluentBit.vue";
import Fluentd from "@/components/ingestion/logs/Fluentd.vue";
import Vector from "@/components/ingestion/logs/Vector.vue";
import Curl from "@/components/ingestion/logs/Curl.vue";
import KinesisFirehose from "@/components/ingestion/logs/KinesisFirehose.vue";
import FileBeat from "@/components/ingestion/logs/FileBeat.vue";
import OpenTelemetry from "@/components/ingestion/traces/OpenTelemetry.vue";
import PrometheusConfig from "@/components/ingestion/metrics/PrometheusConfig.vue";
import OtelCollector from "@/components/ingestion/metrics/OtelCollector.vue";
import TelegrafConfig from "@/components/ingestion/metrics/TelegrafConfig.vue";
import IngestLogs from "@/components/ingestion/logs/Index.vue";
import IngestMetrics from "@/components/ingestion/metrics/Index.vue";
import IngestTraces from "@/components/ingestion/traces/Index.vue";

const useIngestionRoutes = () => {
  const ingestionRoutes: any = [
    {
      path: "ingestion",
      name: "ingestion",
      component: Ingestion,
      beforeEnter(to: any, from: any, next: any) {
        routeGuardPendingSubscriptions(to, from, next);
      },
      children: [
        {
          path: "logs",
          name: "ingestLogs",
          component: IngestLogs,
          children: [
            {
              path: "curl",
              name: "curl",
              component: Curl,
            },
            {
              path: "fluentbit",
              name: "fluentbit",
              component: FluentBit,
            },
            {
              path: "fluentd",
              name: "fluentd",
              component: Fluentd,
            },
            {
              path: "vector",
              name: "vector",
              component: Vector,
            },
            {
              path: "kinesisfirehose",
              name: "kinesisfirehose",
              component: KinesisFirehose,
            },
            {
              path: "filebeat",
              name: "filebeat",
              component: FileBeat,
            },
          ],
        },
        {
          path: "metrics",
          name: "ingestMetrics",
          component: IngestMetrics,
          children: [
            {
              path: "prometheus",
              name: "prometheus",
              component: PrometheusConfig,
            },
            {
              path: "otelcollector",
              name: "otelCollector",
              component: OtelCollector,
            },
            {
              path: "telegraf",
              name: "telegraf",
              component: TelegrafConfig,
            },
          ],
        },
        {
          path: "traces",
          name: "ingestTraces",
          component: IngestTraces,
          children: [
            {
              path: "opentelemetry",
              name: "tracesOTLP",
              component: OpenTelemetry,
            },
          ],
        },
      ],
    },
  ];

  const sysLog = {
    path: "syslog",
    name: "syslog",
    component: SysLog,
  };

  if (config.isZincObserveCloud === "false" || !config.isZincObserveCloud) {
    ingestionRoutes[0].children
      .find((child: any) => child.name === "ingestLogs")
      .children.push(sysLog);
  }
  return ingestionRoutes;
};

export default useIngestionRoutes;
