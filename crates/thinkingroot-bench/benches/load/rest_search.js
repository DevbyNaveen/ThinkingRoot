import http from 'k6/http';
import { check, sleep } from 'k6';
import { Trend, Rate } from 'k6/metrics';

const searchLatency = new Trend('search_latency', true);
const searchFailRate = new Rate('search_fail_rate');

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:9876';
const WS = __ENV.WORKSPACE || 'bench-workspace';

const QUERIES = [
  'authentication',
  'database',
  'cache',
  'handler',
  'middleware',
  'configuration',
  'deployment',
  'migration',
  'service',
  'repository',
  'controller',
  'schema',
  'index',
  'session',
  'authorization',
];

export const options = {
  stages: [
    { duration: '30s', target: 50 },
    { duration: '90s', target: 50 },
    { duration: '30s', target: 200 },
    { duration: '30s', target: 0 },
  ],
  thresholds: {
    'search_latency': ['p(95)<10', 'p(99)<25'],
    'search_fail_rate': ['rate<0.001'],
    'http_reqs': ['rate>100'],
  },
};

export default function () {
  const query = QUERIES[Math.floor(Math.random() * QUERIES.length)];
  const url = `${BASE_URL}/api/v1/ws/${WS}/search?q=${encodeURIComponent(query)}&top_k=10`;

  const start = Date.now();
  const res = http.get(url);
  const elapsed = Date.now() - start;

  searchLatency.add(elapsed);

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

  searchFailRate.add(!success);

  sleep(0.1);
}
