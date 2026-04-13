import http from 'k6/http';
import { check, sleep } from 'k6';
import { Trend, Rate } from 'k6/metrics';

const entityLatency = new Trend('entity_latency', true);
const entityFailRate = new Rate('entity_fail_rate');

const BASE_URL = __ENV.BASE_URL || 'http://127.0.0.1:9876';
const WS = __ENV.WORKSPACE || 'bench-workspace';

export const options = {
  stages: [
    { duration: '20s', target: 50 },
    { duration: '60s', target: 50 },
    { duration: '20s', target: 0 },
  ],
  thresholds: {
    'entity_latency': ['p(95)<10', 'p(99)<25'],
    'entity_fail_rate': ['rate<0.001'],
  },
};

export default function () {
  const url = `${BASE_URL}/api/v1/ws/${WS}/entities`;

  const start = Date.now();
  const res = http.get(url);
  const elapsed = Date.now() - start;

  entityLatency.add(elapsed);

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

  entityFailRate.add(!success);

  sleep(0.1);
}
