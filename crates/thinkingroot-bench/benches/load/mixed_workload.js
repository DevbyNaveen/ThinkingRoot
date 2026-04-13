import http from 'k6/http';
import { check, sleep } from 'k6';
import { Trend, Rate } from 'k6/metrics';

const mixedLatency = new Trend('mixed_latency', true);
const mixedFailRate = new Rate('mixed_fail_rate');

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:9876';
const WS = __ENV.WORKSPACE || 'bench-workspace';

const SEARCH_QUERIES = [
  'authentication',
  'database',
  'cache',
  'handler',
  'middleware',
  'configuration',
  'deployment',
  'migration',
];

export const options = {
  stages: [
    { duration: '30s', target: 50 },
    { duration: '120s', target: 50 },
    { duration: '30s', target: 0 },
  ],
  thresholds: {
    'mixed_latency': ['p(95)<15', 'p(99)<30'],
    'mixed_fail_rate': ['rate<0.001'],
    'http_reqs': ['rate>500'],
  },
};

export default function () {
  const roll = Math.random();
  let url;

  if (roll < 0.50) {
    // 50% — search
    const q = SEARCH_QUERIES[Math.floor(Math.random() * SEARCH_QUERIES.length)];
    url = `${BASE_URL}/api/v1/ws/${WS}/search?q=${encodeURIComponent(q)}&top_k=10`;
  } else if (roll < 0.70) {
    // 20% — entities
    url = `${BASE_URL}/api/v1/ws/${WS}/entities`;
  } else if (roll < 0.85) {
    // 15% — claims
    url = `${BASE_URL}/api/v1/ws/${WS}/claims?type=Fact&limit=50`;
  } else if (roll < 0.95) {
    // 10% — relations
    url = `${BASE_URL}/api/v1/ws/${WS}/relations`;
  } else {
    // 5% — health
    url = `${BASE_URL}/api/v1/ws/${WS}/health`;
  }

  const start = Date.now();
  const res = http.get(url);
  const elapsed = Date.now() - start;

  mixedLatency.add(elapsed);

  const success = check(res, {
    'status is 200': (r) => r.status === 200,
    'ok is true': (r) => {
      try {
        return JSON.parse(r.body).ok === true;
      } catch (_) {
        return false;
      }
    },
  });

  mixedFailRate.add(!success);

  sleep(0.05);
}
